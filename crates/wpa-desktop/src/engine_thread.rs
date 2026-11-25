//! 引擎线程：UI 与引擎的唯一通道。
//!
//! 线程模型（K-2 风险的落地）：
//! - **UI 线程**只做渲染与事件转发（Slint 事件循环）；
//! - **引擎线程**独占 tokio runtime、注册表、会话与存储，永远不阻塞 UI；
//! - 双向通信：UI→引擎 走 `std::sync::mpsc`（命令），引擎→UI 走
//!   `slint::invoke_from_event_loop`（数据推送）。
//!
//! 审批（HITL）是个例外：会话内的审批回调是**同步阻塞**的，因此引擎线程在这里
//! 阻塞等 UI 线程裁决（`recv_timeout`），超时按拒绝处理（fail-closed）。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{ModelRc, VecModel};

use agent_core::{ChatEvent, ChatSession};
use kern_core::model::{AgentId, ExecutionContext, RiskLevel, SessionId, TaskId};
use wpa_engine::config::{AppConfig, ConfigSection};
use wpa_engine::{assemble, build_session, Base, SessionInfo, SessionOpts};

use crate::{
    AppWindow, CapGroupRow, CfgCard, CfgItem, ChatMsg, DayRow, DomainCardRow, DomainGroupChip,
    RegEntryRow, RegGroupRow,
    ModelRow, SpaceRow, TaskRow, ToolRow, TraceRow,
};

/// UI → 引擎 命令。
pub enum Cmd {
    Send(String),
    NewSession,
    SwitchSpace(String),
    CreateSpace { name: String, note: String },
    RemoveSpace(String),
    UseModel(String),
    TestModel(String),
    LoadMemory(String),
    AppendDaily(String),
    ToolTest(String),
    /// 树包启停（写注册表；装配在下次启动生效）
    /// 重新装配（T-6 热装换：会话边界重读树注册表与清单）
    ReloadTree,
    OpenConfig,
    SetTheme(bool),
    SetFont(u32),
    /// 保存授权范围（一行一条规则；写回 `[scope] authorized`）
    SetScope(Vec<String>),
    /// 修改内置能力配置项（key, value）：校验 → 写盘 → 按分区决定热更新或提示重启
    SetField { key: String, value: String },
    /// 能力页选中条目：按名字查快照，把详情回填给界面（界面不做循环过滤）
    PickTool(String),
    /// 能力页注册表操作：(id, op)，写 registry.toml 后重新装配
    RegOp { id: String, op: String },
    /// 领域页选中某个领域 → 下发该领域的组 chips
    DomainPick(usize),
    /// 领域页勾选/取消一个组（只改内存集合）
    DomainToggleGroup(String),
    /// 领域页保存组引用（写 domain.toml + 重新装配）
    DomainSaveGroups,
    /// 读档案并回填编辑表单
    EditModel(String),
    /// 新建（清空表单）
    NewModel,
    /// 保存档案（新增或覆盖同名），写 config.toml 后重新装配
    SaveModel {
        name: String,
        driver: String,
        base_url: String,
        model: String,
        api_key: String,
        ctx: String,
        tools_ok: bool,
        vision: bool,
        enabled: bool,
    },
    /// 删除档案
    DeleteModel(String),
    /// 切换领域：写配置 + **重新装配**（领域影响 system prompt / 工具面 / 约束，
    /// 不是热参数，光重建会话不够 —— 必须重跑 assemble）。
    SwitchDomain(String),
    Refresh,
    Shutdown,
}

/// 供 UI 线程持有的句柄。
pub struct UiHandle {
    pub tx: Sender<Cmd>,
    /// 审批裁决通道（引擎侧阻塞等待）
    pub approval: Arc<Mutex<Option<Sender<bool>>>>,
    /// 引擎线程句柄：窗口关闭后 join，确保配置与索引落盘再退出。
    join: Option<std::thread::JoinHandle<()>>,
}

impl UiHandle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// 关闭流程：发 Shutdown 并等待引擎线程收尾。
    ///
    /// 不能只发消息就返回 —— `run()` 返回后进程立即退出，会把还在写配置的
    /// 引擎线程一起杀掉（配置文件被截断）。
    pub fn join(mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }

    /// UI 按下"放行/拒绝"：把裁决送回阻塞中的审批回调。
    pub fn answer(&self, allow: bool) {
        if let Ok(mut slot) = self.approval.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(allow);
            }
        }
    }
}

/// 启动引擎线程，返回命令句柄。
pub fn spawn(cfg_path: PathBuf, window: slint::Weak<AppWindow>) -> UiHandle {
    let (tx, rx) = mpsc::channel();
    let approval = Arc::new(Mutex::new(None));
    let approval_thread = approval.clone();
    let join = std::thread::Builder::new()
        .name("wpa-engine".into())
        .spawn(move || {
            if let Err(e) = run_engine(cfg_path, window, rx, approval_thread) {
                tracing::error!("引擎线程退出：{e}");
            }
        })
        .expect("启动引擎线程失败");
    UiHandle {
        tx,
        approval,
        join: Some(join),
    }
}

// ---------------------------------------------------------------------------
// UI 更新辅助
// ---------------------------------------------------------------------------

fn ui_set(window: &slint::Weak<AppWindow>, f: impl FnOnce(&AppWindow) + Send + 'static) {
    let w = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = w.upgrade() {
            f(&app);
        }
    });
}

fn toast(window: &slint::Weak<AppWindow>, text: impl Into<String>) {
    let text: String = text.into();
    ui_set(window, move |app| {
        app.set_toast_text(text.into());
        app.set_toast_open(true);
    });
}

fn set_busy(window: &slint::Weak<AppWindow>, busy: bool) {
    ui_set(window, move |app| app.set_busy(busy));
}

fn push_trace(
    window: &slint::Weak<AppWindow>,
    buf: &Arc<Mutex<Vec<TraceRow>>>,
    icon: &str,
    title: impl Into<String>,
    status: &str,
) {
    let (data, _) = {
        let mut v = match buf.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        v.push(TraceRow {
            icon: icon.into(),
            title: title.into().into(),
            status: status.into(),
        });
        if v.len() > 240 {
            v.remove(0);
        }
        (v.clone(), ())
    };
    ui_set(window, move |app| {
        app.set_traces(ModelRc::new(VecModel::from(data)));
    });
}

