//! windpencilagent 基座入口（CLI）。
//!
//! 基座自带通用工具（文件 / 命令 / Python 沙箱 / shell / 空间）与领域模板，
//! 能力扩展走插件（C ABI dll）与注册表。
//!
//! 边界：本 crate 不认识任何专业领域词汇；装配与会话构建在 `wpa-engine`。

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use toolkit_core::ToolRegistryExt;

use agent_core::ApprovalCallback;
use kern_core::model::RiskLevel;
use wpa_engine::config::{self, AppConfig};
use wpa_engine::{
    assemble, build_session, Base, SessionInfo, SessionOpts,
};

mod export;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "windpencilagent",
    version,
    about = "windpencilagent —— 通用智能体基座（无树即对话智能体）",
    long_about = None
)]
struct Cli {
    /// 配置文件路径（缺省 ./config.toml；不存在则用默认配置）
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,
    /// 自动批准所有高风险工具（自动化场景；默认逐次询问）
    #[arg(long, global = true)]
    yes: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 交互对话（默认）
    Chat {
        /// 授权目标（可多次；覆盖配置文件与空间授权范围）
        #[arg(long = "target")]
        targets: Vec<String>,
    },
    /// 单次任务（非交互）
    Run {
        /// 目标描述
        #[arg(long)]
        objective: String,
        /// 授权目标（可多次；覆盖配置文件与空间授权范围）
        #[arg(long = "target")]
        targets: Vec<String>,
    },
    /// 列出当前可用的工具面
    Tools,
    /// 直接调用一个工具（冒烟测试 / 调试插件用，不经过 LLM）
    Call {
        /// 工具名（`registry list` 可见）
        tool: String,
        /// JSON 参数，省略为 `{}`
        #[arg(default_value = "{}")]
        args: String,
    },
    /// 领域操作（列出 / 查看当前领域）
    Domain {
        #[command(subcommand)]
        cmd: DomainCmd,
    },
    /// 新建插件骨架（P0-8）：生成 Cargo.toml + PnC.toml + lib.rs
    NewPlugin {
        /// 工具 id / 目录名，必须带命名空间，如 `file.path_info`
        #[arg(long)]
        id: String,
        /// 所属能力组（如 `file`）
        #[arg(long)]
        group: String,
        /// 展示名；省略则用 id
        #[arg(long)]
        name: Option<String>,
        /// 一句话说明（进 LLM 上下文）
        #[arg(long, default_value = "")]
        summary: String,
        /// 风险档：none/low/medium/high/critical
        #[arg(long, default_value = "low")]
        risk: String,
        /// 插件根目录；省略则用 exe 同级 `plugins/`
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 注册表导出（生成领域工具清单）
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    /// 写出示例配置（已存在则不覆盖）
    Init,
    /// 配置自检（版本/字段/档案引用）
    Check,
}

#[derive(Subcommand)]
enum DomainCmd {
    /// 列出领域根目录下的全部领域
    List,
    /// 查看某个领域（缺省 = 当前激活领域）的概要
    Show {
        /// 领域 id；省略则用配置里的 domain.active
        id: Option<String>,
    },
    /// 改写领域的组引用（组↔领域绑定），保留 domain.toml 里的注释
    SetGroups {
        /// 逗号分隔的组 id，如 `core,file,net`；空串 = 纯对话领域（无工具）
        ids: String,
        /// 领域 id；省略则用配置里的 domain.active
        #[arg(long)]
        id: Option<String>,
    },
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// 导出工具清单（默认写到 domains/<id>/toolkit.toml）
    Export {
        /// 领域 id；省略则用配置里的 domain.active
        #[arg(long)]
        domain: Option<String>,
        /// 输出路径；省略则写 domains/<id>/toolkit.toml
        #[arg(long)]
        out: Option<PathBuf>,
        /// 只校验不写入：与现有文件比对，不一致则退出码 2（CI 用）
        #[arg(long)]
        check: bool,
    },
    /// 列出注册表（组 / 条目 / 状态）
    List,
    /// 安装一个组（组 id 或条目 id）：候选 → 已安装并启用
    Install {
        /// 组 id（如 `file`）或条目 id（如 `file.path_info`）
        id: String,
    },
    /// 卸载一个组/条目：保留记录，条目转为候选
    Uninstall { id: String },
    /// 启用一个组/条目
    Enable { id: String },
    /// 禁用一个组/条目
    Disable { id: String },
    /// 把条目移到另一个组（组↔条目绑定）
    Move {
        /// 条目 id（如 `file.path_info`）
        id: String,
        /// 目标组 id（不存在则新建）
        #[arg(long)]
        group: String,
    },
}

// ---------------------------------------------------------------------------
// 审批与对话
// ---------------------------------------------------------------------------

fn approval_callback(auto: bool) -> ApprovalCallback {
    Arc::new(move |tool: &str, risk: RiskLevel, args: &str| -> bool {
        if auto {
            return true;
        }
        println!("\n┌─ 审批请求 ─────────────────────────────────");
        println!("│ 工具：{tool}");
        println!("│ 风险：{risk:?}");
        println!("│ 参数：{}", truncate(args, 400));
        print!("└─ 放行？[y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes" | "是")
    })
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

fn print_turn(turn: &agent_core::ChatTurn) {
    for line in &turn.trace {
        println!("  · {line}");
    }
    println!("{}", turn.reply);
}

fn print_status(info: &SessionInfo) {
    let model = if info.model.is_empty() {
        "未配置（降级模式）".to_string()
    } else {
        format!("{}（链 {} 档）", info.model, info.models_chain.len())
    };
    println!(
        "工具：{} 个｜模型：{}｜空间：{}｜领域：{}｜上下文窗口：{} tok",
        info.tool_count,
        model,
        if info.space.is_empty() { "全局" } else { &info.space },
        if info.domain_id.is_empty() {
            "未加载".to_string()
        } else {
            format!("{}（{}）", info.domain_name, info.domain_id)
        },
        info.context_window
    );
    if !info.agent_mode {
        println!("提示：当前不发送工具面（chat 模式或模型不支持 function calling）");
    }
    for w in &info.warnings {
        println!("[提示] {w}");
    }
}

async fn repl(
    base: Base,
    cfg: AppConfig,
    targets: Vec<String>,
    auto: bool,
) -> anyhow::Result<()> {
    let (mut session, info) = build_session(
        &base,
        SessionOpts {
            targets,
            approval: approval_callback(auto),
            events: None,
            auto_approve: auto,
        },
    );

    println!("windpencilagent {} —— 基座已就绪", env!("CARGO_PKG_VERSION"));
    print_status(&info);
    println!("输入 /quit 退出｜/tools 看工具面｜/help 看命令");

    let stdin = std::io::stdin();
    loop {
        print!("\n› ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "/quit" | "/exit" | ":q") {
            break;
        }
        if input == "/tools" {
            print_tools(&base);
            continue;
        }
        if input == "/help" {
            println!(
                "命令：/tools 工具面｜/plan 当前计划｜/call <工具名> [JSON] 直接调用｜\
                 /phase [id] 查看或切换阶段｜/quit 退出"
            );
            continue;
        }
        if input == "/plan" {
            print_plan(&session);
            continue;
        }
        if input == "/phase" || input.starts_with("/phase ") {
            let to = input.strip_prefix("/phase").unwrap_or("").trim();
            print_phase(&base, if to.is_empty() { None } else { Some(to) });
            continue;
        }
        if let Some(rest) = input.strip_prefix("/call ") {
            // 与主链路同一份授权范围（--target 覆盖、空间覆盖都在 build_session 里算过）
            let scope = session.authorized_scope().to_vec();
            direct_call(&base, &cfg, &scope, auto, rest).await;
            continue;
        }
        let turn = session.turn(input).await;
        print_turn(&turn);
    }

    base.shutdown().await;
    let events = base.runtime.events().len();
    if events > 0 {
        println!("\n[审计] 本次会话群组事件 {events} 条");
    }
    let _ = &cfg;
    Ok(())
}

async fn run_once(
    base: Base,
    objective: String,
    targets: Vec<String>,
    auto: bool,
) -> anyhow::Result<i32> {
    let (mut session, _info) = build_session(
        &base,
        SessionOpts {
            targets,
            approval: approval_callback(auto),
            events: None,
            auto_approve: auto,
        },
    );
    let turn = session.turn(&objective).await;
    print_turn(&turn);
    base.shutdown().await;
    Ok(if turn.ok { 0 } else { 1 })
}

fn print_tools(base: &Base) {
    let snap = base.snapshot();
    println!("已注册工具 {} 个：", snap.tools.len());
    for t in &snap.tools {
        println!("  · {:<22} [{:<7}] {:<16} {}", t.name, t.risk, t.group, truncate(&t.description, 56));
    }
    if !snap.isolated.is_empty() {
        println!("隔离区（未挂载）：{}", snap.isolated.join(", "));
    }
    if !snap.missing_impls.is_empty() {
        println!("未链接的群组实现：{}", snap.missing_impls.join(", "));
    }
}

/// 查看结构化计划（`plan.*` 维护的那份状态，与模型看到的是同一份）。
fn print_plan(session: &agent_core::ChatSession) {
    match session.plan_snapshot() {
        None => println!("计划层未启用（[agent].plan_mode = off）"),
        Some(p) => {
            let view = p.render();
            if view.trim().is_empty() {
                println!("计划层已启用，当前还没有计划（模型可用 plan.create 建计划）");
            } else {
                println!("{view}");
            }
        }
    }
}

/// 查看 / 切换阶段（阶段机由逻辑群组提供；无阶段时提示）。
fn print_phase(base: &Base, to: Option<&str>) {
    let mut guard = match base.phases.lock() {
        Ok(g) => g,
        Err(_) => {
            println!("阶段状态不可用（锁中毒）");
            return;
        }
    };
    if guard.is_empty() {
        println!("当前树未定义阶段（无逻辑群组或用例不启用阶段机）");
        return;
    }
    if let Some(id) = to {
        match guard.advance(Some(id)) {
            Ok(t) => println!(
                "阶段：{} → {}\n（本阶段可用工具组已收敛，下一轮对话生效）",
                t.from.unwrap_or_else(|| "未进入".into()),
                t.to
            ),
            Err(e) => println!("切换失败：{e}"),
        }
        return;
    }
    let all: Vec<String> = guard
        .phases()
        .iter()
        .map(|p| format!("{}({})", p.id, p.label))
        .collect();
    println!("当前阶段：{}", guard.current_id().unwrap_or("未进入（全阶段可用）"));
    println!("全部阶段：{}", all.join(" → "));
    if let Some(p) = guard.current() {
        println!("进入条件：{}", p.entry_hint);
        println!("退出条件：{}", p.exit_condition);
        println!("可用组：{}", p.allowed_groups.join("、"));
    }
    println!("切换：/phase <id>");
}

/// `/call <工具名> [JSON]`：不经过 LLM 直接调工具（冒烟测试用）。
///
/// 与主链路同一套红线：先 ScopeGuard（空范围 + 带目标 = 拒绝），
/// 再风险审批（High/Critical 需确认），然后才执行。
async fn direct_call(base: &Base, cfg: &AppConfig, scope: &[String], auto: bool, rest: &str) {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    if name.is_empty() {
        println!("用法：/call <工具名> [JSON 参数]（例：/call net.reach {{\"host\":\"127.0.0.1\"}}）");
        println!("授权范围来自本会话（--target / 空间 / 配置），可用范围：{}", if scope.is_empty() { "（空：带目标一律拒绝）".to_string() } else { scope.join(", ") });
        return;
    }
    let raw = parts.next().unwrap_or("{}").trim();
    let raw = if raw.is_empty() { "{}" } else { raw };
    let args: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            println!("参数不是合法 JSON：{e}");
            return;
        }
    };
    let Some(tool) = base.kern.tool(name) else {
        println!("未注册的工具：{name}（/tools 查看全部）");
        return;
    };
    if let Some(reason) = agent_core::scope_denial(scope, &cfg.scope.denylist, &args) {
        println!("拒绝：{reason}");
        return;
    }
    let risk = tool.risk_for(&args);
    if risk.needs_approval() && !auto {
        println!("该工具风险为 {risk:?}，需要确认。放行？[y/N] ");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if !matches!(line.trim(), "y" | "Y" | "yes" | "是") {
            println!("已取消");
            return;
        }
    }
    if risk.needs_approval() {
        println!("[{risk:?}] --yes 自动放行");
    }
    let ctx = kern_core::model::ExecutionContext {
        session_id: kern_core::model::SessionId::new(),
        agent_id: kern_core::model::AgentId::new("cli-direct"),
        task_id: kern_core::model::TaskId::new("direct-call"),
        target: String::new(),
        risk,
        approved: true,
        // 直接把本会话的授权范围带下去，插件据此做 ScopeGuard 二次校验
        scope: scope.to_vec(),
    };
    let started = std::time::Instant::now();
    let res = tool.call(&ctx, args).await;
    println!(
        "{} {}（{}ms）",
        if res.ok { "✓" } else { "✗" },
        res.summary,
        started.elapsed().as_millis()
    );
    if let Some(d) = &res.detail {
        println!("{}", serde_json::to_string_pretty(d).unwrap_or_default());
    }
}

