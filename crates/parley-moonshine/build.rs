// Links the prebuilt libmoonshine from its release tarball. PARLEY_MOONSHINE_DIR points at the
// unpacked `moonshine-voice-<platform>` directory; the default is the parley vendor dir.
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=PARLEY_MOONSHINE_DIR");
    let dir = std::env::var_os("PARLEY_MOONSHINE_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".local/share/parley/vendor/moonshine-voice-linux-x86_64")
    });
    let lib = dir.join("lib");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=dylib=moonshine");
    // dependents read this as DEP_MOONSHINE_LIB to set their own rpath
    println!("cargo:lib={}", lib.display());
    // resolve libmoonshine.so and its bundled libonnxruntime.so.1 without LD_LIBRARY_PATH
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
}
