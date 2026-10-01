// Windows and macOS: libmoonshine's releases there are static libraries, so they are linked here.
// The pinned release is downloaded once into the target directory and checked against its
// sha256, unless PARLAR_MOONSHINE_DIR points at an unpacked one. ONNX Runtime stays a shared
// library next to the binaries (onnxruntime.dll delay-loaded on Windows, libonnxruntime.dylib
// found through an rpath on macOS), so parlard can also find it in its data folder after
// `cargo install`. On Linux there is nothing to build: the library is loaded at run time.

use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "v0.1.5";
/// Per target OS: libmoonshine's tarball name and sha256, and where ONNX Runtime comes from.
struct Target {
    moonshine: &'static str,
    moonshine_sha: &'static str,
    /// The tarball holding the ONNX Runtime library and its folder inside, when it is not in the
    /// libmoonshine release.
    ort: Option<(&'static str, &'static str, &'static str)>,
}

const WINDOWS_X64: Target = Target {
    moonshine: "moonshine-voice-windows-x86_64",
    moonshine_sha: "97c1987e8e1cd77bb5fe3b12ce5aad5172637107e1dfda112ea9b21bac8f4b65",
    ort: None,
};
const MACOS_ARM64: Target = Target {
    moonshine: "moonshine-voice-macos-arm64",
    moonshine_sha: "51151f98eb1b20b8fc141bab361a1574dbfdbef8300b632ee6679e373112e0f6",
    ort: Some((
        "https://github.com/microsoft/onnxruntime/releases/download/v1.23.0/onnxruntime-osx-arm64-1.23.0.tgz",
        "8182db0ebb5caa21036a3c78178f17fabb98a7916bdab454467c8f4cf34bcfdf",
        "onnxruntime-osx-arm64-1.23.0/lib",
    )),
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PARLAR_MOONSHINE_DIR");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target = match (os.as_str(), arch.as_str()) {
        ("windows", "x86_64") => WINDOWS_X64,
        ("macos", "aarch64") => MACOS_ARM64,
        ("linux", _) => return,
        _ => panic!("no prebuilt libmoonshine for {os}/{arch}; set PARLAR_MOONSHINE_DIR"),
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out
    let profile_dir = out.ancestors().nth(3).expect("OUT_DIR layout").to_path_buf();
    let cache = profile_dir.parent().unwrap().join("moonshine").join(VERSION);
    let dir = match std::env::var_os("PARLAR_MOONSHINE_DIR") {
        Some(d) => PathBuf::from(d),
        None => fetch_moonshine(&cache, &target),
    };
    let lib = dir.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    if os == "windows" {
        for name in ["moonshine", "moonshine-utils", "bin-tokenizer", "ort-utils"] {
            println!("cargo:rustc-link-lib=static={name}");
        }
        println!("cargo:rustc-link-lib=dylib=onnxruntime");
        println!("cargo:rustc-link-arg=/DELAYLOAD:onnxruntime.dll");
        println!("cargo:rustc-link-lib=delayimp");
        let _ = std::fs::copy(lib.join("onnxruntime.dll"), profile_dir.join("onnxruntime.dll"));
    } else {
        let ort_dir = fetch_ort(&cache, &target);
        println!("cargo:rustc-link-lib=static=moonshine");
        println!("cargo:rustc-link-lib=c++");
        for fw in ["Foundation", "Accelerate"] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
        println!("cargo:rustc-link-search=native={}", ort_dir.display());
        println!("cargo:rustc-link-lib=dylib=onnxruntime");
        // next to the binary, then the data folder parlard fetch fills
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../lib/parlar");
        let _ =
            std::fs::copy(ort_dir.join("libonnxruntime.1.23.0.dylib"), profile_dir.join("libonnxruntime.1.23.0.dylib"));
    }
}

fn fetch_moonshine(cache: &Path, t: &Target) -> PathBuf {
    let dir = cache.join(t.moonshine);
    if dir.join("lib").exists() {
        return dir;
    }
    let url = format!("https://github.com/moonshine-ai/moonshine/releases/download/{VERSION}/{}.tar.gz", t.moonshine);
    fetch_tarball(cache, &url, t.moonshine_sha);
    dir
}

fn fetch_ort(cache: &Path, t: &Target) -> PathBuf {
    let (url, sha, inner) = t.ort.expect("macOS needs ONNX Runtime from its own release");
    let dir = cache.join(inner);
    if dir.exists() {
        return dir;
    }
    fetch_tarball(cache, url, sha);
    dir
}

/// Download a tarball into `cache`, check its sha256, and unpack it there.
fn fetch_tarball(cache: &Path, url: &str, sha: &str) {
    std::fs::create_dir_all(cache).unwrap();
    let tarball = cache.join(format!("{}.tar.gz", sha));
    // curl and tar ship with Windows 10 and later, and with macOS
    run(Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&tarball).arg(url), "download");
    let got = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(std::fs::read(&tarball).unwrap()))
    };
    if got != sha {
        let _ = std::fs::remove_file(&tarball);
        panic!("checksum mismatch for {url}: got {got}");
    }
    run(Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(cache), "unpack");
    let _ = std::fs::remove_file(&tarball);
}

fn run(cmd: &mut Command, what: &str) {
    let ok = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}")).success();
    assert!(ok, "{what} failed");
}
