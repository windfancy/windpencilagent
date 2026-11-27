fn main() {
    slint_build::compile("ui/app.slint").expect("Slint 界面编译失败");

    // 把机器人图标编进 exe 的 RT_ICON 资源：资源管理器 / 任务栏显示的就是它。
    // 用 CARGO_CFG_TARGET_OS 运行时判断（build.rs 在 host 上跑，#[cfg] 不可靠）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-env-changed=WPA_WIN_SDK");
        println!("cargo:rerun-if-env-changed=WPA_WIN_SDK_BIN");
        println!("cargo:rerun-if-changed=icons/app.ico");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/app.ico");
        // 受限环境（沙箱 / CI）里 reg.exe 可能被拦，winresource 就靠注册表
        // 定位 Windows SDK，找不到时图标资源被**静默跳过**——exe 能跑，但没有
        // RT_ICON，资源管理器里显示默认图标。给一条不依赖注册表的路。
        if let Some(sdk) = sdk_path() {
            println!("cargo:warning=按环境变量定位资源编译器：{sdk}");
            res.set_toolkit_path(&sdk);
        }
        if let Err(e) = res.compile() {
            println!("cargo:warning=Windows 图标资源编译失败：{e}");
        }
    }
}

/// 从环境变量取资源编译器路径，交给 `winresource`。
///
/// `set_toolkit_path` 期望的是**含 `rc.exe` 的目录**（形如
/// `...\Windows Kits\10\bin\10.0.26100.0\x64`）；给 SDK 根它也会自己找
/// `bin/*/<arch>/rc.exe`。原样传，不做路径改写。
fn sdk_path() -> Option<String> {
    for key in ["WPA_WIN_SDK_BIN", "WPA_WIN_SDK"] {
        let Ok(v) = std::env::var(key) else {
            continue;
        };
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}
