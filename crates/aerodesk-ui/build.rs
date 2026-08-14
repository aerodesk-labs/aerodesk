fn main() {
    slint_build::compile("ui/app.slint").expect("slint compile");
    // #7 Windows 发行：内嵌应用图标 + 版本信息（Explorer/任务栏/Properties 显示）。
    // embed-resource 用 rustc-link-arg 直接传资源对象，避免 winres 静态库成员
    // 因无符号引用不被链接器提取（GNU/MSVC 均适用）。
    #[cfg(windows)]
    {
        // 版本资源与 Cargo.toml 自动同步：占位符替换后写入 OUT_DIR 再编译。
        let version = env!("CARGO_PKG_VERSION");
        let nums: Vec<u32> = version.split('.').map(|s| s.parse().unwrap_or(0)).collect();
        let fv = format!(
            "{}, {}, {}, {}",
            nums.first().copied().unwrap_or(0),
            nums.get(1).copied().unwrap_or(0),
            nums.get(2).copied().unwrap_or(0),
            nums.get(3).copied().unwrap_or(0)
        );
        let rc = std::fs::read_to_string("resources/app.rc").expect("app.rc");
        let rc = rc
            .replace("@@FILEVERSION@@", &fv)
            .replace("@@PRODUCTVERSION@@", &fv)
            .replace("@@VERSION@@", version);
        let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("app.rc");
        std::fs::write(&out, rc).expect("write app.rc");
        // compile() 内部失败即 panic。
        embed_resource::compile(out.to_str().unwrap(), embed_resource::NONE);
    }
}
