// Windows only: libmoonshine's Windows release is a static library, so it is linked here.
// The pinned release is downloaded once into the target directory and checked against its
// sha256, unless PARLAR_MOONSHINE_DIR points at an unpacked one. onnxruntime.dll is copied next
// to the binaries and delay-loaded, so parlard can also find it in its data folder after
// `cargo install`. On other platforms there is nothing to build: the library is loaded at run time.

use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "v0.1.5";
const WINDOWS_X64: &str = "97c1987e8e1cd77bb5fe3b12ce5aad5172637107e1dfda112ea9b21bac8f4b65";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PARLAR_MOONSHINE_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out
    let profile_dir = out.ancestors().nth(3).expect("OUT_DIR layout").to_path_buf();
    let dir = match std::env::var_os("PARLAR_MOONSHINE_DIR") {
        Some(d) => PathBuf::from(d),
        None => fetch(&profile_dir),
    };
    let lib = dir.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in ["moonshine", "moonshine-utils", "bin-tokenizer", "ort-utils"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
    println!("cargo:rustc-link-lib=dylib=onnxruntime");
    println!("cargo:rustc-link-arg=/DELAYLOAD:onnxruntime.dll");
    println!("cargo:rustc-link-lib=delayimp");
    let dll = lib.join("onnxruntime.dll");
    let _ = std::fs::copy(&dll, profile_dir.join("onnxruntime.dll"));
}

fn fetch(profile_dir: &Path) -> PathBuf {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    assert_eq!(arch, "x86_64", "libmoonshine has a Windows build for x86_64 only; set PARLAR_MOONSHINE_DIR");
    let name = "moonshine-voice-windows-x86_64";
    let cache = profile_dir.parent().unwrap().join("moonshine").join(VERSION);
    let dir = cache.join(name);
    if dir.join("lib").join("moonshine.lib").exists() {
        return dir;
    }
    std::fs::create_dir_all(&cache).unwrap();
    let tarball = cache.join(format!("{name}.tar.gz"));
    let url = format!("https://github.com/moonshine-ai/moonshine/releases/download/{VERSION}/{name}.tar.gz");
    // curl and tar ship with Windows 10 and later
    run(Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&tarball).arg(&url), "download libmoonshine");
    let got = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(std::fs::read(&tarball).unwrap()))
    };
    if got != WINDOWS_X64 {
        let _ = std::fs::remove_file(&tarball);
        panic!("libmoonshine checksum mismatch for {url}: got {got}");
    }
    run(Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&cache), "unpack libmoonshine");
    let _ = std::fs::remove_file(&tarball);
    dir
}

fn run(cmd: &mut Command, what: &str) {
    let ok = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}")).success();
    assert!(ok, "{what} failed");
}
