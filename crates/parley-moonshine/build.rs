// Links libmoonshine from its prebuilt release.
//
// PARLEY_MOONSHINE_DIR may point at an unpacked `moonshine-voice-<platform>` directory (offline
// builds, packagers). Otherwise the pinned release for the target is downloaded once into the
// target directory and checked against its sha256. Either way libmoonshine.so and the
// libonnxruntime.so.1 it loads are copied next to the built binaries, and binaries find them
// through $ORIGIN, so nothing depends on where the build ran.

use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "v0.1.5";
const RELEASES: &[(&str, &str, &str)] = &[
    ("linux", "x86_64", "9c3a87fea93ff2ad957938868f95a0a366dce9ff8ad86bde6cdcf5a4cadb51df"),
    ("linux", "aarch64", "1600c80a0806b7a2582307c98e7a56f4072e4b060498b08a0b75eb20af42def2"),
];
const LIBS: &[&str] = &["libmoonshine.so", "libonnxruntime.so.1"];

fn main() {
    println!("cargo:rerun-if-env-changed=PARLEY_MOONSHINE_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out
    let profile_dir = out.ancestors().nth(3).expect("OUT_DIR layout").to_path_buf();
    let dir = match std::env::var_os("PARLEY_MOONSHINE_DIR") {
        Some(d) => PathBuf::from(d),
        None => fetch(&profile_dir),
    };
    let lib = dir.join("lib");
    for name in LIBS {
        let src = lib.join(name);
        assert!(src.exists(), "{} is missing; set PARLEY_MOONSHINE_DIR to an unpacked moonshine-voice release", src.display());
        // copy under a temporary name, then rename over the old one: a running binary keeps the
        // library it mapped instead of seeing it rewritten in place
        let tmp = profile_dir.join(format!(".{name}.{}.tmp", std::process::id()));
        std::fs::copy(&src, &tmp).expect("copy library next to binaries");
        std::fs::rename(&tmp, profile_dir.join(name)).expect("move library into place");
    }
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=moonshine");
    // examples live one level below the profile dir
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/..:$ORIGIN/../lib/parley");
}

fn fetch(profile_dir: &Path) -> PathBuf {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let Some((_, _, sha)) = RELEASES.iter().find(|(o, a, _)| *o == os && *a == arch) else {
        panic!("no prebuilt libmoonshine for {os}/{arch}; set PARLEY_MOONSHINE_DIR");
    };
    let platform = format!("linux-{}", if arch == "aarch64" { "arm64" } else { "x86_64" });
    let name = format!("moonshine-voice-{platform}");
    let cache = profile_dir.parent().unwrap().join("moonshine").join(VERSION);
    let dir = cache.join(&name);
    if dir.join("lib").join(LIBS[0]).exists() {
        return dir;
    }
    std::fs::create_dir_all(&cache).unwrap();
    let tarball = cache.join(format!("{name}.tar.gz"));
    let url = format!("https://github.com/moonshine-ai/moonshine/releases/download/{VERSION}/{name}.tar.gz");
    run(Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&tarball).arg(&url), "download libmoonshine");
    let sum = Command::new("sha256sum").arg(&tarball).output().expect("run sha256sum");
    let got = String::from_utf8_lossy(&sum.stdout).split_whitespace().next().unwrap_or("").to_string();
    if got != *sha {
        let _ = std::fs::remove_file(&tarball);
        panic!("libmoonshine checksum mismatch for {url}: got {got}, want {sha}");
    }
    run(Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&cache), "unpack libmoonshine");
    let _ = std::fs::remove_file(&tarball);
    dir
}

fn run(cmd: &mut Command, what: &str) {
    let ok = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}")).success();
    assert!(ok, "{what} failed");
}