// ---------------------------------------------------------------------------
// 领域（描述包）
// ---------------------------------------------------------------------------

fn domain_list(cfg: &AppConfig) -> anyhow::Result<()> {
    let root = cfg.domains_root();
    println!("领域目录：{}", root.display());
    if !root.is_dir() {
        println!("\n目录不存在 —— 首次装配时会自动写出默认领域模板。");
        return Ok(());
    }
    let rows = wpa_engine::domain::list(&root, &cfg.domain.active);
    if rows.is_empty() {
        println!("\n没有可用领域（目录下需要有 <id>/domain.toml）。");
        return Ok(());
    }
    println!("\n共 {} 个领域：", rows.len());
    for r in &rows {
        if let Some(err) = &r.error {
            println!("  · {:<14} [解析失败] {err}", r.id);
            continue;
        }
        println!(
            "  · {:<14} {} 组 {}｜知识 {} 条｜范式 {}｜{}",
            r.id,
            if r.active { "[激活]" } else { "      " },
            r.groups,
            r.knowledge_entries,
            if r.has_workflow { "有" } else { "无" },
            truncate(&r.summary, 42)
        );
    }
    Ok(())
}

/// 改写领域的组引用（组↔领域绑定）。
fn domain_set_groups(cfg: &AppConfig, raw: &str, id: Option<&str>) -> anyhow::Result<()> {
    let id = id.map(|s| s.to_string()).unwrap_or_else(|| cfg.domain.active.clone());
    let file = cfg.domains_root().join(&id).join("domain.toml");
    if !file.is_file() {
        anyhow::bail!("领域 {id} 不存在：{}", file.display());
    }
    let ids: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for g in &ids {
        if !wpa_engine::catalog::is_known_group(g) {
            // 未知组不致命（插件组可能还没装），但必须提示
            eprintln!("提示：{g} 不是基座内置组，装配时若没有任何工具属于它会告警");
        }
    }
    wpa_engine::domain::set_groups(&file, &ids).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("领域 {id} 的组已更新（{} 个）：{}", ids.len(), ids.join(", "));
    println!("文件：{}", file.display());
    Ok(())
}

