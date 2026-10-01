//! Model files: where they live and how they are fetched from the Moonshine CDN.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub fn root() -> PathBuf {
    if let Some(p) = std::env::var_os("PARLEY_MODELS") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/share/parley/models")
}

pub fn stt_dir() -> PathBuf {
    root().join("stt/small-streaming-en")
}

/// The G2P and vocoder root that libmoonshine resolves TTS asset keys against.
pub fn tts_dir() -> PathBuf {
    root().join("tts")
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
}

pub fn fetch(voice: &str) -> Result<()> {
    let arch = parley_moonshine::ARCH_SMALL_STREAMING.to_string();
    let stt = parley_moonshine::stt_manifest("en", &[("model_arch", &arch)])?;
    download(&serde_json::from_str(&stt)?, &stt_dir())?;
    let tts = parley_moonshine::tts_manifest("en", &[("voice", voice)])?;
    download(&serde_json::from_str(&tts)?, &tts_dir())?;
    Ok(())
}

fn download(m: &Manifest, dir: &Path) -> Result<()> {
    for f in m.groups.iter().flat_map(|g| &g.files) {
        let dest = dir.join(&f.name);
        if let (Ok(meta), Some(size)) = (std::fs::metadata(&dest), f.size) {
            if meta.len() == size {
                continue;
            }
        } else if f.size.is_none() && dest.exists() {
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
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
}
