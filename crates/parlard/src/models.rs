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
    parlar::dirs::data().join("models")
}

/// The G2P and vocoder root that libmoonshine resolves TTS asset keys against.
pub fn tts_dir() -> PathBuf {
    root().join("tts")
}

const TURN_FILE: &str = "smart-turn-v3.2-cpu.onnx";
const TURN_URL: &str = "https://huggingface.co/pipecat-ai/smart-turn-v3/resolve/f766f81d3cfdf7737ac64aad813d91bbfd56bf93/smart-turn-v3.2-cpu.onnx";
const TURN_SHA256: &str = "2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f";
const TURN_SIZE: u64 = 8_679_182;

const VAD_FILE: &str = "silero_vad.onnx";
const VAD_URL: &str =
    "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad.onnx";
const VAD_SIZE: u64 = 2_327_524;
const VAD_SHA256: &str = "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3";

pub fn vad_model() -> PathBuf {
    root().join("vad").join(VAD_FILE)
}

/// The speech recognizer's directory: a built-in model under the models root, or a directory
/// named in config.toml.
pub fn final_dir() -> PathBuf {
    let model = crate::config::get().recognizer();
    match model.as_str() {
        crate::config::PHONON | crate::config::PARAKEET => root().join("stt").join(model),
        dir => PathBuf::from(dir),
    }
}

pub fn turn_model() -> PathBuf {
    root().join("turn").join(TURN_FILE)
}

/// The runtime files `fetch` installs. On Linux libmoonshine is a shared library with its own
/// ONNX Runtime beside it. On Windows and macOS libmoonshine is linked into parlard (its releases
/// there are static libraries), so only ONNX Runtime is a separate file.
#[cfg(target_os = "linux")]
const MOONSHINE: &str = "libmoonshine.so";
#[cfg(target_os = "linux")]
const ORT: &str = "libonnxruntime.so.1";
#[cfg(target_os = "linux")]
const RUNTIME: &[&str] = &[ORT, MOONSHINE];
#[cfg(windows)]
const ORT: &str = "onnxruntime.dll";
#[cfg(target_os = "macos")]
const ORT: &str = "libonnxruntime.1.23.0.dylib";
#[cfg(not(target_os = "linux"))]
const MOONSHINE: &str = ORT;
#[cfg(not(target_os = "linux"))]
const RUNTIME: &[&str] = &[ORT];

/// Where the runtime files come from, per OS and architecture: the tarball URL, its sha256, and
/// the folder inside it that holds the files. Linux takes libmoonshine's own release (shared
/// library plus ONNX Runtime); Windows takes ONNX Runtime from libmoonshine's release too; macOS
/// takes ONNX Runtime from Microsoft's release, because libmoonshine's macOS release has none.
const RUNTIME_RELEASES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "linux",
        "x86_64",
        "https://github.com/moonshine-ai/moonshine/releases/download/v0.1.5/moonshine-voice-linux-x86_64.tar.gz",
        "9c3a87fea93ff2ad957938868f95a0a366dce9ff8ad86bde6cdcf5a4cadb51df",
        "moonshine-voice-linux-x86_64/lib",
    ),
    (
        "linux",
        "aarch64",
        "https://github.com/moonshine-ai/moonshine/releases/download/v0.1.5/moonshine-voice-linux-arm64.tar.gz",
        "1600c80a0806b7a2582307c98e7a56f4072e4b060498b08a0b75eb20af42def2",
        "moonshine-voice-linux-arm64/lib",
    ),
    (
        "windows",
        "x86_64",
        "https://github.com/moonshine-ai/moonshine/releases/download/v0.1.5/moonshine-voice-windows-x86_64.tar.gz",
        "97c1987e8e1cd77bb5fe3b12ce5aad5172637107e1dfda112ea9b21bac8f4b65",
        "moonshine-voice-windows-x86_64/lib",
    ),
    (
        "macos",
        "aarch64",
        "https://github.com/microsoft/onnxruntime/releases/download/v1.23.0/onnxruntime-osx-arm64-1.23.0.tgz",
        "8182db0ebb5caa21036a3c78178f17fabb98a7916bdab454467c8f4cf34bcfdf",
        "onnxruntime-osx-arm64-1.23.0/lib",
    ),
];