fn domain_show(cfg: &AppConfig, id: Option<&str>) -> anyhow::Result<()> {
    let id = id.unwrap_or(&cfg.domain.active);
    let dir = cfg.domains_root().join(id);
    if !dir.join("domain.toml").is_file() {
        println!("领域 {id} 不存在：{}", dir.join("domain.toml").display());
        println!("（`windpencilagent-cli domain list` 看有哪些领域；首次装配会写出 default）");
        return Ok(());
    }
    let pack = match wpa_engine::domain::load(&dir) {
        Ok(p) => p,
        Err(e) => {
            println!("领域 {id} 解析失败：{e}");
            std::process::exit(2);
        }
    };
    println!("领域：{}（{}）v{}", pack.manifest.name, pack.manifest.id, pack.manifest.version);
    println!("目录：{}", pack.dir.display());
    println!("类型：{}", if pack.manifest.kind.is_empty() { "general" } else { &pack.manifest.kind });
    println!("简介：{}", pack.manifest.summary);
    println!("\n引用组（{} 个）：{}", pack.manifest.groups.len(), pack.manifest.groups.join("、"));
    println!("工具面：禁止高风险的组外工具一律不可见；本领域 allow_high_risk = {}", pack.params.allow_high_risk);
    println!("轮数上限：{}", if pack.params.max_tool_rounds == 0 { "沿用全局".to_string() } else { pack.params.max_tool_rounds.to_string() });
    println!(
        "\n描述文件：工作范式 {}｜知识 {} 条｜红线文件 {}",
        pack.manifest.workflow.as_deref().unwrap_or("—"),
        pack.knowledge_entries,
        pack.manifest.constraints.as_deref().unwrap_or("—")
    );
    if let Some(w) = &pack.workflow {
        println!("范式注入策略：{}（预算 {} 字）", w.inject, w.budget_chars);
    }
    if !pack.tool_drop.is_empty() {
        println!("tool_filter.drop：{}", pack.tool_drop.join("、"));
    }
    for w in &pack.warnings {
        println!("  ! {w}");
    }
    Ok(())
}

