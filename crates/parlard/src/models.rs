//! Model files and the libmoonshine runtime: where they live and how they are fetched.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// `$PARLAR_MODELS`, else `$XDG_DATA_HOME/parlar/models`, else `~/.local/share/parlar/models`.
pub fn root() -> PathBuf {
    if let Some(p) = std::env::var_os("PARLAR_MODELS") {
        return PathBuf::from(p);
    }
    data_home().join("parlar/models")
}

fn data_home() -> PathBuf {
    if let Some(p) = std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/share")
}

/// The G2P and vocoder root that libmoonshine resolves TTS asset keys against.
pub fn tts_dir() -> PathBuf {
    root().join("tts")
}

const TURN_FILE: &str = "smart-turn-v3.2-cpu.onnx";
const TURN_URL: &str = "https://huggingface.co/pipecat-ai/smart-turn-v3/resolve/main/smart-turn-v3.2-cpu.onnx";
const TURN_SIZE: u64 = 8_679_182;

const VAD_FILE: &str = "silero_vad.onnx";
const VAD_URL: &str = "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad.onnx";
const VAD_SIZE: u64 = 2_327_524;

pub fn vad_model() -> PathBuf {
    root().join("vad").join(VAD_FILE)
}

/// The speech recognizer: Phonon-2, or Parakeet TDT 0.6B v3 (the same layout) if only that is
/// installed.
pub fn final_dir() -> PathBuf {
    let stt = root().join("stt");
    ["phonon-2", "parakeet-tdt-0.6b-v3"]
        .iter()
        .map(|n| stt.join(n))
        .find(|d| d.join("vocab.txt").exists())
        .unwrap_or_else(|| stt.join("phonon-2"))
}

pub fn turn_model() -> PathBuf {
    root().join("turn").join(TURN_FILE)
}

const MOONSHINE: &str = "libmoonshine.so";
const ORT: &str = "libonnxruntime.so.1";

/// The pinned libmoonshine release per architecture, with the sha256 of its tarball.
const MOONSHINE_VERSION: &str = "v0.1.5";
const MOONSHINE_RELEASES: &[(&str, &str, &str)] = &[
    ("x86_64", "linux-x86_64", "9c3a87fea93ff2ad957938868f95a0a366dce9ff8ad86bde6cdcf5a4cadb51df"),
    ("aarch64", "linux-arm64", "1600c80a0806b7a2582307c98e7a56f4072e4b060498b08a0b75eb20af42def2"),
];

/// Where libmoonshine and its ONNX Runtime are: `$PARLAR_LIB_DIR`, next to this binary,
/// `../lib/parlar` from it (packages), else `$XDG_DATA_HOME/parlar/lib` (what `fetch` fills).
pub fn lib_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("PARLAR_LIB_DIR") {
        return PathBuf::from(p);
    }
    let fetched = data_home().join("parlar/lib");
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let dir = exe.as_deref().and_then(|p| p.parent()).map(PathBuf::from);
    dir.iter()
        .flat_map(|d| [d.clone(), d.join("../lib/parlar")])
        .chain([fetched.clone()])
        .find(|d| d.join(MOONSHINE).exists())
        .unwrap_or(fetched)
}

/// The ONNX Runtime that ships with libmoonshine, so every model shares one runtime.
pub fn ort_lib() -> PathBuf {
    match std::env::var_os("PARLAR_ORT_LIB") {
        Some(p) => PathBuf::from(p),
        None => lib_dir().join(ORT),
    }
}

/// Hand freed model memory back to the system. glibc keeps large freed heaps mapped, so without
/// this an unloaded model still shows in RSS.
pub fn release_memory() {
    #[cfg(target_env = "gnu")]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: no arguments that can be invalid; it only walks the allocator's own arenas
        unsafe { malloc_trim(0) };
    }
}

/// Load libmoonshine for the voice.
pub fn moonshine() -> Result<()> {
    let lib = lib_dir().join(MOONSHINE);
    if !lib.exists() {
        bail!("{} is missing; run `parlard fetch`", lib.display());
    }
    parlar_moonshine::open(&lib)
}

