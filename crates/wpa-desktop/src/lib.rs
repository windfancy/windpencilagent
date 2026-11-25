//! windpencilagent 桌面端（Slint）：UI 线程只做渲染，引擎在独立线程（见 `engine_thread`）。

pub mod engine_thread;

use std::path::PathBuf;

use slint::ComponentHandle;

use engine_thread::Cmd;

slint::include_modules!();

/// 启动桌面应用（阻塞至窗口关闭）。
///
/// 在**专用大栈线程**里跑，而不是直接用 main 线程：Slint 构建组件树是递归的，
/// 十个页面 + 50 个 kit 组件 + 弹层让递归深度不小，Windows 默认 1MB 主线程栈
/// 在 debug 构建下会直接 `has overflowed its stack`（release 帧更小才侥幸不炸）。
/// 给 32MB 栈后 debug/release 都能稳定启动，也给后续加页面留了余量。
pub fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let worker = std::thread::Builder::new()
        .name("wpa-desktop".into())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || run_inner(config_path))?;
    match worker.join() {
        Ok(res) => res,
        Err(_) => Err(anyhow::anyhow!("UI 线程异常退出")),
    }
}

/// 按配置恢复窗口尺寸。
///
/// 只在启动时设一次：`set_size` 是命令式的，不会把布局绑死（不像 `.slint` 里的
/// `width:` 绑定），所以之后最大化/拖拽都能正常跟随。
fn restore_window(app: &AppWindow, cfg: &wpa_engine::AppConfig) {
    let (w, h) = (cfg.app.window_w, cfg.app.window_h);
    // 过小的值多半是配置写坏/首次默认被清空，直接用 .slint 的 preferred-* 兜底
    if w >= 400.0 && h >= 300.0 {
        app.window().set_size(slint::LogicalSize::new(w, h));
    }
}

/// 退出时把实际窗口尺寸写回配置（`remember_window` 关闭则不动）。
///
/// 注意：最大化状态下退出，存下来的是最大化后的尺寸，下次启动是"同样大的普通窗口"
/// 而不是"最大化状态"。Slint 没有跨平台的窗口状态查询，这里不假装能记住。
fn save_window(app: &AppWindow, config_path: &std::path::Path) {
    let Ok(mut cfg) = wpa_engine::AppConfig::load(config_path) else {
        return;
    };
    if !cfg.app.remember_window {
        return;
    }
    let win = app.window();
    let size = win.size();
    let scale = if win.scale_factor() > 0.0 { win.scale_factor() } else { 1.0 };
    let (w, h) = (size.width as f32 / scale, size.height as f32 / scale);
    if w < 400.0 || h < 300.0 {
        return; // 窗口被隐藏/销毁后读到 0，别把有效值覆盖成垃圾
    }
    cfg.app.window_w = w.round();
    cfg.app.window_h = h.round();
    if let Err(e) = cfg.commit(config_path, &[wpa_engine::ConfigSection::App]) {
        tracing::warn!(error = %e, "窗口尺寸写回失败");
    }
}