fn push_msg(
    window: &slint::Weak<AppWindow>,
    buf: &Arc<Mutex<Vec<ChatMsg>>>,
    role: &str,
    text: impl Into<String>,
) {
    let data = {
        let mut v = match buf.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        v.push(ChatMsg {
            role: role.into(),
            text: text.into().into(),
        });
        v.clone()
    };
    ui_set(window, move |app| {
        app.set_messages(ModelRc::new(VecModel::from(data)));
    });
}

// ---------------------------------------------------------------------------
// 快照推送
// ---------------------------------------------------------------------------

struct UiData {
    messages: Arc<Mutex<Vec<ChatMsg>>>,
    traces: Arc<Mutex<Vec<TraceRow>>>,
}

impl UiData {
    fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            traces: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// 配置卡片模型：完全由 `wpa_engine::cards()` 驱动，界面不写死参数名。
///
/// 新增一个可调参数时只改配置元数据（字段 + Default + validate + cards），
/// 这里与 `.slint` 都不用动 —— 这是"所有参数都在卡片里"的结构性保证。
fn build_cfg_cards(cfg: &AppConfig) -> Vec<CfgCard> {
    wpa_engine::cards()
        .into_iter()
        .map(|c| {
            let items: Vec<CfgItem> = c
                .fields
                .iter()
                .map(|f| {
                    // 候选项：Enum 在编译期就确定；Pick/Action 要读运行期状态
                    // （领域目录、已有档案、内置预设），所以在建卡片时解析一次。
                    let options: Vec<slint::SharedString> = match &f.kind {
                        wpa_engine::FieldKind::Enum { options } => {
                            options.iter().map(|o| (*o).into()).collect()
                        }
                        wpa_engine::FieldKind::Pick { from }
                        | wpa_engine::FieldKind::Action { from } => {
                            from.options(cfg).into_iter().map(Into::into).collect()
                        }
                        _ => Vec::new(),
                    };
                    CfgItem {
                        key: f.key.into(),
                        label: f.label.into(),
                        help: f.help.into(),
                        kind: match &f.kind {
                            wpa_engine::FieldKind::Bool => "bool",
                            wpa_engine::FieldKind::Int { .. } => "int",
                            wpa_engine::FieldKind::Float { .. } => "float",
                            wpa_engine::FieldKind::Secret => "secret",
                            wpa_engine::FieldKind::Path { .. } => "path",
                            wpa_engine::FieldKind::Enum { .. } => "enum",
                            wpa_engine::FieldKind::Pick { .. } => "pick",
                            wpa_engine::FieldKind::Action { .. } => "action",
                            wpa_engine::FieldKind::List => "list",
                            wpa_engine::FieldKind::Text => "text",
                        }
                        .into(),
                        value: cfg.get_field(f.key).unwrap_or_default().into(),
                        options: ModelRc::new(VecModel::from(options)),
                        restart: !f.section.is_hot(),
                    }
                })
                .collect();
            CfgCard {
                id: c.id.into(),
                title: c.title.into(),
                desc: c.desc.into(),
                page: c.page.id().into(),
                items: ModelRc::new(VecModel::from(items)),
            }
        })
        .collect()
}

/// 按配置二级页分组后的卡片模型（`page` 是编译期元数据，界面不做过滤）。
///
/// 为什么在 Rust 侧分组而不是界面里 `for + if`：本版 Slint 不支持
/// `for c in cards : if cond : Component` 的写法（实测解析报错），
/// 而给每张卡包一层 Rectangle 会破坏 VerticalLayout 的流式排列。
fn cards_of(all: &[CfgCard], page: &str) -> ModelRc<CfgCard> {
    ModelRc::new(VecModel::from(
        all.iter().filter(|c| c.page.as_str() == page).cloned().collect::<Vec<_>>(),
    ))
}

/// 把 11 张卡按 5 个归属页一次性下发（配置 4 页 + 领域页内嵌）。
fn publish_cfg_cards(app: &AppWindow, cfg: &AppConfig) {
    let all = build_cfg_cards(cfg);
    app.set_cfg_cards_model(cards_of(&all, "model"));
    app.set_cfg_cards_capability(cards_of(&all, "capability"));
    app.set_cfg_cards_security(cards_of(&all, "security"));
    app.set_cfg_cards_data(cards_of(&all, "data"));
    app.set_cfg_cards_domain(cards_of(&all, "domain"));
}

impl Engine {
    /// 下发领域页的组 chips：全部基座组 + 内存编辑态的勾选标记。
    ///
    /// 编辑态为空（还没选中领域 / 刚保存）时按"选中领域的声明"显示，不标 dirty。
    fn push_domain_chips(&self, snap: &wpa_engine::Snapshot, dirty: bool) {
        let declared: Vec<String> = self
            .domain_edit
            .as_ref()
            .map(|(_, g)| g.clone())
            .unwrap_or_default();
        let chips: Vec<DomainGroupChip> = snap
            .cap_groups
            .iter()
            .map(|g| DomainGroupChip {
                id: g.id.clone().into(),
                label: g.label.clone().into(),
                declared: declared.iter().any(|x| x == &g.id),
            })
            .collect();
        ui_set(&self.window, move |app| {
            app.set_domain_chips(ModelRc::new(VecModel::from(chips)));
            app.set_domain_dirty(dirty);
        });
    }
}


/// 取 base 当前快照里的领域列表（首个快照调用点使用）。
fn snap_domains_of(base: &Base) -> Option<wpa_engine::DomainCardRow> {
    base.snapshot().domains.into_iter().find(|d| d.active)
}

fn apply_snapshot(base: &Base, window: &slint::Weak<AppWindow>) {
    // 领域页组 chips 首次初始化：按激活领域的声明显示（未进入编辑态）
    if let Some(d) = snap_domains_of(base) {
        let chips: Vec<DomainGroupChip> = base
            .snapshot()
            .cap_groups
            .iter()
            .map(|g| DomainGroupChip {
                id: g.id.clone().into(),
                label: g.label.clone().into(),
                declared: d.groups.iter().any(|x| x == &g.id),
            })
            .collect();
        ui_set(window, move |app| {
            app.set_domain_chips(ModelRc::new(VecModel::from(chips)));
        });
    }

    let snap = base.snapshot();

    let tools: Vec<ToolRow> = snap
        .tools
        .iter()
        .map(|t| ToolRow {
            name: t.name.clone().into(),
            group: t.group.clone().into(),
            risk: t.risk.clone().into(),
            desc: t.description.clone().into(),
        })
        .collect();
    let spaces: Vec<SpaceRow> = snap
        .spaces
        .iter()
        .map(|s| SpaceRow {
            name: s.name.clone().into(),
            active: s.active,
            scope: s.scope as i32,
            plugins: s.plugins as i32,
            note: s.note.clone().into(),
        })
        .collect();
    let isolated: Vec<slint::SharedString> =
        snap.isolated.iter().map(|s| s.clone().into()).collect();
    // 知识/技能：技能按名字+触发场景列出（列表接口只有 group id，太糊）；
    // 知识页仍按群组 id 展示（知识提供者不暴露条目数，避免误导）。
    // 知识条目改由领域 provider 提供，快照里不再按契约群组列举
    let knowledge: Vec<slint::SharedString> = Vec::new();
    let skills: Vec<slint::SharedString> = snap
        .skills
        .iter()
        .map(|s| {
            if s.trigger.is_empty() {
                s.name.clone().into()
            } else {
                format!("{}（{}）", s.name, s.trigger).into()
            }
        })
        .collect();

    let models: Vec<ModelRow> = base
        .models()
        .iter()
        .map(|m| ModelRow {
            name: m.name.clone().into(),
            model: m.model.clone().into(),
            ctx: m.context_window as i32,
            tools_ok: m.tools_ok,
            active: m.active,
        })
        .collect();

    // 任务板
    let all = base.tasks.visible();
    let pairs: Vec<(u64, i32)> = all.iter().map(|r| (r.id, r.status)).collect();
    let tasks: Vec<TaskRow> = all
        .iter()
        .filter_map(|r| {
            let col = r.board_col(&pairs)?;
            Some(TaskRow {
                title: r.subject.clone().into(),
                status: match col {
                    0 => "待规划",
                    1 => "待办",
                    2 => "进行中",
                    _ => "已完成",
                }
                .into(),
                col: col as i32,
            })
        })
        .collect();
    let counts = base.board_counts();

    // 记忆
    let days: Vec<DayRow> = base
        .memory_dates()
        .into_iter()
        .map(|d| {
            let chars = base.memory_daily(&d).chars().count() as i32;
            DayRow { date: d.into(), chars }
        })
        .collect();
    let curated = base.memory_curated();

    let status_line = snap.status_line();
    let warnings = snap.warnings.join(" · ");
    let ctx_window = base
        .models()
        .iter()
        .find(|m| m.active)
        .map(|m| m.context_window as i32)
        .unwrap_or(32_000);

    let tool_count = snap.tool_count as i32;
    let scope_text = snap.scope_rules.join("\n");
    // 卡片模型含 ModelRc（单线程类型），只能在 UI 线程内构建 ⇒ 把配置快照搬进闭包
    let cfg_for_cards = base.cfg.clone();
    let domain_name = snap.domain.clone();
    ui_set(window, move |app| {
        app.set_tools(ModelRc::new(VecModel::from(tools)));
        // 领域行含 ModelRc（!Send），只能在 UI 线程里构建
        let domains: Vec<DomainCardRow> = snap
            .domains
            .iter()
            .map(|d| DomainCardRow {
                id: d.id.clone().into(),
                name: d.name.clone().into(),
                kind: d.kind.clone().into(),
                summary: d.summary.clone().into(),
                groups: ModelRc::new(VecModel::from(
                    d.groups
                        .iter()
                        .map(|g| slint::SharedString::from(g.clone()))
                        .collect::<Vec<_>>(),
                )),
                tools: d.tools as i32,
                knowledge_entries: d.knowledge_entries as i32,
                has_workflow: d.has_workflow,
                active: d.active,
                error: d.error.clone().into(),
            })
            .collect();
        app.set_domains(ModelRc::new(VecModel::from(domains)));
        // 能力组是纯数据（无 ModelRc），可以在闭包外先建；这里就地映射保持一处可读
        let cap_groups: Vec<CapGroupRow> = snap
            .cap_groups
            .iter()
            .map(|g| CapGroupRow {
                id: g.id.clone().into(),
                label: g.label.clone().into(),
                source: g.source.clone().into(),
                tools: g.tools as i32,
                declared: g.declared,
            })
            .collect();
        app.set_cap_groups(ModelRc::new(VecModel::from(cap_groups)));

        // 注册表视图（含插件与未安装条目）
        let reg_groups: Vec<RegGroupRow> = snap
            .reg_groups
            .iter()
            .map(|g| RegGroupRow {
                id: g.id.clone().into(),
                label: g.label.clone().into(),
                source: g.source.clone().into(),
                enabled: g.enabled,
                total: g.total as i32,
                active: g.active as i32,
                broken: g.broken as i32,
                discovered: g.discovered as i32,
            })
            .collect();
        let reg_entries: Vec<RegEntryRow> = snap
            .reg_entries
            .iter()
            .map(|e| RegEntryRow {
                id: e.id.clone().into(),
                group: e.group.clone().into(),
                name: e.name.clone().into(),
                source: e.source.clone().into(),
                risk: e.risk.clone().into(),
                status: e.status.clone().into(),
                enabled: e.enabled,
                installed: e.installed,
                group_enabled: e.group_enabled,
                note: e.note.clone().into(),
            })
            .collect();
        app.set_reg_groups(ModelRc::new(VecModel::from(reg_groups)));
        app.set_reg_entries(ModelRc::new(VecModel::from(reg_entries)));
        app.set_reg_summary(snap.reg_summary.as_str().into());
        app.set_spaces(ModelRc::new(VecModel::from(spaces)));
        app.set_models(ModelRc::new(VecModel::from(models)));
        app.set_tasks(ModelRc::new(VecModel::from(tasks)));
        app.set_memory_days(ModelRc::new(VecModel::from(days)));
        app.set_isolated(ModelRc::new(VecModel::from(isolated)));
        app.set_knowledge_names(ModelRc::new(VecModel::from(knowledge)));
        app.set_skill_names(ModelRc::new(VecModel::from(skills)));
        app.set_tool_count(tool_count);
        app.set_c0(counts.planned as i32);
        app.set_c1(counts.todo as i32);
        app.set_c2(counts.doing as i32);
        app.set_c3(counts.done as i32);
        app.set_curated_text(curated.into());
        app.set_status_line(
            if warnings.is_empty() { status_line } else { format!("{status_line}　·　{warnings}") }.into(),
        );
        app.set_model_name(snap.model.into());
        app.set_space_name(snap.space.into());
        app.set_ctx_window(ctx_window);
        publish_cfg_cards(app, &cfg_for_cards);
        app.set_domain_name(domain_name.into());
        app.set_data_root(snap.data_root.into());
        app.set_version(snap.version.into());
        // 输入框已有内容（用户正在改）就不覆盖，避免刷新把未保存的编辑冲掉
        if app.get_scope_text().trim().is_empty() {
            app.set_scope_text(scope_text.into());
        }
    });
}

// ---------------------------------------------------------------------------
// 引擎状态
// ---------------------------------------------------------------------------

struct Engine {
    rt: tokio::runtime::Runtime,
    base: Base,
    cfg: AppConfig,
    cfg_path: PathBuf,
    window: slint::Weak<AppWindow>,
    /// 领域页的组绑定编辑态：Some((领域 id, 组集合))。勾选只改这里，保存才落盘。
    domain_edit: Option<(String, Vec<String>)>,
    session: Option<ChatSession>,
    info: Option<SessionInfo>,
    ui: UiData,
    approval: Arc<Mutex<Option<Sender<bool>>>>,
    /// 本进程累计用量（状态条展示；空间级持久记账另有 record_usage）
    tokens_in: u64,
    tokens_out: u64,
}

fn run_engine(
    cfg_path: PathBuf,
    window: slint::Weak<AppWindow>,
    rx: Receiver<Cmd>,
    approval: Arc<Mutex<Option<Sender<bool>>>>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let cfg = match AppConfig::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            toast(&window, e);
            return Ok(());
        }
    };

    // 首屏主题（配置里的外观域）
    {
        let dark = cfg.app.theme == "dark";
        let font = cfg.app.font_size as i32;
        ui_set(&window, move |app| {
            app.set_is_dark(dark);
            app.set_font_size(font);
        });
    }

    let base = match rt.block_on(assemble(&cfg, None)) {
        Ok(b) => b,
        Err(e) => {
            toast(&window, format!("装配失败：{e}"));
            return Ok(());
        }
    };

    let mut engine = Engine {
        domain_edit: None,
        tokens_in: 0,
        tokens_out: 0,
        rt,
        base,
        cfg,
        cfg_path,
        window: window.clone(),
        session: None,
        info: None,
        ui: UiData::new(),
        approval,
    };

    apply_snapshot(&engine.base, &window);
    engine.ensure_session();
    if let Some(info) = engine.info.clone() {
        engine.apply_info(&info);
    }

    // 命令循环
    while let Ok(cmd) = rx.recv() {
        if matches!(cmd, Cmd::Shutdown) {
            break;
        }
        engine.handle(cmd);
    }

    engine.rt.block_on(engine.base.shutdown());
    Ok(())
}

