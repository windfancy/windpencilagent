fn main() {
    slint_build::compile("ui/app.slint").expect("Slint 界面编译失败");

    // 把机器人图标编进 exe 的 RT_ICON 资源：资源管理器 / 任务栏显示的就是它。
    // 用 CARGO_CFG_TARGET_OS 运行时判断（build.rs 在 host 上跑，#[cfg] 不可靠）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/app.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=Windows 图标资源编译失败：{e}");
        }
    }
}