fn run_inner(config_path: PathBuf) -> anyhow::Result<()> {
    let app = AppWindow::new()?;
    if let Ok(cfg) = wpa_engine::AppConfig::load(&config_path) {
        restore_window(&app, &cfg);
    }
    let handle = engine_thread::spawn(config_path.clone(), app.as_weak());

    // 发送消息
    {
        let h = handle.tx.clone();
        app.on_send(move |text| {
            let _ = h.send(Cmd::Send(text.to_string()));
        });
    }
    // 新会话
    {
        let h = handle.tx.clone();
        app.on_new_session(move || {
            let _ = h.send(Cmd::NewSession);
        });
    }
    // 审批裁决（同时关闭浮层）
    {
        let h = handle.approval.clone();
        let weak = app.as_weak();
        app.on_approve(move |allow| {
            if let Ok(mut slot) = h.lock() {
                if let Some(tx) = slot.take() {
                    let _ = tx.send(allow);
                }
            }
            if let Some(app) = weak.upgrade() {
                app.set_approval_open(false);
            }
        });
    }
    // 空间
    {
        let h = handle.tx.clone();
        app.on_space_switch(move |name| {
            let _ = h.send(Cmd::SwitchSpace(name.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_space_create(move |name, note| {
            let _ = h.send(Cmd::CreateSpace {
                name: name.to_string(),
                note: note.to_string(),
            });
        });
    }
    {
        let h = handle.tx.clone();
        app.on_space_remove(move |name| {
            let _ = h.send(Cmd::RemoveSpace(name.to_string()));
        });
    }
    // 模型
    {
        let h = handle.tx.clone();
        app.on_model_use(move |name| {
            let _ = h.send(Cmd::UseModel(name.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_model_test(move |name| {
            let _ = h.send(Cmd::TestModel(name.to_string()));
        });
    }
    // 记忆
    {
        let h = handle.tx.clone();
        app.on_memory_load(move |date| {
            let _ = h.send(Cmd::LoadMemory(date.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_memory_append(move |text| {
            let _ = h.send(Cmd::AppendDaily(text.to_string()));
        });
    }
    // 外观（先本地生效，再落盘）
    {
        let h = handle.tx.clone();
        let weak = app.as_weak();
        app.on_theme_change(move |dark| {
            if let Some(app) = weak.upgrade() {
                app.set_is_dark(dark);
            }
            let _ = h.send(Cmd::SetTheme(dark));
        });
    }
    {
        let h = handle.tx.clone();
        let weak = app.as_weak();
        app.on_font_change(move |size| {
            if let Some(app) = weak.upgrade() {
                app.set_font_size(size);
            }
            let _ = h.send(Cmd::SetFont(size.max(10) as u32));
        });
    }
    // 打开配置文件 / 工具冒烟 / 刷新
    {
        let h = handle.tx.clone();
        app.on_scope_save(move |text| {
            let rules: Vec<String> = text.lines().map(|l| l.to_string()).collect();
            let _ = h.send(Cmd::SetScope(rules));
        });
    }
    // 内置能力配置卡片：改一项保存一项（无效值会被引擎拒绝并在提示条说明原因）
    {
        let h = handle.tx.clone();
        app.on_cfg_edit(move |key, value| {
            let _ = h.send(Cmd::SetField {
                key: key.to_string(),
                value: value.to_string(),
            });
        });
    }
    {
        let h = handle.tx.clone();
        app.on_open_config(move || {
            let _ = h.send(Cmd::OpenConfig);
        });
    }
    {
        let h = handle.tx.clone();
        app.on_tool_test(move |name| {
            let _ = h.send(Cmd::ToolTest(name.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_refresh(move || {
            let _ = h.send(Cmd::Refresh);
        });
    }
    // 能力页选中条目：交回引擎查详情
    {
        let h = handle.tx.clone();
        app.on_ability_pick(move |n| {
            let _ = h.send(Cmd::PickTool(n.to_string()));
        });
    }
    // 模型档案编辑（P0-6）
    {
        let h = handle.tx.clone();
        app.on_model_new(move || {
            let _ = h.send(Cmd::NewModel);
        });
    }
    {
        let h = handle.tx.clone();
        app.on_model_edit(move |n| {
            let _ = h.send(Cmd::EditModel(n.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_model_delete(move |n| {
            let _ = h.send(Cmd::DeleteModel(n.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        let weak = app.as_weak();
        app.on_model_save(move || {
            let Some(app) = weak.upgrade() else { return };
            let _ = h.send(Cmd::SaveModel {
                name: app.get_md_name().to_string(),
                driver: app.get_md_driver().to_string(),
                base_url: app.get_md_url().to_string(),
                model: app.get_md_model().to_string(),
                api_key: app.get_md_key().to_string(),
                ctx: app.get_md_ctx().to_string(),
                tools_ok: app.get_md_tools(),
                vision: app.get_md_vision(),
                enabled: app.get_md_enabled(),
            });
        });
    }
    // 领域页组绑定
    {
        let h = handle.tx.clone();
        app.on_domain_pick(move |i| {
            let _ = h.send(Cmd::DomainPick(i as usize));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_domain_toggle_group(move |g| {
            let _ = h.send(Cmd::DomainToggleGroup(g.to_string()));
        });
    }
    {
        let h = handle.tx.clone();
        app.on_domain_save_groups(move || {
            let _ = h.send(Cmd::DomainSaveGroups);
        });
    }
    // 能力页注册表操作（启用/禁用/安装/卸载 → 写 registry.toml + 重新装配）
    {
        let h = handle.tx.clone();
        app.on_reg_op(move |id, what| {
            let _ = h.send(Cmd::RegOp {
                id: id.to_string(),
                op: what.to_string(),
            });
        });
    }
    // 切换领域（写配置 + 重新装配）
    {
        let h = handle.tx.clone();
        app.on_domain_switch(move |id| {
            let _ = h.send(Cmd::SwitchDomain(id.to_string()));
        });
    }
    // 重新装配（装了新包 / 手工改了清单后重读）
    {
        let h = handle.tx.clone();
        app.on_tree_reload(move || {
            let _ = h.send(Cmd::ReloadTree);
        });
    }
    // 关窗即退出：通知引擎线程收尾后 join，保证配置落盘
    {
        let h = handle.tx.clone();
        app.window().on_close_requested(move || {
            let _ = h.send(Cmd::Shutdown);
            slint::CloseRequestResponse::HideWindow
        });
    }

    app.run()?;
    // 先存窗口尺寸再 join：join 会触发引擎落盘配置，两边都写同一个文件，
    // 顺序错了会把刚存进去的尺寸又覆盖回旧值。
    save_window(&app, &config_path);
    handle.join();
    Ok(())
}
