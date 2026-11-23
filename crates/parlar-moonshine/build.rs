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
    /// The archive holding the ONNX Runtime library, its sha256 and the library's path inside it,
    /// when it is not in the libmoonshine release.
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
    // Microsoft's NuGet package, signed and immutable once published, holds the same 1.23.0
    // build that its GitHub release tarball had, with the same install name
    // (@rpath/libonnxruntime.1.23.0.dylib)
    ort: Some((
        "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/1.23.0/microsoft.ml.onnxruntime.1.23.0.nupkg",
        "cadbf21358de4b6002eddce08329465071271d52bb1483779f6f09670af281fc",
        "runtimes/osx-arm64/native/libonnxruntime.dylib",
    )),
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PARLAR_MOONSHINE_DIR");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if os == "linux" {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out, or
    // <target>/<profile>/build/<crate>/<hash>/out in Cargo's newer build-dir layout
    let profile_dir = out
        .ancestors()
        .find(|d| d.file_name().is_some_and(|n| n == "build"))
        .and_then(Path::parent)
        .expect("OUT_DIR layout")
        .to_path_buf();
    let cache = profile_dir.parent().unwrap().join("moonshine").join(VERSION);
    // a prebuilt release for the target, or the user's own unpacked one
    let known = match (os.as_str(), arch.as_str()) {
        ("windows", "x86_64") => Some(WINDOWS_X64),
        ("macos", "aarch64") => Some(MACOS_ARM64),
        _ => None,
    };
    let dir = match (std::env::var_os("PARLAR_MOONSHINE_DIR"), &known) {
        (Some(d), _) => PathBuf::from(d),
        (None, Some(t)) => fetch_moonshine(&cache, t),
        (None, None) => panic!(
            "no prebuilt libmoonshine for {os}/{arch}; unpack a moonshine-voice release and set PARLAR_MOONSHINE_DIR"
        ),
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
        std::fs::copy(lib.join("onnxruntime.dll"), profile_dir.join("onnxruntime.dll")).expect("copy onnxruntime.dll");
    } else {
        println!("cargo:rustc-link-lib=static=moonshine");
        println!("cargo:rustc-link-lib=c++");
        for fw in ["Foundation", "Accelerate"] {
            println!("cargo:rustc-link-lib=framework={fw}");
        }
        // libmoonshine's objects use clang's availability checks (___isPlatformVersionAtLeast),
        // which live in clang's compiler runtime, not in Rust's
        if let Some(rt) = clang_rt() {
            println!("cargo:rustc-link-search=native={}", rt.parent().unwrap().display());
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
        // ONNX Runtime is weakly linked and opened by parlard from its lib folder before the
        // first call (see models::moonshine), so a binary installed anywhere starts without it
        if let Some(t) = &known {
            let ort_dir = fetch_ort(&cache, t);
            println!("cargo:rustc-link-search=native={}", ort_dir.display());
            std::fs::copy(ort_dir.join("libonnxruntime.1.23.0.dylib"), profile_dir.join("libonnxruntime.1.23.0.dylib"))
                .expect("copy libonnxruntime.1.23.0.dylib");
        } else {
            println!("cargo:rustc-link-search=native={}", lib.display());
        }
        println!("cargo:rustc-link-arg=-Wl,-weak-lonnxruntime");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../lib/parlar");
    }
}

/// clang's compiler runtime for macOS, from the toolchain `cc` resolves to.
fn clang_rt() -> Option<PathBuf> {
    let out = Command::new("cc").args(["-print-resource-dir"]).output().ok()?;
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let lib = dir.join("lib/darwin/libclang_rt.osx.a");
    lib.exists().then_some(lib)
}

fn fetch_moonshine(cache: &Path, t: &Target) -> PathBuf {
    let dir = cache.join(t.moonshine);
    if dir.join("lib").exists() {
        return dir;
    }
    let url = format!("https://github.com/moonshine-ai/moonshine/releases/download/{VERSION}/{}.tar.gz", t.moonshine);
    fetch_archive(cache, &url, t.moonshine_sha, None);
    dir
}

/// ONNX Runtime 1.23.0 in `<cache>/onnxruntime-1.23.0`, as `libonnxruntime.dylib` for the linker
/// and `libonnxruntime.1.23.0.dylib`, the name it is bound by at launch.
fn fetch_ort(cache: &Path, t: &Target) -> PathBuf {
    let (url, sha, member) = t.ort.expect("macOS needs ONNX Runtime from its own release");
    let dir = cache.join("onnxruntime-1.23.0");
    let versioned = dir.join("libonnxruntime.1.23.0.dylib");
    if versioned.exists() {
        return dir;
    }
    fetch_archive(cache, url, sha, Some(member));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::rename(cache.join(member), dir.join("libonnxruntime.dylib")).unwrap();
    let _ = std::fs::remove_dir_all(cache.join("runtimes"));
    std::fs::copy(dir.join("libonnxruntime.dylib"), &versioned).unwrap();
    dir
}

/// Download an archive (a tarball, or a zip such as a NuGet package) into `cache`, check its
/// sha256, and unpack it there: all of it, or only `member`.
fn fetch_archive(cache: &Path, url: &str, sha: &str, member: Option<&str>) {
    std::fs::create_dir_all(cache).unwrap();
    let tarball = cache.join(format!("{}.archive", sha));
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
    // tar detects the format on extraction; the bsdtar in Windows and macOS also reads zip
    run(Command::new("tar").arg("xf").arg(&tarball).arg("-C").arg(cache).args(member), "unpack");
    let _ = std::fs::remove_file(&tarball);
}

fn run(cmd: &mut Command, what: &str) {
    let ok = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}")).success();
    assert!(ok, "{what} failed");
}
