use std::env;
use std::process::Command;

fn detect_sdk_major_version() -> Option<u32> {
    let output = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-version"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version_str = String::from_utf8_lossy(&output.stdout);
    let major = version_str.trim().split('.').next()?;
    major.parse().ok()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
    println!("cargo:rerun-if-env-changed=SDKROOT");

    if env::var("DOCS_RS").is_ok() {
        return;
    }

    let _ = detect_sdk_major_version(); // currently unused; reserved for future macos_* feature flags

    println!("cargo:rustc-link-lib=framework=CoreGraphics");
    println!("cargo:rustc-link-lib=framework=IOSurface");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=framework=CoreMedia");
    println!("cargo:rustc-link-lib=framework=CoreVideo");
    println!("cargo:rustc-link-lib=framework=Metal");

    let swift_dir = "swift-bridge";
    let out_dir = env::var("OUT_DIR").unwrap();
    let swift_build_dir = format!("{out_dir}/swift-build");

    println!("cargo:rerun-if-changed={swift_dir}");

    // AeroDesk patch: 支持 iOS / iOS Simulator 交叉编译。
    let target = env::var("TARGET").unwrap_or_default();
    let (swift_triple, sdk_name) = match target.as_str() {
        "aarch64-apple-ios" => ("arm64-apple-ios", Some("iphoneos")),
        "aarch64-apple-ios-sim" => ("arm64-apple-ios-simulator", Some("iphonesimulator")),
        "x86_64-apple-ios" => ("x86_64-apple-ios-simulator", Some("iphonesimulator")),
        "aarch64-apple-darwin" => ("arm64-apple-macosx", None),
        "x86_64-apple-darwin" => ("x86_64-apple-macosx", None),
        other => panic!("apple-cf: unsupported target '{other}'. Expected apple-ios/apple-ios-sim/apple-darwin."),
    };

    let mut swift_args = vec![
        "build".to_string(),
        "-c".to_string(),
        "release".to_string(),
        "--triple".to_string(),
        swift_triple.to_string(),
        "--package-path".to_string(),
        swift_dir.to_string(),
        "--scratch-path".to_string(),
        // 分 target，避免 host/iOS 构建互相污染。
        format!("{swift_build_dir}-{target}"),
    ];
    if let Some(sdk) = sdk_name {
        let sdk_path = Command::new("xcrun")
            .args(["--sdk", sdk, "--show-sdk-path"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if let Some(sdk_path) = sdk_path {
            swift_args.push("--sdk".to_string());
            swift_args.push(sdk_path);
        }
    }

    let output = Command::new("swift")
        .args(&swift_args)
        .output()
        .expect("Failed to build Swift bridge");

    if !output.status.success() {
        eprintln!(
            "Swift build STDOUT:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "Swift build STDERR:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        panic!(
            "Swift build failed with exit code: {:?}",
            output.status.code()
        );
    }

    link_swift_bridge(&format!("{swift_build_dir}-{target}"));
}

fn link_swift_bridge(swift_build_dir: &str) {
    println!("cargo:rustc-link-search=native={swift_build_dir}/release");
    println!("cargo:rustc-link-lib=static=AppleCFBridge");

    println!("cargo:rustc-link-lib=framework=Foundation");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("ios") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }

    if let Ok(output) = Command::new("xcode-select").arg("-p").output() {
        if output.status.success() {
            let xcode_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let swift_lib_path =
                format!("{xcode_path}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx");
            println!("cargo:rustc-link-arg=-Wl,-rpath,{swift_lib_path}");
        }
    }
}