/// 新建插件骨架（P0-8）：3 分钟从零到"工具出现在能力页"。
///
/// 只生成三个文件 + 往 `plugins/Cargo.toml` 的 members 里加一行；
/// 编译与复制 dll 交给 build 脚本（或用户手动两步），刻意不在向导里偷偷起 cargo。
fn new_plugin(
    cfg: &AppConfig,
    id: &str,
    group: &str,
    name: Option<&str>,
    summary: &str,
    risk: &str,
    root: Option<PathBuf>,
) -> anyhow::Result<()> {
    if !id.contains('.') {
        anyhow::bail!("插件 id 要带命名空间（例：`file.path_info`），否则组里会出现裸名字");
    }
    if id != dir_name_for(id) {
        anyhow::bail!("id 只允许 [a-z0-9_.]：{id}");
    }
    let risk = risk.to_ascii_lowercase();
    if !["none", "low", "medium", "high", "critical"].contains(&risk.as_str()) {
        anyhow::bail!("risk 只能是 none/low/medium/high/critical，收到 {risk}");
    }

    let plugins_root = root.unwrap_or_else(|| cfg.plugins_root());
    let pkg = id.replace('.', "_");
    let dir = plugins_root.join(group).join(id);
    if dir.exists() {
        anyhow::bail!("目录已存在：{}（要重建请先删掉它）", dir.display());
    }
    std::fs::create_dir_all(dir.join("src"))?;

    let display = name.unwrap_or(id);
    let summary = if summary.trim().is_empty() {
        format!("{display}：TODO 写清楚它做什么、什么时候该用")
    } else {
        summary.trim().to_string()
    };

    // 1) PnC.toml
    std::fs::write(
        dir.join("PnC.toml"),
        format!(
            r#"[plugin]
id = "{id}"
name = "{display}"
version = "0.1.0"
group = "{group}"
risk = "{risk}"
permissions = []
binary = "{pkg}.dll"
summary = "{summary}"
default_enabled = true
tags = ["{group}"]
capabilities = []
"#
        ),
    )?;

    // 2) Cargo.toml
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{pkg}"
version.workspace = true
edition.workspace = true
description = "{summary}"

[lib]
name = "{pkg}"
crate-type = ["cdylib"]

[dependencies]
plugin-sdk = {{ workspace = true }}
serde_json = {{ workspace = true }}
"#
        ),
    )?;

    // 3) src/lib.rs
    std::fs::write(
        dir.join("src").join("lib.rs"),
        format!(
            r##"//! {display}
//!
//! 由 `wpa-cli new-plugin` 生成。改 `run()` 即可，导出符号由 `declare_tool!` 负责。

use plugin_sdk::prelude::*;

fn run(args: &Value) -> Result<Reply, String> {{
    // 必填参数用 args::req_str（缺失即报错），可选参数给默认值
    let input = args::req_str(args, "input")?;
    Ok(Reply::both(
        format!("收到：{{input}}"),
        json!({{ "input": input }}),
    ))
}}

declare_tool! {{
    id: "{id}",
    group: "{group}",
    version: "0.1.0",
    risk: "{risk}",
    permissions: [],
    summary: "{summary}",
    schema: r#"{{"type":"object","required":["input"],"properties":{{"input":{{"type":"string","description":"输入"}}}}}}"#,
    run: run,
}}
"##
        ),
    )?;

    // 4) 加进 plugins workspace 的 members（Cargo glob 对空组目录报错，必须显式列名）
    let ws = plugins_root.join("Cargo.toml");
    let member_line = format!("\"{group}/{id}\"");
    let mut added = false;
    if ws.is_file() {
        let text = std::fs::read_to_string(&ws)?;
        if !text.contains(&member_line) {
            let replaced = text.replacen(
                "members = [",
                &format!("members = [\n    {member_line},"),
                1,
            );
            if replaced != text {
                std::fs::write(&ws, replaced)?;
                added = true;
            }
        }
    }

    println!("已生成插件骨架：{}", dir.display());
    println!("  PnC.toml / Cargo.toml / src/lib.rs");
    if added {
        println!("已登记到 {}", ws.display());
    }
    println!("\n下一步：");
    println!("  1) 在 {} 里实现 run()", dir.join("src").join("lib.rs").display());
    println!(
        "  2) cd {} && cargo build --release",
        plugins_root.display()
    );
    println!(
        "  3) 把 target/release/{pkg}.dll 复制到 {}",
        dir.display()
    );
    println!("  4) wpa-cli registry list（应出现 {id}，状态=未安装）");
    println!("  5) wpa-cli registry install {group} → 工具面出现 {id}");
    Ok(())
}

