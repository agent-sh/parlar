//! Model files: where they live and how they are fetched from the Moonshine CDN.

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

pub fn stt_dir() -> PathBuf {
    root().join("stt/small-streaming-en")
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

/// The final-transcript model: Phonon-2 when present, else Parakeet TDT 0.6B v3.
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

/// The ONNX Runtime library that ships with libmoonshine, so the end-of-turn model shares the
/// runtime the recognizer already loaded. Looked up where the rpath looks: next to this binary,
/// then `../lib/parlar`.
pub fn ort_lib() -> PathBuf {
    const NAME: &str = "libonnxruntime.so.1";
    if let Some(p) = std::env::var_os("PARLAR_ORT_LIB") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let dir = exe.as_deref().and_then(|p| p.parent()).map(PathBuf::from).unwrap_or_default();
    [dir.join(NAME), dir.join("../lib/parlar").join(NAME)]
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(NAME))
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
}

impl File {
    fn new(name: &str, url: &str, size: u64) -> File {
        File { name: name.into(), url: url.into(), size: Some(size), checksum: None, checksum_type: None }
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

/// Parakeet TDT 0.6B v3 in the onnx-asr layout, pinned to one revision of its repo.
const PARAKEET_REPO: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
const PARAKEET_FILES: &[(&str, u64)] = &[
    ("encoder-model.int8.onnx", 652_183_999),
    ("decoder_joint-model.int8.onnx", 18_202_004),
    ("vocab.txt", 93_939),
    ("config.json", 97),
];
/// The NeMo mel preprocessor is generated when onnx-asr is packaged, so it comes from the
/// pinned wheel (MIT), checked by sha256.
const ONNX_ASR_WHEEL: &str = "https://files.pythonhosted.org/packages/6a/60/2fa469a2ee674c35ab48821a1039762ae7b9d0b88188ac1012e779477f76/onnx_asr-0.12.0-py3-none-any.whl";
const ONNX_ASR_WHEEL_SHA256: &str = "5e7ceca454609819ea7833f61e2302e0c8f6ece4f8a78b66c5daba53cb51de4a";
const NEMO128_MEMBER: &str = "onnx_asr/preprocessors/data/nemo128.onnx";

pub fn fetch(voice: &str) -> Result<()> {
    let stt = root().join("stt/parakeet-tdt-0.6b-v3");
    let files = PARAKEET_FILES.iter().map(|(n, size)| File::new(n, &format!("{PARAKEET_REPO}/{n}"), *size)).collect();
    download(&Manifest { groups: vec![Group { files }] }, &stt)?;
    if !stt.join("nemo128.onnx").exists() {
        fetch_preprocessor(&stt)?;
    }
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

fn fetch_preprocessor(dir: &Path) -> Result<()> {
    use sha2::Digest;
    use std::io::Read;
    eprintln!("fetch nemo128.onnx (from the onnx-asr 0.12.0 wheel)");
    let wheel = dir.join(".onnx-asr.whl.part");
    let ok = Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&wheel).arg(ONNX_ASR_WHEEL).status()?.success();
    if !ok {
        bail!("download failed: {ONNX_ASR_WHEEL}");
    }
    let bytes = std::fs::read(&wheel)?;
    let _ = std::fs::remove_file(&wheel);
    let got = format!("{:x}", sha2::Sha256::digest(&bytes));
    if got != ONNX_ASR_WHEEL_SHA256 {
        bail!("onnx-asr wheel checksum mismatch: got {got}");
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut member = zip.by_name(NEMO128_MEMBER)?;
    let mut out = Vec::new();
    member.read_to_end(&mut out)?;
    std::fs::write(dir.join("nemo128.onnx"), out)?;
    Ok(())
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
        if have && crc.is_none_or(|want| file_crc32c(&dest).is_ok_and(|got| got == want)) {
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
        };
        assert_eq!(f.crc32c().unwrap(), Some(0x74f6_c58b));
        let none = File { checksum: Some(String::new()), checksum_type: Some(String::new()), ..f };
        assert_eq!(none.crc32c().unwrap(), None);
        assert_eq!(base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(crc32c::crc32c(b"123456789"), 0xe306_9283);
    }
}