impl Engine {
    /// T-6 热装换：在**会话边界**重读树注册表与清单，重建基座与会话。
    ///
    /// 为什么不在会话中途换：工具面、约束钩子、阶段状态都挂在会话上，
    /// 中途替换会让"上一轮用过的工具这一轮消失"，行为不可复盘。换会话才是干净的边界。
    fn reload_tree(&mut self) {
        // 1) 先装配新的：失败则原样保留（绝不让用户落到"基座已关、树没装上"的死状态）
        let new_base = match self.rt.block_on(assemble(&self.cfg, None)) {
            Ok(b) => b,
            Err(e) => {
                toast(&self.window, format!("重新装配失败（保持原状）：{e}"));
                return;
            }
        };
        // 2) 丢旧会话（释放对旧注册表的引用）→ 关旧基座 → 换新
        self.session = None;
        self.info = None;
        self.rt.block_on(self.base.shutdown());
        self.base = new_base;

        // 3) 新会话 + 刷新界面
        self.ensure_session();
        apply_snapshot(&self.base, &self.window);
        toast(&self.window, format!("已重新装配：{} 个工具", self.base.snapshot().tool_count));
    }

    fn apply_info(&self, info: &SessionInfo) {
        let model = if info.model.is_empty() { "未配置模型".to_string() } else { info.model.clone() };
        let space = if info.space.is_empty() { "全局".to_string() } else { info.space.clone() };
        let ctx = info.context_window as i32;
        let warnings = info.warnings.join(" · ");
        ui_set(&self.window, move |app| {
            app.set_model_name(model.into());
            app.set_space_name(space.into());
            app.set_ctx_window(ctx);
            if !warnings.is_empty() {
                app.set_toast_text(warnings.into());
                app.set_toast_open(true);
            }
        });
    }

