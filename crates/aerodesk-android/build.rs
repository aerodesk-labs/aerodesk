fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let slint_path = std::path::Path::new(&manifest_dir).join("../../ui/mobile-common.slint");
    slint_build::compile(slint_path).expect("compile mobile-common.slint");
}