/// Download the pinned libmoonshine release into `$XDG_DATA_HOME/parlar/lib`, unless one is
/// already found.
fn fetch_moonshine() -> Result<()> {
    if lib_dir().join(MOONSHINE).exists() && lib_dir().join(ORT).exists() {
        return Ok(());
    }
    let arch = std::env::consts::ARCH;
    let Some((_, platform, sha)) = MOONSHINE_RELEASES.iter().find(|(a, _, _)| *a == arch) else {
        bail!("no prebuilt libmoonshine for {arch}; put {MOONSHINE} and {ORT} in a directory and set PARLAR_LIB_DIR");
    };
    let name = format!("moonshine-voice-{platform}");
    let dest = data_home().join("parlar/lib");
    let tmp = dest.join(".fetch");
    let _ = std::fs::remove_dir_all(&tmp);
    let tarball = File {
        sha256: Some((*sha).into()),
        size: None,
        ..File::new(
            &format!("{name}.tar.gz"),
            &format!("https://github.com/moonshine-ai/moonshine/releases/download/{MOONSHINE_VERSION}/{name}.tar.gz"),
            0,
        )
    };
    download(&Manifest { groups: vec![Group { files: vec![tarball] }] }, &tmp)?;
    let ok = Command::new("tar")
        .arg("xzf")
        .arg(tmp.join(format!("{name}.tar.gz")))
        .arg("-C")
        .arg(&tmp)
        .status()
        .context("run tar")?
        .success();
    if !ok {
        bail!("unpack {name}.tar.gz failed");
    }
    for lib in [ORT, MOONSHINE] {
        std::fs::rename(tmp.join(&name).join("lib").join(lib), dest.join(lib)).with_context(|| format!("install {lib}"))?;
    }
    std::fs::remove_dir_all(&tmp)?;
    Ok(())
}

#[derive(Deserialize)]
struct Manifest {
    groups: Vec<Group>,
}

#[derive(Deserialize)]
struct Group {
    files: Vec<File>,
}

#[derive(Deserialize)]
struct File {
    name: String,
    url: String,
    size: Option<u64>,
    /// Base64 of the big-endian CRC32C when `checksum_type` is "crc32c"; empty when unknown.
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    checksum_type: Option<String>,
    /// Hex sha256, for files we pin ourselves.
    #[serde(default)]
    sha256: Option<String>,
}

impl File {
    fn new(name: &str, url: &str, size: u64) -> File {
        File { name: name.into(), url: url.into(), size: Some(size), checksum: None, checksum_type: None, sha256: None }
    }

    /// The expected CRC32C, when the manifest gives one.
    fn crc32c(&self) -> Result<Option<u32>> {
        let (Some(sum), Some("crc32c")) = (self.checksum.as_deref(), self.checksum_type.as_deref()) else {
            return Ok(None);
        };
        if sum.is_empty() {
            return Ok(None);
        }
        let bytes = base64(sum).with_context(|| format!("{}: bad checksum {sum:?}", self.name))?;
        let b: [u8; 4] = bytes.try_into().map_err(|_| anyhow!("{}: checksum {sum:?} is not 4 bytes", self.name))?;
        Ok(Some(u32::from_be_bytes(b)))
    }
}

/// Standard base64 with padding, as the manifests carry it.
fn base64(s: &str) -> Result<Vec<u8>> {
    let val = |c: u8| -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("not base64: {:?}", c as char),
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        if c.len() == 1 {
            bail!("truncated base64");
        }
        let mut n = 0u32;
        for (i, &b) in c.iter().enumerate() {
            n |= val(b)? << (18 - 6 * i);
        }
        out.extend_from_slice(&n.to_be_bytes()[1..c.len()]);
    }
    Ok(out)
}

fn file_crc32c(path: &Path) -> Result<u32> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut crc = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(crc);
        }
        crc = crc32c::crc32c_append(crc, &buf[..n]);
    }
}