/// 目录名规则：只允许小写字母、数字、点、下划线、连字符。
fn dir_name_for(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        .collect()
}

/// 直接调用工具并打印结果（`wpa-cli call`）。
async fn call_tool(base: &Base, name: &str, raw: &str) -> anyhow::Result<()> {
    let args: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| anyhow::anyhow!("参数不是合法 JSON：{e}"))?;
    let Some(tool) = base.kern.tool(name) else {
        anyhow::bail!("未注册的工具：{name}（用 `tools` 查看全部，`registry list` 看注册表）");
    };
    let ctx = kern_core::model::ExecutionContext {
        scope: Vec::new(),
        session_id: kern_core::model::SessionId::new(),
        agent_id: kern_core::model::AgentId::new("cli-call"),
        task_id: kern_core::model::TaskId::new("cli-call"),
        target: String::new(),
        risk: tool.risk(),
        approved: true,
    };
    let out = tool.call(&ctx, args).await;
    println!("ok={} · {} ms", out.ok, out.duration_ms);
    println!("{}", out.summary);
    if let Some(d) = &out.detail {
        let txt = serde_json::to_string_pretty(d).unwrap_or_else(|_| d.to_string());
        println!("--- detail ---\n{txt}");
    }
    if !out.ok {
        std::process::exit(2);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 注册表（data/registry.toml）
// ---------------------------------------------------------------------------

/// 注册表操作类型。
#[derive(Clone, Copy)]
enum RegOp {
    Install,
    Uninstall,
    Enable,
    Disable,
}

fn registry_file(cfg: &AppConfig) -> PathBuf {
    wpa_engine::registry::registry_path(&cfg.data_root())
}

fn registry_list(cfg: &AppConfig) -> anyhow::Result<()> {
    let path = registry_file(cfg);
    let (mut reg, created) = wpa_engine::registry::ToolRegistry::load_or_init(&path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // 先扫一遍 manifests：list 要能看到"磁盘上有但还没安装"的候选
    let (snap, scan_problems) = wpa_engine::registry::scan_plugins(&cfg.plugins_root());
    let merged = reg.reconcile(&snap);
    if !merged.is_empty() {
        reg.save(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if created {
        println!("已生成注册表：{}", path.display());
    } else {
        println!("注册表：{}", path.display());
    }
    if !merged.is_empty() {
        println!("扫描：{}", merged.describe());
    }
    for p in &scan_problems {
        println!("  清单告警：{p}");
    }
    // 候选 = 扫描到但注册表里没有（账本只记已安装）
    let candidates: Vec<_> = snap
        .groups
        .iter()
        .flat_map(|g| g.entries.iter().map(move |e| (g, e)))
        .filter(|(_, e)| reg.entry(&e.id).is_none())
        .collect();
    let st = reg.stats();
    println!(
        "组 {}/{} 启用 · 已装 {}（可用 {} / 缺文件 {}）· 未安装 {}",
        st.groups_enabled,
        st.groups,
        st.entries,
        st.ok,
        st.broken,
        candidates.len()
    );
    if !candidates.is_empty() {
        println!("\n未安装（扫描候选，需 registry install 后才可用）：");
        for (g, e) in &candidates {
            println!("  · {:<24} 组 {:<14} {}", e.id, g.id, e.name);
        }
    }
    for g in &reg.groups {
        let on = if g.enabled { "启用" } else { "停用" };
        let live = g.entries.iter().filter(|e| e.active(g.enabled)).count();
        println!(
            "\n[{on}] {}（{}）  来源 {} · 生效 {} 条",
            g.id,
            g.name,
            g.source,
            live
        );
        for e in &g.entries {
            let mark = if e.active(g.enabled) { "✓" } else { "·" };
            let state = match e.status.as_str() {
                "ok" => {
                    if !e.installed {
                        "未安装"
                    } else if !e.enabled {
                        "已禁用"
                    } else if !g.enabled {
                        "组停用"
                    } else {
                        "可用"
                    }
                }
                "broken" => "缺文件",
                "discovered" => "未安装",
                other => other,
            };
            println!(
                "  {mark} {:<24} {:<6} {:<7} {:<10} {}",
                e.id, e.source, e.risk, state, e.name
            );
            if !e.note.is_empty() {
                println!("      ↳ {}", e.note);
            }
        }
    }
    Ok(())
}

/// 把条目移到另一个组（`registry move`）。
///
/// 为什么要单独一条：一个工具"归哪个组"决定了领域能否把它收敛进来，
/// 而工具可能来自内置 crate 或插件，改代码/改清单都不方便——注册表里一行搞定。
fn registry_move(cfg: &AppConfig, tool: &str, group: &str) -> anyhow::Result<()> {
    let path = registry_file(cfg);
    let (mut reg, _) =
        wpa_engine::registry::ToolRegistry::load_or_init(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (snap, _) = wpa_engine::registry::scan_plugins(&cfg.plugins_root());
    reg.reconcile(&snap);
    match reg.move_entry(tool, group) {
        Some((from, to)) if from == to => {
            println!("{tool} 已经在组 {to} 里，无需移动");
            Ok(())
        }
        Some((from, to)) => {
            reg.save(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{tool}：{from} → {to}（下次装配生效）");
            Ok(())
        }
        None => anyhow::bail!(
            "注册表里没有已安装的条目 `{tool}`（内置工具不能换组；未安装的先 registry install）"
        ),
    }
}

fn registry_act(cfg: &AppConfig, id: &str, op: RegOp) -> anyhow::Result<()> {
    let path = registry_file(cfg);
    let (mut reg, _) =
        wpa_engine::registry::ToolRegistry::load_or_init(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    // 操作前先对账一次：已装条目的 ok/broken 要反映磁盘现状
    let (snap, _) = wpa_engine::registry::scan_plugins(&cfg.plugins_root());
    reg.reconcile(&snap);

    // "存在"有三种：注册表里的组、注册表里的已装条目、扫描到的候选
    let candidate = snap
        .groups
        .iter()
        .flat_map(|g| g.entries.iter().map(move |e| (g, e)))
        .find(|(_, e)| e.id == id);
    let is_group = reg.group(id).is_some() || snap.groups.iter().any(|g| g.id == id);
    let is_entry = reg.entry(id).is_some() || candidate.is_some();
    if !is_group && !is_entry {
        anyhow::bail!("注册表与扫描结果里都没有 `{id}`");
    }

    let msg = match (op, is_group) {
        (RegOp::Install, true) => {
            let n = reg.install_group_from(id, &snap);
            format!("组 {id} 已安装：{n} 个条目")
        }
        (RegOp::Install, false) => {
            if reg.is_installed(id) {
                format!("条目 {id} 已安装")
            } else if let Some((g, e)) = candidate {
                reg.install_candidate(&g.id, &g.source, e)
                    .map_err(|x| anyhow::anyhow!("{x}"))?;
                format!("条目 {id} 已安装（组 {}）", g.id)
            } else {
                format!("找不到条目 {id}（也不在扫描候选里）")
            }
        }
        (RegOp::Uninstall, true) => {
            let n = reg.uninstall_group(id);
            format!("组 {id} 已卸载：{n} 个条目转为候选")
        }
        (RegOp::Uninstall, false) => {
            if reg.set_entry_enabled(id, false) {
                format!("条目 {id} 已禁用（记录保留，可再启用）")
            } else {
                format!("条目 {id} 状态未变化")
            }
        }
        (RegOp::Enable, true) => {
            if reg.set_group_enabled(id, true) {
                format!("组 {id} 已启用")
            } else {
                format!("组 {id} 已是启用状态")
            }
        }
        (RegOp::Enable, false) => {
            if reg.set_entry_enabled(id, true) {
                format!("条目 {id} 已启用")
            } else {
                format!("条目 {id} 已是启用状态")
            }
        }
        (RegOp::Disable, true) => {
            if reg.set_group_enabled(id, false) {
                format!("组 {id} 已停用（条目开关保留）")
            } else {
                format!("组 {id} 已是停用状态")
            }
        }
        (RegOp::Disable, false) => {
            if reg.set_entry_enabled(id, false) {
                format!("条目 {id} 已禁用")
            } else {
                format!("条目 {id} 已是禁用状态")
            }
        }
    };
    reg.save(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{msg}");
    println!("注册表已更新：{}（下次装配生效）", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// 注册表导出
// ---------------------------------------------------------------------------

async fn registry_export(
    cfg: &AppConfig,
    domain: Option<String>,
    out: Option<PathBuf>,
    check: bool,
) -> anyhow::Result<()> {
    let id = domain.unwrap_or_else(|| cfg.domain.active.clone());
    let dir = cfg.domains_root().join(&id);
    if !dir.join("domain.toml").is_file() {
        println!("领域 {id} 不存在：{}", dir.join("domain.toml").display());
        std::process::exit(2);
    }
    let pack = wpa_engine::domain::load(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    let base = assemble(cfg, None).await?;
    let text = export::render_for(&base, &id, &pack.manifest.name, &pack.manifest.groups);
    base.shutdown().await;

    let target = out.unwrap_or_else(|| dir.join("toolkit.toml"));
    if check {
        let old = std::fs::read_to_string(&target).unwrap_or_default();
        if old == text {
            println!("清单一致：{}", target.display());
            return Ok(());
        }
        println!("清单已过期：{}", target.display());
        println!("  重新生成：windpencilagent-cli registry export --domain {id}");
        std::process::exit(2);
    }
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&target, &text)?;
    println!("已写出工具清单：{}（{} 字节）", target.display(), text.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// 树包管理（安装 / 启停 / 卸载 / 校验）
// ---------------------------------------------------------------------------

/// 统一的失败出口：把字段级原因逐条打印后以 2 退出（fail-closed，不装半套）。
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,windpencilagent=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(env).try_init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.cmd {
        Some(Cmd::Init) => {
            // 写到 --config 指定的位置；已存在则不动（避免覆盖用户改过的档案）
            let p = &cli.config;
            if p.exists() {
                println!("配置已存在，未覆盖：{}", p.display());
                println!("（需要示例模板：删除该文件后重跑 `windpencilagent init`，或 `windpencilagent check` 看当前问题）");
                return Ok(());
            }
            if let Some(dir) = p.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            std::fs::write(p, config::SAMPLE_CONFIG)?;
            println!("已写入示例配置：{}", p.display());
            // 领域模板一起落地：只写缺失文件，不覆盖用户改动
            let cfg = AppConfig::load(p).unwrap_or_default();
            let root = cfg.domains_root();
            match wpa_engine::domain::ensure_default(&root) {
                Ok(files) if !files.is_empty() => {
                    println!("已写出默认领域模板：{}（{}）", root.join("default").display(), files.join("、"))
                }
                Ok(_) => println!("默认领域模板已存在：{}", root.join("default").display()),
                Err(e) => println!("默认领域模板写出失败：{e}"),
            }
            println!("\n下一步：");
            println!("  windpencilagent-cli check                      # 配置自检");
            println!("  windpencilagent-cli domain show                # 看默认领域");
            println!("  windpencilagent-cli registry export            # 生成工具清单");
            println!("  windpencilagent-cli                            # 进入对话");
            return Ok(());
        }
        Some(Cmd::Check) => {
            let cfg = AppConfig::load(&cli.config).map_err(|e| anyhow::anyhow!("{e}"))?;
            let errs = cfg.validate();
            // 授权范围/拒绝清单条目自检：写错的条目在装配期会被安全处理（见 ScopeGuard），
            // 但用户需要在"改配置"这一步就看到问题，而不是等调工具时被拒。
            let (_, scope_notes) = kern_core::ScopeGuard::from_lists_lenient(
                &cfg.scope.authorized,
                &cfg.scope.denylist,
            );
            if !scope_notes.is_empty() {
                println!("授权范围提示（{} 条）：", scope_notes.len());
                for n in &scope_notes {
                    println!("  ! {n}");
                }
            }
            if errs.is_empty() {
                println!("配置有效：{}", cli.config.display());
                return Ok(());
            }
            println!("配置存在 {} 个问题：", errs.len());
            for e in &errs {
                println!("  ✗ {e}");
            }
            std::process::exit(2);
        }
        _ => {}
    }

    let cfg = AppConfig::load(&cli.config).map_err(|e| anyhow::anyhow!("{e}"))?;

    match cli.cmd {
        Some(Cmd::Init) | Some(Cmd::Check) => unreachable!("已前置处理"),
        Some(Cmd::Tools) => {
            let base = assemble(&cfg, None).await?;
            print_tools(&base);
            base.shutdown().await;
            Ok(())
        }
        Some(Cmd::NewPlugin { id, group, name, summary, risk, root }) => {
            new_plugin(&cfg, &id, &group, name.as_deref(), &summary, &risk, root)
        }
        Some(Cmd::Call { tool, args }) => {
            let base = assemble(&cfg, None).await?;
            let code = call_tool(&base, &tool, &args).await;
            base.shutdown().await;
            code
        }
        Some(Cmd::Domain { cmd }) => match cmd {
            DomainCmd::List => domain_list(&cfg),
            DomainCmd::Show { id } => domain_show(&cfg, id.as_deref()),
            DomainCmd::SetGroups { ids, id } => domain_set_groups(&cfg, &ids, id.as_deref()),
        },
        Some(Cmd::Registry { cmd }) => match cmd {
            RegistryCmd::Export { domain, out, check } => {
                registry_export(&cfg, domain, out, check).await
            }
            RegistryCmd::List => registry_list(&cfg),
            RegistryCmd::Install { id } => registry_act(&cfg, &id, RegOp::Install),
            RegistryCmd::Uninstall { id } => registry_act(&cfg, &id, RegOp::Uninstall),
            RegistryCmd::Enable { id } => registry_act(&cfg, &id, RegOp::Enable),
            RegistryCmd::Disable { id } => registry_act(&cfg, &id, RegOp::Disable),
            RegistryCmd::Move { id, group } => registry_move(&cfg, &id, &group),
        },
        Some(Cmd::Run { objective, targets }) => {
            let base = assemble(&cfg, None).await?;
            let code = run_once(base, objective, targets, cli.yes).await?;
            std::process::exit(code);
        }
        Some(Cmd::Chat { targets }) => {
            let base = assemble(&cfg, None).await?;
            repl(base, cfg, targets, cli.yes).await
        }
        None => {
            let base = assemble(&cfg, None).await?;
            repl(base, cfg, vec![], cli.yes).await
        }
    }
}