/// Where libmoonshine and its ONNX Runtime are: `$PARLAR_LIB_DIR`, next to this binary,
/// `../lib/parlar` from it (packages), else `$XDG_DATA_HOME/parlar/lib` (what `fetch` fills).
pub fn lib_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("PARLAR_LIB_DIR") {
        return PathBuf::from(p);
    }
    let fetched = parlar::dirs::data().join("lib");
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
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
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
    #[cfg(windows)]
    dll_directory(&lib_dir())?;
    #[cfg(target_os = "macos")]
    load_ort(&lib)?;
    parlar_moonshine::open(&lib)
}

/// ONNX Runtime is weakly linked on macOS (see parlar-moonshine's build script), so parlard
/// starts without it, and `fetch` can run first. dyld binds the weak imports at launch, from
/// `@executable_path` or `@executable_path/../lib/parlar`; a library found only later cannot be
/// bound into a running process. So: if it was not found at launch, say where `fetch` put it.
#[cfg(target_os = "macos")]
fn load_ort(lib: &Path) -> Result<()> {
    unsafe extern "C" {
        fn dlsym(handle: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *mut core::ffi::c_void;
    }
    // RTLD_DEFAULT: the images dyld loaded at launch, which is where the weak dependency landed
    // if it was found
    let rtld_default = -2isize as *mut core::ffi::c_void;
    // SAFETY: a NUL-terminated symbol name; the result is only compared with null
    let bound = unsafe { !dlsym(rtld_default, c"OrtGetApiBase".as_ptr()).is_null() };
    if bound {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let beside = exe.with_file_name(ORT);
    bail!(
        "ONNX Runtime was not found when parlard started. It is at {}; link it next to the binary and start again:\n  ln -sf {} {}",
        lib.display(),
        lib.display(),
        beside.display()
    );
}

/// onnxruntime.dll is delay-loaded (see parlar-moonshine's build script): point the loader at the
/// folder it was installed to before the first call into it.
#[cfg(windows)]
fn dll_directory(dir: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated wide path; the loader copies it
    let ok = unsafe { windows_sys::Win32::System::LibraryLoader::SetDllDirectoryW(wide.as_ptr()) };
    if ok == 0 {
        bail!("set the DLL folder to {}: {}", dir.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

/// Download the pinned libmoonshine release into `$XDG_DATA_HOME/parlar/lib`, unless one is
/// already found.
fn fetch_moonshine() -> Result<()> {
    if RUNTIME.iter().all(|l| lib_dir().join(l).exists()) {
        return Ok(());
    }
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let Some((_, _, url, sha, inner)) = RUNTIME_RELEASES.iter().find(|(o, a, _, _, _)| *o == os && *a == arch) else {
        bail!(
            "no prebuilt speech runtime for {os}/{arch}; put {} in a directory and set PARLAR_LIB_DIR",
            RUNTIME.join(" and ")
        );
    };
    let dest = parlar::dirs::data().join("lib");
    let tmp = dest.join(".fetch");
    let _ = std::fs::remove_dir_all(&tmp);
    let tarball = File { sha256: Some((*sha).into()), size: None, ..File::new("runtime.tar.gz", url, 0) };
    download(&Manifest { groups: vec![Group { files: vec![tarball] }] }, &tmp)?;
    let ok = Command::new("tar")
        .arg("xzf")
        .arg(tmp.join("runtime.tar.gz"))
        .arg("-C")
        .arg(&tmp)
        .status()
        .context("run tar")?
        .success();
    if !ok {
        bail!("unpack {url} failed");
    }
    std::fs::create_dir_all(&dest)?;
    for lib in RUNTIME {
        std::fs::rename(tmp.join(inner).join(lib), dest.join(lib)).with_context(|| format!("install {lib}"))?;
    }
    std::fs::remove_dir_all(&tmp)?;
    #[cfg(target_os = "macos")]
    link_beside_binaries(&dest)?;
    Ok(())
}

/// macOS binds ONNX Runtime at launch from `@executable_path`, so a copy installed anywhere
/// (cargo install, a tarball) gets a symlink to the fetched library next to it.
#[cfg(target_os = "macos")]
fn link_beside_binaries(dest: &Path) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let link = exe.with_file_name(ORT);
    if link.exists() {
        return Ok(());
    }
    match std::os::unix::fs::symlink(dest.join(ORT), &link) {
        Ok(()) => eprintln!("linked {} next to parlard", ORT),
        Err(e) => eprintln!(
            "could not link {} next to {} ({e}); run: ln -sf {} {}",
            ORT,
            exe.display(),
            dest.join(ORT).display(),
            link.display()
        ),
    }
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
const PHONON_REPO: &str =
    "https://huggingface.co/tiyuvta/Phonon-2-ONNX/resolve/df0802202a996b5c0574acd805968940f62a6a04";
const PHONON_FILES: &[(&str, u64, &str)] = &[
    ("decoder_joint-model.onnx", 72_518_934, "420125e0e13596692320c35ef648eee9bf4583718c7896c8732ebf6f50b9ca0d"),
    ("preprocessor-model.onnx", 1_193_996, "8184d564f7d34d1daf04e4b35a0222fb72c54668b38dcd2b8ddda3517676614b"),
    ("vocab.txt", 93_939, "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d"),
    ("config.json", 121, "db59e29a3c1fde6a081bf04965e72bba26cd65be1aee65b064360df8aef468e5"),
];

/// Pinned files: name, size, sha256.
type Files = &'static [(&'static str, u64, &'static str)];

/// Phonon-2's encoders: int8 (the default), the bit-exact exact4x2, and fp32 with its weights file.
const PHONON_ENCODERS: &[(&str, Files)] = &[
    (
        "int8",
        &[("encoder-model.int8.onnx", 614_486_780, "3c100e38ca2e70623c928ab5c5414c62848603ea3d943a7f2e1c3ce1d92fc04b")],
    ),
    (
        "exact4x2",
        &[(
            "encoder-model.exact4x2.onnx",
            662_190_977,
            "abfdefaa1c74d6d3ca367a7ed358732a6140fb26a312b650ee57e46f1a9849ec",
        )],
    ),
    (
        "fp32",
        &[
            ("encoder-model.onnx", 789_941, "799506536ea9ab6933174bae2bb00177284f79419ceb5efa7e35a16bb9d4133f"),
            (
                "encoder-model.onnx.data",
                2_435_420_160,
                "4fa6441ccbed2c8d1242bd5cf7a9ee522b57148fadb34a016fa661e8d540fab3",
            ),
        ],
    ),
];

/// Parakeet TDT 0.6B v3 in ONNX (istupakov/parakeet-tdt-0.6b-v3-onnx, 25 European languages),
/// pinned to one revision. Its NeMo front end is the one Phonon-2 ships, from that pinned repo.
const PARAKEET_REPO: &str =
    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce";
const PARAKEET_FILES: &[(&str, u64, &str)] = &[
    ("decoder_joint-model.int8.onnx", 18_202_004, "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70"),
    ("vocab.txt", 93_939, "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d"),
    ("config.json", 97, "666903c76b9798caf2c210afd4f6cd60b08a8dbf9800ec8d7a3bc0d2148ac466"),
];

const PARAKEET_ENCODERS: &[(&str, Files)] = &[
    (
        "int8",
        &[("encoder-model.int8.onnx", 652_183_999, "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09")],
    ),
    (
        "fp32",
        &[
            ("encoder-model.onnx", 41_770_866, "98a74b21b4cc0017c1e7030319a4a96f4a9506e50f0708f3a516d02a77c96bb1"),
            (
                "encoder-model.onnx.data",
                2_435_420_160,
                "9a22d372c51455c34f13405da2520baefb7125bd16981397561423ed32d24f36",
            ),
        ],
    ),
];

/// The encoder variants a built-in recognizer ships, for validating config.toml.
pub fn encoders(model: &str) -> &'static [&'static str] {
    match model {
        crate::config::PARAKEET => &["int8", "fp32"],
        _ => &["int8", "exact4x2", "fp32"],
    }
}

/// The configured encoder's files, int8 when none is set.
fn encoder_files(table: &'static [(&'static str, Files)]) -> Files {
    let want = crate::config::get().recognizer.encoder.as_deref().unwrap_or("int8");
    table.iter().find(|(e, _)| *e == want).map_or(table[0].1, |(_, f)| f)
}

fn pinned(repo: &str, files: &[(&str, u64, &str)]) -> Manifest {
    let files = files
        .iter()
        .map(|(n, size, sha)| File { sha256: Some((*sha).into()), ..File::new(n, &format!("{repo}/{n}"), *size) })
        .collect();
    Manifest { groups: vec![Group { files }] }
}

/// libmoonshine's default voice for a language, read from its download manifest: a Kokoro voice
/// file `kokoro/voices/<v>.kokorovoice` is the voice `kokoro_<v>`.
pub fn default_voice(lang: &str) -> Option<String> {
    let m: Manifest = serde_json::from_str(&parlar_moonshine::tts_manifest(lang, &[]).ok()?).ok()?;
    m.groups.iter().flat_map(|g| &g.files).find_map(|f| {
        let v = f.name.strip_prefix("kokoro/voices/")?.strip_suffix(".kokorovoice")?;
        Some(format!("kokoro_{v}"))
    })
}

pub fn fetch(voice: &str) -> Result<()> {
    fetch_moonshine()?;
    moonshine()?;
    let cfg = crate::config::get();
    let stt = final_dir();
    match cfg.recognizer().as_str() {
        crate::config::PHONON => {
            download(&pinned(PHONON_REPO, PHONON_FILES), &stt)?;
            download(&pinned(PHONON_REPO, encoder_files(PHONON_ENCODERS)), &stt)?;
        }
        crate::config::PARAKEET => {
            download(&pinned(PARAKEET_REPO, PARAKEET_FILES), &stt)?;
            download(&pinned(PARAKEET_REPO, encoder_files(PARAKEET_ENCODERS)), &stt)?;
            let pre: Vec<_> =
                PHONON_FILES.iter().filter(|(n, _, _)| *n == "preprocessor-model.onnx").copied().collect();
            download(&pinned(PHONON_REPO, &pre), &stt)?;
        }
        dir => eprintln!("recognizer: using the model in {dir}"),
    }
    if cfg.voice.command.is_empty() {
        let lang = cfg.language().tts.unwrap_or("en_us");
        let opts: Vec<(&str, &str)> = if voice.is_empty() { vec![] } else { vec![("voice", voice)] };
        let tts = parlar_moonshine::tts_manifest(lang, &opts).with_context(|| format!("voice files for {lang}"))?;
        download(&serde_json::from_str(&tts)?, &tts_dir())?;
    }
    let turn = Manifest {
        groups: vec![Group {
            files: vec![File { sha256: Some(TURN_SHA256.into()), ..File::new(TURN_FILE, TURN_URL, TURN_SIZE) }],
        }],
    };
    download(&turn, &root().join("turn"))?;
    let vad = Manifest {
        groups: vec![Group {
            files: vec![File { sha256: Some(VAD_SHA256.into()), ..File::new(VAD_FILE, VAD_URL, VAD_SIZE) }],
        }],
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
