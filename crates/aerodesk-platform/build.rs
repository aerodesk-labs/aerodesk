fn main() {
    // screencapturekit 依赖 Swift 运行时（/usr/lib/swift/）
    // 必须在 build.rs 里设置（cargo install 不携带项目级 .cargo/ 配置）
    let target = std::env::var("TARGET").expect("Cargo always sets TARGET in build.rs");
    if target.contains("-apple-") && !target.contains("-ios") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