    /// 确保会话存在（空间/模型变更后重建）。
    fn ensure_session(&mut self) {
        if self.session.is_some() {
            return;
        }
        let window = self.window.clone();
        let ev_window = window.clone();
        let ev_buf = self.ui.traces.clone();
        let events: agent_core::ChatEventCallback = Arc::new(move |ev: ChatEvent| {
            let (icon, title, status): (&str, String, &str) = match ev {
                ChatEvent::RoundStarted { round } => ("↻", format!("第 {round} 轮推理"), "run"),
                ChatEvent::AssistantNote { text, .. } => {
                    ("…", clip(&text, 120), "run")
                }
                ChatEvent::AssistantDelta { .. } => return,
                ChatEvent::ToolStarted { tool, args_summary } => {
                    ("→", format!("{tool} {}", clip(&args_summary, 90)), "run")
                }
                ChatEvent::ToolFinished { tool, ok, summary, duration_ms, .. } => (
                    if ok { "✓" } else { "✗" },
                    format!("{tool} {}ms · {}", duration_ms, clip(&summary, 80)),
                    if ok { "ok" } else { "err" },
                ),
                ChatEvent::ApprovalRequested { tool, risk, .. } => {
                    ("!", format!("{tool} 需要审批（{risk}）"), "ask")
                }
                ChatEvent::ApprovalDenied { tool } => ("✗", format!("{tool} 审批被拒"), "err"),
                ChatEvent::ApprovalDecision { tool, allowed, via } => (
                    "·",
                    format!("{tool} 裁决={} via={via}", if allowed { "放行" } else { "拒绝" }),
                    "detail",
                ),
                ChatEvent::MemoryCompressed { compressed, kept } => (
                    "↯",
                    format!("上下文压缩：{compressed} → {kept} 条"),
                    "detail",
                ),
                ChatEvent::ExperienceSaved { count } => {
                    ("✦", format!("抽取经验 {count} 条"), "detail")
                }
            };
            push_trace(&ev_window, &ev_buf, icon, title, status);
        });

        // 审批回调：同步阻塞等 UI 裁决
        let ap_slot = self.approval.clone();
        let ap_window = window.clone();
        let auto = self.cfg.run.auto_approve;
        let timeout = self.cfg.run.approval_timeout_secs.unwrap_or(300);
        let approval: agent_core::ApprovalCallback =
            Arc::new(move |tool: &str, risk: RiskLevel, args: &str| -> bool {
                if auto {
                    return true;
                }
                let (tx, rx) = mpsc::channel::<bool>();
                if let Ok(mut slot) = ap_slot.lock() {
                    *slot = Some(tx);
                }
                let tool_s = tool.to_string();
                let risk_s = format!("{risk:?}");
                let args_s = clip(args, 600);
                let w = ap_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = w.upgrade() {
                        app.set_approval_tool(tool_s.into());
                        app.set_approval_risk(risk_s.into());
                        app.set_approval_args(args_s.into());
                        app.set_approval_open(true);
                    }
                });
                let allow = rx.recv_timeout(Duration::from_secs(timeout)).unwrap_or(false);
                let w = ap_window.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = w.upgrade() {
                        app.set_approval_open(false);
                    }
                });
                allow
            });

        let opts = SessionOpts {
            targets: Vec::new(),
            approval,
            events: Some(events),
            auto_approve: self.cfg.run.auto_approve,
        };
        let (session, info) = build_session(&self.base, opts);
        self.session = Some(session);
        self.info = Some(info);
    }

    /// 热更新配置后重建会话（保留历史与轮次）。
    ///
    /// 不能只改 `self.cfg` —— 会话里的 ChatConfig 是构建时快照，
    /// 改了配置不重建等于没改，而用户会以为"保存成功了却没生效"。
    fn rebuild_session(&mut self) {
        let (snap, turn) = match &self.session {
            Some(s) => (s.state_snapshot(), s.turn_no()),
            None => return,
        };
        self.session = None;
        // 网络参数是进程级单例（见 net-core::settings）：会话重建时一并刷新，
        // 否则「配置→内置能力」里改的超时/UA/代理只对下一次启动生效。
        wpa_engine::apply_net_settings(&self.cfg);
        self.ensure_session();
        if let Some(s) = self.session.as_mut() {
            s.restore_full(snap, turn);
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Send(text) => self.send(text),
            Cmd::NewSession => {
                self.session = None;
                self.ensure_session();
                if let Ok(mut v) = self.ui.messages.lock() {
                    v.clear();
                }
                ui_set(&self.window, |app| {
                    app.set_messages(ModelRc::new(VecModel::from(Vec::new())));
                    app.set_traces(ModelRc::new(VecModel::from(Vec::new())));
                });
                if let Ok(mut v) = self.ui.traces.lock() {
                    v.clear();
                }
                if let Some(info) = self.info.clone() {
                    self.apply_info(&info);
                }
                toast(&self.window, "已新建会话");
            }
            Cmd::SwitchSpace(name) => {
                match self.base.workspaces.switch(&name) {
                    Ok(w) => {
                        self.session = None;
                        self.ensure_session();
                        apply_snapshot(&self.base, &self.window);
                        if let Some(info) = self.info.clone() {
                            self.apply_info(&info);
                        }
                        toast(&self.window, format!("已切换到空间 {}", w.name));
                    }
                    Err(e) => toast(&self.window, format!("切换失败：{e}")),
                }
            }
            Cmd::CreateSpace { name, note } => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    toast(&self.window, "空间名不能为空");
                    return;
                }
                let ws = workspace_core::Workspace {
                    name: name.clone(),
                    note,
                    ..Default::default()
                };
                match self.base.workspaces.create(ws) {
                    Ok(_) => {
                        apply_snapshot(&self.base, &self.window);
                        ui_set(&self.window, |app| {
                            app.set_new_space_name("".into());
                            app.set_new_space_note("".into());
                        });
                        toast(&self.window, format!("空间 {name} 已创建"));
                    }
                    Err(e) => toast(&self.window, format!("创建失败：{e}")),
                }
            }
            Cmd::RemoveSpace(name) => match self.base.workspaces.remove(&name) {
                Ok(true) => {
                    self.session = None;
                    self.ensure_session();
                    apply_snapshot(&self.base, &self.window);
                    toast(&self.window, format!("空间 {name} 已移除（目录保留）"));
                }
                Ok(false) => toast(&self.window, "空间不存在"),
                Err(e) => toast(&self.window, format!("移除失败：{e}")),
            },
            Cmd::UseModel(name) => {
                self.cfg.llm.active = Some(name.clone());
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Llm]) {
                    Ok(cs) => {
                        // llm 域是热更新域：重建会话即可让新档案与降级链生效
                        self.session = None;
                        self.ensure_session();
                        apply_snapshot(&self.base, &self.window);
                        if let Some(info) = self.info.clone() {
                            self.apply_info(&info);
                        }
                        toast(&self.window, format!("已启用模型档案 {name} · {}", cs.describe()));
                    }
                    Err(e) => toast(&self.window, format!("保存配置失败：{e}")),
                }
            }
            Cmd::NewModel => {
                ui_set(&self.window, |app| {
                    app.set_md_new(true);
                    app.set_md_open(true);
                    app.set_md_name("".into());
                    app.set_md_driver("openai".into());
                    app.set_md_url("https://api.openai.com/v1".into());
                    app.set_md_model("".into());
                    app.set_md_key("".into());
                    app.set_md_ctx("32000".into());
                    app.set_md_tools(true);
                    app.set_md_vision(false);
                    app.set_md_enabled(true);
                });
            }
            Cmd::EditModel(name) => {
                let hit = self
                    .cfg
                    .llm
                    .profiles
                    .iter()
                    .find(|p| p.name == name)
                    .cloned();
                let Some(p) = hit else {
                    toast(&self.window, format!("档案 {name} 不存在（刷新后再试）"));
                    return;
                };
                let ctx = p.context_window.unwrap_or(32000).to_string();
                ui_set(&self.window, move |app| {
                    app.set_md_new(false);
                    app.set_md_open(true);
                    app.set_md_name(p.name.clone().into());
                    app.set_md_driver(p.driver.clone().into());
                    app.set_md_url(p.base_url.clone().into());
                    app.set_md_model(p.model.clone().into());
                    app.set_md_key("".into());
                    app.set_md_ctx(ctx.into());
                    app.set_md_tools(p.supports_tools.unwrap_or(true));
                    app.set_md_vision(p.supports_vision.unwrap_or(false));
                    app.set_md_enabled(p.enabled);
                });
            }
            Cmd::SaveModel {
                name,
                driver,
                base_url,
                model,
                api_key,
                ctx,
                tools_ok,
                vision,
                enabled,
            } => {
                if name.trim().is_empty() || model.trim().is_empty() || base_url.trim().is_empty() {
                    toast(&self.window, "档案名 / base_url / 模型名 都不能为空");
                    return;
                }
                let ctx_n: u32 = ctx.trim().parse().unwrap_or(32000).clamp(1024, 2_000_000);
                // 留空 = 保留原密钥（编辑时不强制重填）
                let keep_key = api_key.trim().is_empty();
                let existing = self
                    .cfg
                    .llm
                    .profiles
                    .iter()
                    .find(|p| p.name == name)
                    .cloned();
                let profile = base_llm::LlmProfile {
                    name: name.clone(),
                    driver: if driver.trim().is_empty() {
                        "openai".to_string()
                    } else {
                        driver.trim().to_string()
                    },
                    base_url: base_url.trim().to_string(),
                    model: model.trim().to_string(),
                    api_key: if keep_key {
                        existing.and_then(|p| p.api_key)
                    } else {
                        Some(api_key.trim().to_string())
                    },
                    temperature: None,
                    max_tokens: None,
                    enabled,
                    mode: "agent".to_string(),
                    context_window: Some(ctx_n),
                    supports_tools: Some(tools_ok),
                    supports_vision: Some(vision),
                    cost_per_1k_in: None,
                    cost_per_1k_out: None,
                };
                match self
                    .cfg
                    .llm
                    .profiles
                    .iter_mut()
                    .find(|p| p.name == name)
                {
                    Some(slot) => *slot = profile,
                    None => self.cfg.llm.profiles.push(profile),
                }
                if self.cfg.llm.active.is_none() {
                    self.cfg.llm.active = Some(name.clone());
                }
                let msg = match self.cfg.commit(&self.cfg_path, &[ConfigSection::Llm]) {
                    Ok(cs) => cs.describe(),
                    Err(e) => {
                        toast(&self.window, format!("保存失败：{e}"));
                        return;
                    }
                };
                ui_set(&self.window, |app| app.set_md_open(false));
                self.reload_tree();
                toast(&self.window, format!("{msg}；档案 {name} 已保存"));
            }
            Cmd::DeleteModel(name) => {
                let before = self.cfg.llm.profiles.len();
                self.cfg.llm.profiles.retain(|p| p.name != name);
                if self.cfg.llm.profiles.len() == before {
                    toast(&self.window, format!("档案 {name} 不存在"));
                    return;
                }
                if self.cfg.llm.active.as_deref() == Some(name.as_str()) {
                    self.cfg.llm.active = self.cfg.llm.profiles.first().map(|p| p.name.clone());
                }
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Llm]) {
                    Ok(cs) => {
                        self.reload_tree();
                        toast(&self.window, format!("{}；档案 {name} 已删除", cs.describe()));
                    }
                    Err(e) => toast(&self.window, format!("删除后保存失败：{e}")),
                }
            }
            Cmd::TestModel(name) => {
                let profile = self
                    .cfg
                    .llm
                    .profiles
                    .iter()
                    .find(|p| p.name == name)
                    .cloned();
                let Some(profile) = profile else {
                    toast(&self.window, "未找到该模型档案");
                    return;
                };
                let window = self.window.clone();
                toast(&window, format!("正在测试 {name} …"));
                match base_llm::build_provider(profile.to_provider_config()) {
                    Ok(p) => {
                        let r = self.rt.block_on(base_llm::probe_provider(p.as_ref()));
                        match r {
                            Ok(text) => toast(
                                &window,
                                format!("{name} 连通正常：{}", clip(&text, 60)),
                            ),
                            Err(e) => toast(&window, format!("{name} 失败：{}", clip(&e, 120))),
                        }
                    }
                    Err(e) => toast(&window, format!("{name} 初始化失败：{e}")),
                }
            }
            Cmd::LoadMemory(date) => {
                let text = self.base.memory_daily(&date);
                ui_set(&self.window, move |app| {
                    app.set_daily_date(date.into());
                    app.set_daily_text(text.into());
                });
            }
            Cmd::AppendDaily(text) => {
                let space = self.base.workspaces.active_name().unwrap_or_default();
                if space.is_empty() {
                    toast(&self.window, "无活跃空间：请先在「空间」页切换或创建");
                    return;
                }
                let content = if text.trim().is_empty() {
                    // 空文本 = 把最近一条助手答复写入日志
                    self.ui
                        .messages
                        .lock()
                        .ok()
                        .and_then(|v| {
                            v.iter()
                                .rev()
                                .find(|m| m.role == "assistant")
                                .map(|m| m.text.to_string())
                        })
                        .unwrap_or_default()
                } else {
                    text
                };
                if content.trim().is_empty() {
                    toast(&self.window, "没有可写入的内容");
                    return;
                }
                match self.base.workspaces.append_memory_daily(&space, &content) {
                    Ok(path) => {
                        apply_snapshot(&self.base, &self.window);
                        toast(&self.window, format!("已写入 {path}"));
                    }
                    Err(e) => toast(&self.window, format!("写入失败：{e}")),
                }
            }
            Cmd::ToolTest(name) => {
                let args = if name == "util.echo" {
                    serde_json::json!({"text": "基座自检"})
                } else {
                    serde_json::json!({})
                };
                let ctx = ExecutionContext {
            scope: Vec::new(),
                    session_id: SessionId::new(),
                    agent_id: AgentId::new("ui"),
                    task_id: TaskId::new("ui-smoke"),
                    target: String::new(),
                    risk: RiskLevel::Low,
                    approved: true,
                };
                // 走执行器而不是直接取组件：执行器负责解析工具名、套用风险表与
                // 隔离区规则，UI 自检与模型调用因此共用同一条路径。
                let out = self.rt.block_on(self.base.executor.execute(
                    &ctx,
                    std::slice::from_ref(&name),
                    args,
                ));
                let summary = if out.ok { "成功" } else { "失败" };
                toast(
                    &self.window,
                    format!("{name} {summary}：{}", clip(&out.summary, 90)),
                );
                push_trace(
                    &self.window,
                    &self.ui.traces,
                    if out.ok { "✓" } else { "✗" },
                    format!("{name} {summary} · {}", clip(&out.summary, 80)),
                    if out.ok { "ok" } else { "err" },
                );
            }
            Cmd::OpenConfig => {
                // 配置文件不存在则落一份示例；随后在应用内提示路径（不拉起外部程序）
                if !self.cfg_path.exists() {
                    let _ = std::fs::write(&self.cfg_path, wpa_engine::config::SAMPLE_CONFIG);
                }
                let p = self.cfg_path.display().to_string();
                toast(&self.window, format!("配置文件：{p}"));
            }
            Cmd::SetTheme(dark) => {
                self.cfg.app.theme = if dark { "dark".into() } else { "light".into() };
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Theme]) {
                    Ok(cs) => {
                        // 主题是热更新域：Theme.is_dark 一改，palette 立即重算
                        ui_set(&self.window, move |app| app.set_is_dark(dark));
                        toast(&self.window, cs.describe());
                    }
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }
            Cmd::SetFont(size) => {
                self.cfg.app.font_size = size;
                // 字号散落在各处硬编码 px 上，只能重启生效 —— 明说，别让用户以为已变
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::App]) {
                    Ok(cs) => toast(&self.window, cs.describe()),
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }
            Cmd::ReloadTree => self.reload_tree(),
            Cmd::SetField { key, value } => {
                let old_theme = self.cfg.app.theme.clone();
                let old_font = self.cfg.app.font_size;
                if let Err(e) = self.cfg.set_field(&key, &value) {
                    toast(&self.window, format!("{key} 写入失败：{e}"));
                    return;
                }
                // 校验不通过就不落盘（把校验消息原样给出，而不是"保存成功"）
                let errs = self.cfg.validate();
                if !errs.is_empty() {
                    toast(&self.window, format!("未保存：{}", errs.join("；")));
                    return;
                }
                let section = wpa_engine::field(&key)
                    .map(|f| f.section)
                    .unwrap_or(ConfigSection::App);
                match self.cfg.commit(&self.cfg_path, &[section]) {
                    Ok(cs) => {
                        // 热更新：主题/字号当场生效（其余分区靠重建会话或重启）
                        if self.cfg.app.theme != old_theme || self.cfg.app.font_size != old_font {
                            let dark = self.cfg.app.theme == "dark";
                            let font = self.cfg.app.font_size as i32;
                            ui_set(&self.window, move |app| {
                                app.set_is_dark(dark);
                                app.set_font_size(font);
                            });
                        }
                        // 改了模型/上下文/循环参数：重建会话让新值立刻生效
                        if section.is_hot() {
                            self.rebuild_session();
                        }
                        ui_set(&self.window, move |app| {
                            app.set_cfg_note(cs.describe().into());
                        });
                        // 卡片要重建：改了激活档案/领域后，其他行的"当前值"和候选项都会变
                        // （例如选了预设之后，模型名、地址、窗口大小都应该刷成新档案的值）。
                        // 注意卡片模型不是 Send：只能在 UI 线程闭包里构建，所以把配置
                        // 拷一份进去，而不是在外面建好再传。
                        let cfg_snapshot = self.cfg.clone();
                        ui_set(&self.window, move |app| publish_cfg_cards(app, &cfg_snapshot));
                        // 装了模型但界面上一眼看不出效果：把状态栏与工具面刷新一遍
                        if let Some(info) = self.info.clone() {
                            self.apply_info(&info);
                        }
                        apply_snapshot(&self.base, &self.window);
                    }
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }
            Cmd::SwitchDomain(id) => {
                if id == self.cfg.domain.active {
                    toast(&self.window, format!("已经是领域 {id}"));
                    return;
                }
                let old = self.cfg.domain.active.clone();
                self.cfg.domain.active = id.clone();
                // 先校验（领域目录不存在/domain.toml 写错在装配期会降级成"无领域"，
                // 提前拦住比让用户看到"切了但没生效"更好）
                let errs = self.cfg.validate();
                if !errs.is_empty() {
                    self.cfg.domain.active = old;
                    toast(&self.window, format!("未切换：{}", errs.join("；")));
                    return;
                }
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Domain]) {
                    Ok(_) => {
                        // 领域不是热参数：重跑装配（与树包启停同一条路径）
                        self.reload_tree();
                        let cfg_snapshot = self.cfg.clone();
                        ui_set(&self.window, move |app| publish_cfg_cards(app, &cfg_snapshot));
                        if let Some(info) = self.info.clone() {
                            self.apply_info(&info);
                        }
                        apply_snapshot(&self.base, &self.window);
                        toast(&self.window, format!("已切换到领域 {id}（已重新装配）"));
                    }
                    Err(e) => {
                        self.cfg.domain.active = old;
                        toast(&self.window, format!("保存失败：{e}"));
                    }
                }
            }
            Cmd::SetScope(rules) => {
                let rules: Vec<String> = rules
                    .into_iter()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                // 先校验（与执行层同一套判据），把问题摆到用户面前再决定是否落盘
                let notes = wpa_engine::scope_notes(&rules, &self.cfg.scope.denylist);
                self.cfg.scope.authorized = rules.clone();
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Scope]) {
                    Ok(_) => {
                        let msg = if notes.is_empty() {
                            format!(
                                "授权范围已保存（{} 条）；新会话生效，当前会话仍用旧范围",
                                rules.len()
                            )
                        } else {
                            format!("已保存，但有问题：{}", notes.join("；"))
                        };
                        let for_ui = msg.clone();
                        ui_set(&self.window, move |app| app.set_scope_note(for_ui.clone().into()));
                        toast(&self.window, msg);
                    }
                    Err(e) => {
                        let msg = format!("保存失败：{e}");
                        ui_set(&self.window, {
                            let msg = msg.clone();
                            move |app| app.set_scope_note(msg.clone().into())
                        });
                        toast(&self.window, msg);
                    }
                }
            }
            Cmd::PickTool(id) => {
                let snap = self.base.snapshot();
                let hit = snap.reg_entries.iter().find(|e| e.id == id).cloned();
                let (name, group, source, risk, status, note, installed, enabled) = match hit {
                    Some(e) => (
                        e.name, e.group, e.source, e.risk, e.status, e.note, e.installed, e.enabled,
                    ),
                    None => (
                        String::new(), String::new(), String::new(), String::new(),
                        String::new(), String::new(), false, false,
                    ),
                };
                ui_set(&self.window, move |app| {
                    app.set_ab_id(id.into());
                    app.set_ab_name(name.into());
                    app.set_ab_group(group.into());
                    app.set_ab_source(source.into());
                    app.set_ab_risk(risk.into());
                    app.set_ab_status(status.into());
                    app.set_ab_note(note.into());
                    app.set_ab_installed(installed);
                    app.set_ab_enabled(enabled);
                });
            }
            Cmd::RegOp { id, op } => {
                // "move:<组id>" = 组↔条目绑定（换归属不动开关）
                if let Some(to_group) = op.strip_prefix("move:") {
                    let path = wpa_engine::registry::registry_path(&self.base.cfg.data_root());
                    let mut reg = match self.base.registry.write() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    let (snap, _) = wpa_engine::registry::scan_plugins(&self.base.cfg.plugins_root());
                    reg.merge_scan(&snap);
                    let msg = match reg.move_entry(&id, to_group) {
                        Some((from, to)) if from == to => {
                            format!("{id} 已经在组 {to} 里")
                        }
                        Some((from, to)) => match reg.save(&path) {
                            Ok(()) => {
                                drop(reg);
                                self.reload_tree();
                                format!("{id}：{from} → {to}（已重新装配）")
                            }
                            Err(e) => format!("移动后保存失败：{e}"),
                        },
                        None => format!("注册表里没有条目 `{id}`"),
                    };
                    toast(&self.window, msg);
                    return;
                }
                let path = wpa_engine::registry::registry_path(&self.base.cfg.data_root());
                let msg = {
                    let mut reg = match self.base.registry.write() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    let is_group = reg.group(&id).is_some();
                    let out = match (op.as_str(), is_group) {
                        ("install", true) => {
                            let n = reg.install_group(&id);
                            format!("组 {id} 已安装：{n} 个条目")
                        }
                        ("install", false) => {
                            if reg.set_entry_enabled(&id, true) {
                                format!("条目 {id} 已启用")
                            } else {
                                format!("条目 {id} 状态未变化")
                            }
                        }
                        ("uninstall", true) => {
                            let n = reg.uninstall_group(&id);
                            format!("组 {id} 已卸载：{n} 个条目转为候选")
                        }
                        ("uninstall", false) => {
                            if reg.set_entry_enabled(&id, false) {
                                format!("条目 {id} 已卸载（记录保留）")
                            } else {
                                format!("条目 {id} 状态未变化")
                            }
                        }
                        ("enable", _) => {
                            if reg.set_entry_enabled(&id, true) {
                                format!("条目 {id} 已启用")
                            } else {
                                format!("条目 {id} 已是启用状态")
                            }
                        }
                        ("disable", _) => {
                            if reg.set_entry_enabled(&id, false) {
                                format!("条目 {id} 已禁用")
                            } else {
                                format!("条目 {id} 已是禁用状态")
                            }
                        }
                        ("group-on", _) => {
                            if reg.set_group_enabled(&id, true) {
                                format!("组 {id} 已启用")
                            } else {
                                format!("组 {id} 已是启用状态")
                            }
                        }
                        ("group-off", _) => {
                            if reg.set_group_enabled(&id, false) {
                                format!("组 {id} 已停用")
                            } else {
                                format!("组 {id} 已是停用状态")
                            }
                        }
                        (other, _) => format!("未知操作：{other}"),
                    };
                    match reg.save(&path) {
                        Ok(()) => out,
                        Err(e) => format!("{out}（但保存失败：{e}）"),
                    }
                };
                self.reload_tree();
                toast(&self.window, msg);
            }
            Cmd::DomainPick(i) => {
                let snap = self.base.snapshot();
                let Some(d) = snap.domains.get(i) else {
                    toast(&self.window, format!("领域索引 {i} 不存在（快照 {} 个）", snap.domains.len()));
                    return;
                };
                self.domain_edit = Some((d.id.clone(), d.groups.clone()));
                self.push_domain_chips(&snap, false);
            }
            Cmd::DomainToggleGroup(gid) => {
                let snap = self.base.snapshot();
                let Some((_, groups)) = &mut self.domain_edit else {
                    toast(&self.window, "先在左侧选中一个领域");
                    return;
                };
                match groups.iter().position(|x| *x == *gid) {
                    Some(i) => {
                        groups.remove(i);
                    }
                    None => groups.push(gid.clone()),
                }
                self.push_domain_chips(&snap, true);
            }
            Cmd::DomainSaveGroups => {
                let Some((id, groups)) = self.domain_edit.clone() else {
                    toast(&self.window, "先在左侧选中一个领域");
                    return;
                };
                let file = self.base.cfg.domains_root().join(&id).join("domain.toml");
                match wpa_engine::domain::set_groups(&file, &groups) {
                    Ok(()) => {
                        self.reload_tree();
                        self.domain_edit = None;
                        toast(&self.window, format!("领域 {id} 的组引用已保存（{} 个组）", groups.len()));
                    }
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }
            Cmd::Refresh => {
                apply_snapshot(&self.base, &self.window);
                toast(&self.window, "已刷新");
            }
            Cmd::Shutdown => {}
        }
    }

    fn send(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.ensure_session();
        let Some(session) = self.session.as_mut() else {
            toast(&self.window, "会话未就绪");
            return;
        };

        push_msg(&self.window, &self.ui.messages, "user", text.clone());
        set_busy(&self.window, true);
        push_trace(&self.window, &self.ui.traces, "›", clip(&text, 100), "detail");

        let started = std::time::Instant::now();
        let turn = self.rt.block_on(session.turn(&text));
        let elapsed = started.elapsed().as_millis() as u64;

        set_busy(&self.window, false);

        let reply = if turn.reply.trim().is_empty() {
            if turn.ok { "（无输出）".to_string() } else { "（执行失败）".to_string() }
        } else {
            turn.reply.clone()
        };
        push_msg(
            &self.window,
            &self.ui.messages,
            if turn.ok { "assistant" } else { "system" },
            reply.clone(),
        );

        // 用量记账（空间级）
        self.base
            .workspaces
            .record_usage(turn.prompt_tokens, turn.completion_tokens, elapsed);

        // 用量累计（本轮 + 历史）
        self.tokens_in += turn.prompt_tokens;
        self.tokens_out += turn.completion_tokens;
        // 上下文占用：用会话自己的历史估算，**不是** 本轮的 prompt_tokens。
        // 一轮里 N 次工具调用会把 prompt_tokens 累加 N 次，拿它算占比能显示到 200%+。
        let ctx = self
            .info
            .as_ref()
            .map(|i| i.context_window.max(1))
            .unwrap_or(32_000);
        let used = self
            .session
            .as_ref()
            .map(|s| s.context_tokens())
            .unwrap_or(0);
        let ratio = (used as f32 / ctx as f32).clamp(0.0, 1.0);
        let (tin, tout) = (self.tokens_in, self.tokens_out);
        ui_set(&self.window, move |app| {
            app.set_ctx_ratio(ratio);
            app.set_ctx_used(used as i32);
            app.set_ctx_pct(ratio);
            app.set_tokens_in(tin as i32);
            app.set_tokens_out(tout as i32);
            app.set_status_line(format!("最近一轮：{used} tok 上下文 · {elapsed} ms").into());
        });

        apply_snapshot(&self.base, &self.window);
    }
}

fn clip(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max { format!("{t}…") } else { t }
}
