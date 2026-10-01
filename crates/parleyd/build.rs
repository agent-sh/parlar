// Find libmoonshine and its ONNX Runtime next to the binary (target dir) or in ../lib/parley
// (installed layout). parley-moonshine's build script puts them there.
fn main() {
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/../lib/parley");
}
