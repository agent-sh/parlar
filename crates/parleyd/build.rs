// rpath to libmoonshine, whose directory the parley-moonshine build script exports.
fn main() {
    if let Ok(lib) = std::env::var("DEP_MOONSHINE_LIB") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib}");
    }
}