/// Phonon-2 in ONNX (tiyuvta/Phonon-2-ONNX), pinned to one revision: the int8 encoder, the fp32
/// decoder, its own preprocessor, vocab and config, each checked by sha256.
const PHONON_REPO: &str = "https://huggingface.co/tiyuvta/Phonon-2-ONNX/resolve/df0802202a996b5c0574acd805968940f62a6a04";
const PHONON_FILES: &[(&str, u64, &str)] = &[
    ("encoder-model.int8.onnx", 614_486_780, "3c100e38ca2e70623c928ab5c5414c62848603ea3d943a7f2e1c3ce1d92fc04b"),
    ("decoder_joint-model.onnx", 72_518_934, "420125e0e13596692320c35ef648eee9bf4583718c7896c8732ebf6f50b9ca0d"),
    ("preprocessor-model.onnx", 1_193_996, "8184d564f7d34d1daf04e4b35a0222fb72c54668b38dcd2b8ddda3517676614b"),
    ("vocab.txt", 93_939, "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d"),
    ("config.json", 121, "db59e29a3c1fde6a081bf04965e72bba26cd65be1aee65b064360df8aef468e5"),
];

pub fn fetch(voice: &str) -> Result<()> {
    fetch_moonshine()?;
    moonshine()?;
    let stt = root().join("stt/phonon-2");
    let files = PHONON_FILES
        .iter()
        .map(|(n, size, sha)| File { sha256: Some((*sha).into()), ..File::new(n, &format!("{PHONON_REPO}/{n}"), *size) })
        .collect();
    download(&Manifest { groups: vec![Group { files }] }, &stt)?;
    let tts = parlar_moonshine::tts_manifest("en", &[("voice", voice)])?;
    download(&serde_json::from_str(&tts)?, &tts_dir())?;
    let turn = Manifest {
        groups: vec![Group {
            files: vec![File::new(TURN_FILE, TURN_URL, TURN_SIZE)],
        }],
    };
    download(&turn, &root().join("turn"))?;
    let vad = Manifest {
        groups: vec![Group { files: vec![File::new(VAD_FILE, VAD_URL, VAD_SIZE)] }],
    };
    download(&vad, &root().join("vad"))?;
    Ok(())
}

fn file_sha256(p: &Path) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(p)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn download(m: &Manifest, dir: &Path) -> Result<()> {
    for f in m.groups.iter().flat_map(|g| &g.files) {
        let dest = dir.join(&f.name);
        let crc = f.crc32c()?;
        let have = match (std::fs::metadata(&dest), f.size) {
            (Ok(meta), Some(size)) => meta.len() == size,
            (Ok(_), None) => true,
            (Err(_), _) => false,
        };
        // a file of the right size can still be damaged; the checksum decides when there is one
        let sha_ok = |p: &Path| f.sha256.as_ref().is_none_or(|want| file_sha256(p).is_ok_and(|got| &got == want));
        if have && crc.is_none_or(|want| file_crc32c(&dest).is_ok_and(|got| got == want)) && sha_ok(&dest) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let part = dest.with_extension("part");
        eprintln!("fetch {}", f.name);
        let ok = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&part)
            .arg(&f.url)
            .status()
            .context("run curl")?
            .success();
        if !ok {
            let _ = std::fs::remove_file(&part);
            bail!("download failed: {}", f.url);
        }
        if let Some(size) = f.size {
            let got = std::fs::metadata(&part)?.len();
            if got != size {
                let _ = std::fs::remove_file(&part);
                bail!("{}: expected {size} bytes, got {got}", f.name);
            }
        }
        if let Some(want) = crc {
            let got = file_crc32c(&part)?;
            if got != want {
                let _ = std::fs::remove_file(&part);
                bail!("{}: crc32c {got:08x}, expected {want:08x}", f.name);
            }
        }
        if !sha_ok(&part) {
            let _ = std::fs::remove_file(&part);
            bail!("{}: sha256 does not match the pinned value", f.name);
        }
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_checksum_is_big_endian_crc32c() {
        let f = File {
            name: "streaming_config.json".into(),
            url: String::new(),
            size: Some(512),
            checksum: Some("dPbFiw==".into()),
            checksum_type: Some("crc32c".into()),
            sha256: None,
        };
        assert_eq!(f.crc32c().unwrap(), Some(0x74f6_c58b));
        let none = File { checksum: Some(String::new()), checksum_type: Some(String::new()), ..f };
        assert_eq!(none.crc32c().unwrap(), None);
        assert_eq!(base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(crc32c::crc32c(b"123456789"), 0xe306_9283);
    }
}
