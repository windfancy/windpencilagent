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

use slint::{Model, ModelRc, VecModel};

use agent_core::{ChatEvent, ChatSession};
use kern_core::model::{AgentId, ExecutionContext, RiskLevel, SessionId, TaskId};
use wpa_engine::config::{AppConfig, ConfigSection};
use wpa_engine::{assemble, build_session, Base, SessionInfo, SessionOpts};

use crate::{
    AppWindow, CapGroupRow, CfgCard, CfgItem, ChatMsg, DayRow, DomainCardRow, DomainGroupChip,
    KbStoreRow, RegEntryRow, RegGroupRow, KnowledgeRow, SessionRow, DomainExtRow,
    ModelRow, SpaceRow, TaskRow, ToolRow, TraceRow, FileRow,
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
    /// 文本输入草稿：只改内存配置，不写盘 —— 等「保存配置」按钮统一落盘。
    /// 逐键写盘会让每次击键都跑校验/重建会话，且非法中间值（如 "-"）会被当场回滚。
    DraftField { key: String, value: String },
    /// 「保存配置」：全量校验 → 写盘 → 同步装配快照 → 刷新界面。
    SaveConfig,
    /// 空间编辑：点「编辑」时初始化组勾选 chips
    SpacePick(String),
    /// 在工作区根打开资源管理器（让「空间目录」可直达）
    OpenSpaceDir(String),
    /// 初始化空间工作区骨架：scripts/、output/、AGENTS.md、空间记忆目录
    InitSpaceDir(String),
    /// 空间编辑：勾选/取消一个工具组（按组授权，替代手输白名单）
    SpaceToggleGroup(String),
    /// 审批「本轮会话生效」：把工具加入当前会话白名单
    ApprovalGrantSession(String),
    /// 审批「加入空间授权白名单」：参数=审批参数摘要（含目标 JSON），解析目标写入当前空间 scope 并持久化
    ApprovalGrantSpace(String),
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
    /// 领域页：加一个私有外部工具（写 domain.toml 的 `[[external]]`）
    DomainExtAdd { kind: String, path: String, usage: String },
    /// 领域页：移除第 n 条外部工具
    DomainExtRemove(usize),
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

    // ---- 本轮 20 条改动新增命令 ----
    /// #8 复制消息到剪贴板
    CopyMessage(String),
    /// #11 任务看板 CRUD
    TaskAdd(String),
    TaskMove { id: String, col: i32 },
    TaskDelete(String),
    /// #9 空间编辑保存（name, cwd, whitelist 多行, permission: global/unrestricted, sandbox: on/off）
    SpaceSave {
        name: String,
        cwd: String,
        whitelist: String,
        permission: String,
        sandbox: String,
        /// 高风险：domain / on / off
        high_risk: String,
    },
    /// #9.1 会话管理（参数=空间过滤，空串=全部）
    SessionList(String),
    /// 只读预览：拉一条会话的消息到会话页右侧面板，**不动当前对话**
    SessionView(String),
    /// 真正切换当前会话（会话页「继续此会话」/ 召回后才用）
    SessionOpen(String),
    SessionDelete(String),
    SessionExport(String),
    SessionRecall(String),
    /// #7.1 领域 CRUD + 文件编辑
    DomainCreate { id: String, name: String },
    DomainRemove(String),
    DomainFileRead { id: String, file: String },
    DomainFileSave { id: String, file: String, text: String },
    /// 领域概要保存（≤300 字）
    DomainSummarySave { id: String, summary: String },
    /// 按概要调用大模型生成四份配置并替换（原文件先 .bak 备份）。
    /// 带 summary = 用编辑框里**当前**的文本（不必先保存也能生成）。
    DomainAiGenerate { id: String, summary: String },
    /// #7.2 知识管理
    KnowledgeAdd { name: String, text: String },
    KnowledgeRemove(String),
    /// #13.1/#16 路径浏览：把选到的路径写入配置字段（key, path）
    SetFieldPath { key: String, path: String },

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
        // 自动滚屏由 Slint 侧哨兵 + ensure-visible 完成（见 RightRail）
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
        // 自动滚屏由 Slint 侧哨兵 + ensure-visible 完成（见 ChatPage）
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
                // 领域页不再渲染「激活领域」按钮：状态栏的领域 chip 与详情页
                // 「切换到此领域」已经覆盖同一操作，重复一排按钮只会挤占界面。
                // 字段本身仍留在 schema 里（CLI / config set 照旧可用）。
                .filter(|f| !(c.page.id() == "domain" && f.key == "domain.active"))
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
                        // Slider 用 options[0]=min、options[1]=max 传范围给 UI
                        wpa_engine::FieldKind::Slider { min, max } => {
                            vec![min.to_string().into(), max.to_string().into()]
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
                            wpa_engine::FieldKind::Slider { .. } => "slider",
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
                        // 路径字段的文件/目录方向由 schema 的 Path { dir } 决定，
                        // 不再在 UI 里写死 true（python/pip 应选 .exe 文件）。
                        path_is_dir: match &f.kind {
                            wpa_engine::FieldKind::Path { dir } => *dir,
                            _ => true,
                        },
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
    /// 下发空间页的工具组勾选 chips（可配置组 + 当前空间已选）。
    ///
    /// 基础组（core/shell/sandbox/space/memory/kb）不在这里出现：它们永远可用，
    /// 列出来只会让用户以为"没勾就没有"，从而把系统勾坏。
    fn push_space_group_chips(&self) {
        let snap = self.base.snapshot();
        let chips = build_group_chips(&snap, &self.space_group_edit);
        let whitelist = self.space_group_edit.join("\n");
        ui_set(&self.window, move |app| {
            app.set_space_group_chips(ModelRc::new(VecModel::from(chips)));
            app.set_edit_whitelist(whitelist.into());
        });
    }

    /// 配置写盘后，把最新配置同步进装配期 Base 快照。
    ///
    /// 根因修复：`Base.cfg` 是 `assemble` 时的快照，而 `apply_snapshot` 用它下发布尔/
    /// 数值卡片、`base.models()` 用它算模型列表、`build_session` 用它构建会话。
    /// 若只改 `self.cfg` 不回灌，界面会被 `apply_snapshot` 用旧值二次覆盖（看起来"改了没保存"），
    /// 且会话/模型切换读的还是旧快照（看起来"要重启才生效"）。写盘成功后统一回灌即可。
    fn sync_base_cfg(&mut self) {
        self.base.cfg = self.cfg.clone();
        // 向量库的 embedding 档案也跟着换：否则切换模型后 kb.vec.* 还在调旧档案
        self.base.embedder.update_from(&self.cfg);
    }

    /// 下发领域页的组 chips：**只列可配置组** + 内存编辑态的勾选标记。
    ///
    /// 基础组是系统内部前提（永远可用），既不显示也不写进 domain.toml，
    /// 否则用户会以为"不勾就没有"而把它们删掉，装出来的工具面直接缺胳膊少腿。
    ///
    /// 编辑态为空（还没选中领域 / 刚保存）时按"选中领域的声明"显示，不标 dirty。
    fn push_domain_chips(&self, snap: &wpa_engine::Snapshot, dirty: bool) {
        let declared: Vec<String> = self
            .domain_edit
            .as_ref()
            .map(|(_, g)| g.clone())
            .unwrap_or_default();
        let chips = build_group_chips(snap, &declared);
        ui_set(&self.window, move |app| {
            app.set_domain_chips(ModelRc::new(VecModel::from(chips)));
            app.set_domain_dirty(dirty);
        });
    }
}


/// 组 chips 的统一数据源：**全局注册表里的组**（含插件组），排除基础组与「未分配」。
///
/// 为什么不用 `cap_groups`：那是 `catalog::CAP_GROUPS` 静态表，只有内置组，
/// 插件组永远列不出来 —— 领域自然也就无法引用插件组，插件工具必然被收敛掉。
///
/// 候选组（扫到但一个都没装）不列：引用一个空组没有意义，只会让 chips 越滚越长；
/// 组装上东西或启用后会自动出现。
fn build_group_chips(
    snap: &wpa_engine::Snapshot,
    selected: &[String],
) -> Vec<DomainGroupChip> {
    snap.reg_groups
        .iter()
        .filter(|g| !wpa_engine::catalog::is_base_group(&g.id))
        .filter(|g| g.id != wpa_engine::registry::UNASSIGNED_GROUP)
        .filter(|g| g.enabled || g.total > 0)
        .map(|g| DomainGroupChip {
            id: g.id.clone().into(),
            label: g.label.clone().into(),
            declared: selected.iter().any(|x| x == &g.id),
            enabled: g.enabled,
        })
        .collect()
}

/// 把绝对路径写成相对领域目录的形式（在领域目录内时）。
///
/// 为什么坚持相对路径：领域目录整体搬走/改名后，绝对路径就全废了；而
/// `domain.toml` 是要跟着领域包走的文件。领域目录之外的路径只能存绝对路径。
fn relativize(domain_dir: &std::path::Path, path: &str) -> String {
    let p = std::path::Path::new(path.trim());
    if !p.is_absolute() {
        return path.trim().to_string();
    }
    // 大小写不敏感比较：Windows 上用户从对话框拿到的盘符大小写可能与配置不同
    let norm = |s: &str| s.replace('\\', "/").to_lowercase();
    let base = norm(&domain_dir.to_string_lossy());
    let full = norm(&p.to_string_lossy());
    if let Some(rest) = full.strip_prefix(&format!("{base}/")) {
        // 用原路径的切片恢复真实大小写，避免把用户路径写成全小写
        let raw = p.to_string_lossy().replace('\\', "/");
        return raw[raw.len() - rest.len()..].to_string();
    }
    path.trim().to_string()
}

fn apply_snapshot(base: &Base, window: &slint::Weak<AppWindow>) {
    let snap = base.snapshot();
    // 领域页组 chips 首次初始化：按激活领域的声明显示（未进入编辑态）
    if let Some(d) = snap.domains.iter().find(|d| d.active) {
        let chips = build_group_chips(&snap, &d.groups);
        ui_set(window, move |app| {
            app.set_domain_chips(ModelRc::new(VecModel::from(chips)));
        });
    }

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
    let isolated: Vec<slint::SharedString> =
        snap.isolated.iter().map(|s| s.clone().into()).collect();
    // 知识/技能：技能按名字+触发场景列出；知识条目由当前领域 provider 提供（#7.2）
    // 必须显式传 knowledge.md：provider 的 entries 参数是"要加载哪些文件"，
    // 传空数组 = 一个文件都不读，列表永远是 0 条（这正是之前的 bug）。
    let knowledge_entries: Vec<String> = {
        let dir = base.cfg.active_domain_dir();
        let provider = knowledge_core::FileKnowledgeProvider::from_dir(
            "domain",
            &dir,
            &["knowledge.md".to_string()],
            None,
        );
        provider.list_entries()
    };
    let knowledge: Vec<slint::SharedString> = knowledge_entries.iter().map(|s| s.clone().into()).collect();
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
                id: r.id.to_string().into(),
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
    let model_names: Vec<slint::SharedString> = base
        .models()
        .iter()
        .map(|m| m.name.clone().into())
        .collect();
    let degraded = snap.warnings.iter().any(|w| w.contains("降级"));
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
                // 知识库行含 ModelRc（数组字段），必须在 UI 线程里构造
                kb: ModelRc::new(VecModel::from(
                    d.kb
                        .iter()
                        .map(|k| KbStoreRow {
                            kind: k.kind.clone().into(),
                            label: k.label.clone().into(),
                            location: k.location.clone().into(),
                            purpose: k.purpose.clone().into(),
                            entries: k.entries as i32,
                            error: k.error.clone().into(),
                        })
                        .collect::<Vec<_>>(),
                )),
                // 领域私有外部工具（index 用于"移除第 n 条"）
                external: ModelRc::new(VecModel::from(
                    d.external
                        .iter()
                        .map(|e| DomainExtRow {
                            index: e.index as i32,
                            kind: e.kind.clone().into(),
                            name: e.name.clone().into(),
                            path: e.path.clone().into(),
                            usage: e.usage.clone().into(),
                            exists: e.exists,
                            tools: ModelRc::new(VecModel::from(
                                e.tools
                                    .iter()
                                    .map(|t| slint::SharedString::from(t.clone()))
                                    .collect::<Vec<_>>(),
                            )),
                            error: e.error.clone().into(),
                        })
                        .collect::<Vec<_>>(),
                )),
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
        // #3：内置基础组（内核/沙箱/命令与作业/工作空间/记忆检索）不参与插件工具管理，
        // 从能力页左栏隐藏；file/net/text 及插件/未分配组仍保留。
        let reg_groups: Vec<RegGroupRow> = snap
            .reg_groups
            .iter()
            .filter(|g| wpa_engine::snapshot::is_managed_group(&g.id))
            .map(|g| RegGroupRow {
                id: g.id.clone().into(),
                label: g.label.clone().into(),
                source: g.source.clone().into(),
                description: g.description.clone().into(),
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
            .filter(|e| wpa_engine::snapshot::is_managed_group(&e.group))
            .map(|e| RegEntryRow {
                id: e.id.clone().into(),
                group: e.group.clone().into(),
                name: e.name.clone().into(),
                source: e.source.clone().into(),
                risk: e.risk.clone().into(),
                status: e.status.clone().into(),
                enabled: e.enabled,
                installed: e.installed,
                installable: e.installable,
                group_enabled: e.group_enabled,
                note: e.note.clone().into(),
            })
            .collect();
        app.set_reg_groups(ModelRc::new(VecModel::from(reg_groups)));
        app.set_reg_entries(ModelRc::new(VecModel::from(reg_entries)));
        app.set_reg_summary(snap.reg_summary.as_str().into());
        // 空间行含 ModelRc（文件列表，!Send），只能在 UI 线程里构建
        let spaces: Vec<SpaceRow> = snap
            .spaces
            .iter()
            .map(|s| SpaceRow {
                name: s.name.clone().into(),
                active: s.active,
                plugins: s.plugins as i32,
                note: s.note.clone().into(),
                cwd: s.cwd.clone().into(),
                root: s.root.clone().into(),
                files: ModelRc::new(VecModel::from(
                    s.files
                        .iter()
                        .map(|f| FileRow {
                            name: f.name.clone().into(),
                            is_dir: f.is_dir,
                            size: f.size.clone().into(),
                        })
                        .collect::<Vec<_>>(),
                )),
                file_total: s.file_total as i32,
                whitelist: s.whitelist.join("\n").into(),
                permission: s.permission.clone().into(),
                sandbox: s.sandbox.clone().into(),
                high_risk: s.high_risk.clone().into(),
            })
            .collect();
        app.set_spaces(ModelRc::new(VecModel::from(spaces)));
        app.set_models(ModelRc::new(VecModel::from(models)));
        // 任务看板四列：把同一份任务按 col 拆成四份下发（Slint 侧不做过滤）
        let col0: Vec<TaskRow> = tasks.iter().filter(|t| t.col == 0).cloned().collect();
        let col1: Vec<TaskRow> = tasks.iter().filter(|t| t.col == 1).cloned().collect();
        let col2: Vec<TaskRow> = tasks.iter().filter(|t| t.col == 2).cloned().collect();
        let col3: Vec<TaskRow> = tasks.iter().filter(|t| t.col == 3).cloned().collect();
        app.set_tasks(ModelRc::new(VecModel::from(tasks)));
        app.set_col0(ModelRc::new(VecModel::from(col0)));
        app.set_col1(ModelRc::new(VecModel::from(col1)));
        app.set_col2(ModelRc::new(VecModel::from(col2)));
        app.set_col3(ModelRc::new(VecModel::from(col3)));
        // 对话页选择器候选（空间名 / 领域名 / 模型名）
        // 注意：闭包是 'static，不能捕获 base 引用——模型名在闭包外用 owned 值算好
        app.set_space_options(ModelRc::new(VecModel::from(
            snap.spaces.iter().map(|s| s.name.clone().into()).collect::<Vec<slint::SharedString>>(),
        )));
        app.set_domain_options(ModelRc::new(VecModel::from(
            snap.domains.iter().map(|d| d.name.clone().into()).collect::<Vec<slint::SharedString>>(),
        )));
        app.set_model_options(ModelRc::new(VecModel::from(model_names.clone())));
        app.set_degraded(degraded);
        app.set_memory_days(ModelRc::new(VecModel::from(days)));
        app.set_isolated(ModelRc::new(VecModel::from(isolated)));
        app.set_knowledge_names(ModelRc::new(VecModel::from(knowledge)));
        app.set_knowledge(ModelRc::new(VecModel::from(
            knowledge_entries
                .iter()
                .map(|n| KnowledgeRow { name: n.clone().into(), excerpt: "".into() })
                .collect::<Vec<_>>(),
        )));
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
    /// 空间页的工具组勾选编辑态：当前已选组 id 集合（对应 SpaceRow.whitelist）
    space_group_edit: Vec<String>,
    session: Option<ChatSession>,
    info: Option<SessionInfo>,
    ui: UiData,
    approval: Arc<Mutex<Option<Sender<bool>>>>,
    /// 本进程累计用量（状态条展示；空间级持久记账另有 record_usage）
    tokens_in: u64,
    tokens_out: u64,
}

/// 毫秒时间戳 → 本地时间字符串（会话列表用）。
fn format_time(ms: u64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
    let secs = (ms / 1000) as i64;
    // 用标准库把 UTC 秒转成日期时间，避免引入 chrono 依赖
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// _days since 1970-01-01 → (year, month, day)，Howard Hinnant 的算法（无依赖）。
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
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
        space_group_edit: Vec::new(),
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
        // #1 决策：降级只在对话框顶部条显示"降级"标记，不再用 toast 弹整段警告
        // （那正是"点预设/改输入框就弹警告"的观感来源）。其余诊断仍留在状态栏。
        let degraded = info.warnings.iter().any(|w| w.contains("降级"));
        ui_set(&self.window, move |app| {
            app.set_model_name(model.into());
            app.set_space_name(space.into());
            app.set_ctx_window(ctx);
            app.set_degraded(degraded);
        });
    }

    /// #4 回填能力页选中条目的详情（安装/卸载后也要立即刷新，不能等重新点选）
    fn push_ab_detail(&self, id: &str) {
        let snap = self.base.snapshot();
        let hit = snap.reg_entries.iter().find(|e| e.id == id).cloned();
        let (name, group, source, risk, status, note, installed, installable, enabled) = match hit {
            Some(e) => (
                e.name,
                e.group,
                e.source,
                e.risk,
                e.status,
                e.note,
                e.installed,
                e.installable,
                e.enabled,
            ),
            None => (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                false,
                false,
                false,
            ),
        };
        let id = id.to_string();
        ui_set(&self.window, move |app| {
            app.set_ab_id(id.into());
            app.set_ab_name(name.into());
            app.set_ab_group(group.into());
            app.set_ab_source(source.into());
            app.set_ab_risk(risk.into());
            app.set_ab_status(status.into());
            app.set_ab_note(note.into());
            app.set_ab_installed(installed);
            app.set_ab_installable(installable);
            app.set_ab_enabled(enabled);
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
        // 空间级审批覆盖（#14）：完全访问空间或全局自动批准 → 直接放行
        let auto = self
            .base
            .workspaces
            .active()
            .map(|w| w.auto_approve())
            .unwrap_or(false)
            || self.cfg.run.auto_approve;
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
            auto_approve: self
                .base
                .workspaces
                .active()
                .map(|w| w.auto_approve())
                .unwrap_or(false)
                || self.cfg.run.auto_approve,
        };
        let (mut session, info) = build_session(&self.base, opts);
        // 会话归属：落盘时写入 SessionRecord，会话列表据此按空间分类
        let space = self
            .base
            .workspaces
            .active()
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let profile = self.cfg.llm.active.clone().unwrap_or_default();
        session.set_persist_identity(space, profile);
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
                        // 工作区变了：file 工具根、shell 默认 cwd、AGENTS.md、空间白名单
                        // 全在装配期决定 ⇒ 必须重新装配，只换会话会让工具仍指向旧目录。
                        self.reload_tree();
                        toast(&self.window, format!("已切换到空间 {}（工作区已重载）", w.name));
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
                let old = self.cfg.llm.active.clone();
                self.cfg.llm.active = Some(name.clone());
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Llm]) {
                    Ok(cs) => {
                        // 关键：先把新配置回灌进 Base 快照，再重建会话/刷新界面。
                        // 否则 build_session 读 base.cfg 旧值、模型列表也被旧快照覆盖，
                        // 表现为"切了模型要重启才生效"。
                        self.sync_base_cfg();
                        self.session = None;
                        self.ensure_session();
                        apply_snapshot(&self.base, &self.window);
                        if let Some(info) = self.info.clone() {
                            self.apply_info(&info);
                        }
                        toast(&self.window, format!("已启用模型档案 {name} · {}", cs.describe()));
                    }
                    Err(e) => {
                        self.cfg.llm.active = old;
                        toast(&self.window, format!("保存配置失败：{e}"));
                    }
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
                let old_value = self.cfg.get_field(&key).unwrap_or_default();
                if let Err(e) = self.cfg.set_field(&key, &value) {
                    toast(&self.window, format!("{key} 写入失败：{e}"));
                    return;
                }
                // #12 局部校验：只校验当前正在改的字段，不再因整表别处的错误
                // 卡住这一次输入（那会让"改任意输入框"都弹出无关警告）。
                let errs = self.cfg.validate_field(&key);
                if !errs.is_empty() {
                    // 回滚这一次写入，避免非法值留在内存里
                    let _ = self.cfg.set_field(&key, &old_value);
                    toast(&self.window, format!("未保存：{}", errs.join("；")));
                    return;
                }
                let section = wpa_engine::field(&key)
                    .map(|f| f.section)
                    .unwrap_or(ConfigSection::App);
                match self.cfg.commit(&self.cfg_path, &[section]) {
                    Ok(cs) => {
                        // 关键：先把新配置回灌进 Base 快照。apply_snapshot 用它下发布尔/数值
                        // 卡片，build_session 用它构建会话 —— 不回灌就会被旧值二次覆盖。
                        self.sync_base_cfg();
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
            Cmd::DraftField { key, value } => {
                // 只写内存草稿，不写盘、不重建会话、不 toast —— 逐字输入不该每次击键
                // 都跑一遍校验/会话重建。点「保存配置」时再统一校验落盘。
                if let Err(e) = self.cfg.set_field(&key, &value) {
                    // 非法中间值（例如刚敲了个 "-"）：静默保留 UI 里的文本，等保存时报错
                    let _ = e;
                }
                ui_set(&self.window, |app| {
                    app.set_cfg_note("● 有未保存的修改，点「保存配置」生效".into());
                });
            }
            Cmd::SaveConfig => {
                let errs = self.cfg.validate();
                if !errs.is_empty() {
                    toast(&self.window, format!("未保存：{}", errs.join("；")));
                    return;
                }
                match self.cfg.commit(&self.cfg_path, &ConfigSection::ALL) {
                    Ok(cs) => {
                        self.sync_base_cfg();
                        // 热域改动重建会话，让新值下一轮就生效
                        self.rebuild_session();
                        apply_snapshot(&self.base, &self.window);
                        if let Some(info) = self.info.clone() {
                            self.apply_info(&info);
                        }
                        ui_set(&self.window, move |app| app.set_cfg_note(cs.describe().into()));
                        toast(&self.window, "配置已保存到 config.toml");
                    }
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }
            Cmd::SpacePick(name) => {
                let Some(w) = self.base.workspaces.get(&name) else { return };
                // 已有白名单：组 id 直接保留；精确工具名也保留（chips 只显示组）
                self.space_group_edit = w.plugins.clone();
                self.push_space_group_chips();
            }
            Cmd::SpaceToggleGroup(gid) => {
                match self.space_group_edit.iter().position(|x| x == &gid) {
                    Some(i) => { self.space_group_edit.remove(i); }
                    None => self.space_group_edit.push(gid.clone()),
                }
                self.push_space_group_chips();
            }
            Cmd::ApprovalGrantSession(tool) => {
                if let Some(s) = self.session.as_mut() {
                    s.allow_tool(&tool);
                }
                toast(&self.window, format!("工具 {tool} 已加入本轮会话白名单"));
            }
            Cmd::ApprovalGrantSpace(text) => {
                // 从审批参数摘要里提取目标（target/host/url），写入当前空间 scope 并持久化。
                // 提取失败时退回整段文本，由用户后续在配置页修正。
                let target = {
                    let mut found = String::new();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        for k in ["target", "host", "url", "domain", "ip"] {
                            if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                                found = s.to_string();
                                break;
                            }
                        }
                    }
                    if found.is_empty() {
                        text.split('｜').next().unwrap_or(&text).trim().to_string()
                    } else {
                        found
                    }
                };
                if target.is_empty() {
                    toast(&self.window, "未能从参数中识别出目标，请手动加入授权范围");
                    return;
                }
                // 空间不再有独立授权目标：无论当前是否有活跃空间，一律写全局 [scope] authorized。
                if !self.cfg.scope.authorized.contains(&target) {
                    self.cfg.scope.authorized.push(target.clone());
                }
                match self.cfg.commit(&self.cfg_path, &[ConfigSection::Scope]) {
                    Ok(_) => {
                        self.sync_base_cfg();
                        self.session = None;
                        self.ensure_session();
                        apply_snapshot(&self.base, &self.window);
                        toast(&self.window, format!("目标 {target} 已加入全局授权白名单"));
                    }
                    Err(e) => toast(&self.window, format!("加入授权白名单失败：{e}")),
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
                        self.sync_base_cfg();
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
            Cmd::PickTool(id) => self.push_ab_detail(&id),
            Cmd::RegOp { id, op } => {
                // #5/#6 组管理 + 分配：这些 op 只改注册表，不改工具装配语义
                let group_op: Option<String> = if let Some(name) = op.strip_prefix("group-add:") {
                    Some(format!("新建组 {name}"))
                } else if let Some(name) = op.strip_prefix("group-rename:") {
                    Some(format!("组改名为 {name}"))
                } else if op == "group-remove" {
                    Some("删除组（条目移到未分配）".into())
                } else if let Some(desc) = op.strip_prefix("group-desc:") {
                    Some(format!("组描述：{desc}"))
                } else if let Some(group) = op.strip_prefix("assign:") {
                    Some(format!("分配到组 {group}"))
                } else {
                    None
                };
                if group_op.is_some() {
                    let path = wpa_engine::registry::registry_path(&self.cfg.data_root());
                    // 删组前先查引用：必须在拿 registry 写锁**之前**算，
                    // snapshot() 内部会再读这把锁，持锁调用会自锁。
                    let group_refs: Vec<String> = if op == "group-remove" {
                        self.base
                            .snapshot()
                            .domains
                            .iter()
                            .filter(|d| d.groups.iter().any(|g| g == &id))
                            .map(|d| d.id.clone())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut reg = self.base.registry.write().unwrap_or_else(|e| e.into_inner());
                    let (scanned, _) = wpa_engine::registry::scan_plugins(&self.base.cfg.plugins_root());
                    reg.reconcile(&scanned);
                    // 新建组时自动派生 id：用户只该输入组名，手敲 id 属于"手书"，
                    // 而且还要保证它是个合法目录名——那是程序该干的事。
                    let mut created: Option<(String, String)> = None;
                    // assign 成功后要搬目录（账改了、文件也得跟着走）
                    let mut assigned_to: Option<String> = None;
                    let result: Result<(), String> = if let Some(name) = op.strip_prefix("group-add:") {
                        let display = name.trim().to_string();
                        if display.is_empty() {
                            Err("组名称不能为空".to_string())
                        } else {
                            let base = wpa_engine::registry::slug_group_id(&display);
                            let gid = wpa_engine::registry::unique_group_id(&base, |c| {
                                reg.groups.iter().any(|g| g.id == c)
                            });
                            match reg.add_group(&gid, &display, "") {
                                Ok(()) => {
                                    created = Some((gid, display));
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        }
                    } else if let Some(name) = op.strip_prefix("group-rename:") {
                        reg.rename_group(&id, name.trim())
                    } else if op == "group-remove" {
                        if !group_refs.is_empty() {
                            Err(format!(
                                "组 {id} 被领域 {} 引用；先去那些领域页取消引用，再删组",
                                group_refs.join("、")
                            ))
                        } else {
                            reg.remove_group(&id).map(|_| ())
                        }
                    } else if let Some(desc) = op.strip_prefix("group-desc:") {
                        reg.set_group_description(&id, desc)
                    } else if let Some(group) = op.strip_prefix("assign:") {
                        match reg.assign_entry(&id, group) {
                            Ok(()) => {
                                assigned_to = Some(group.to_string());
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        unreachable!()
                    };
                    let (ok, mut msg) = match result {
                        Ok(()) => match reg.save(&path) {
                            Ok(()) => (true, format!("{}：成功（已保存）", group_op.unwrap())),
                            Err(e) => (false, format!("保存失败：{e}")),
                        },
                        Err(e) => (false, e),
                    };
                    drop(reg);
                    // 建组顺带建目录 + 清单：不建的话用户没法往里放 dll，
                    // 放了也扫不到（扫描只认带 PnC.toml 的目录）。
                    if ok {
                        if let Some((gid, gname)) = &created {
                            let root = self.base.cfg.plugins_root();
                            match wpa_engine::registry::ensure_group_dir(&root, gid, gname, "") {
                                Ok(dir) => {
                                    msg = format!("{msg}；已建目录 {}", dir.display());
                                }
                                Err(e) => {
                                    msg = format!("{msg}；但建目录失败：{e}（往组里放 dll 前请手工建目录）")
                                }
                            }
                        }
                        // 分配成功后搬文件：账上说在 B，磁盘也得在 B，
                        // 否则"账上在 B、实际从 A 加载"。
                        if let Some(g) = &assigned_to {
                            let root = self.base.cfg.plugins_root();
                            let note = wpa_engine::registry::relocate_plugin(&root, &id, g);
                            if !note.is_empty() {
                                msg = format!("{msg}；{note}");
                            }
                        }
                    }
                    self.reload_tree();
                    toast(&self.window, msg);
                    return;
                }
                // move:<组id> = 组↔条目绑定（换归属不动开关）
                if let Some(to_group) = op.strip_prefix("move:") {
                    let path = wpa_engine::registry::registry_path(&self.cfg.data_root());
                    let mut reg = match self.base.registry.write() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    let (snap, _) = wpa_engine::registry::scan_plugins(&self.base.cfg.plugins_root());
                    reg.reconcile(&snap);
                    let msg = match reg.move_entry(&id, to_group) {
                        Some((from, to)) if from == to => {
                            format!("{id} 已经在组 {to} 里")
                        }
                        Some((from, to)) => match reg.save(&path) {
                            Ok(()) => {
                                drop(reg);
                                // 账改完再搬目录：搬不动（被占用）会登记待下次启动重试，
                                // 账不回滚——用户的意图是对的，只是时机不对。
                                let root = self.base.cfg.plugins_root();
                                let note = wpa_engine::registry::relocate_plugin(&root, &id, &to);
                                self.reload_tree();
                                if note.is_empty() {
                                    format!("{id}：{from} → {to}（已重新装配）")
                                } else {
                                    format!("{id}：{from} → {to}；{note}")
                                }
                            }
                            Err(e) => format!("移动后保存失败：{e}"),
                        },
                        None => format!(
                            "注册表里没有可移动的条目 `{id}`（内置工具固定属于原组，不能换组）"
                        ),
                    };
                    toast(&self.window, msg);
                    return;
                }
                let path = wpa_engine::registry::registry_path(&self.cfg.data_root());
                let (scanned, _) = wpa_engine::registry::scan_plugins(&self.base.cfg.plugins_root());
                let msg = {
                    let mut reg = match self.base.registry.write() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    let is_group = reg.group(&id).is_some();
                    let out = match (op.as_str(), is_group) {
                        ("install", true) => {
                            // 组里的候选来自扫描快照（注册表只有已安装的）
                            let n = reg.install_group_from(&id, &scanned);
                            if n == 0 && reg.group(&id).map(|g| g.entries.is_empty()).unwrap_or(true) {
                                format!("组 {id} 里没有可安装的条目（检查 dll 是否齐全）")
                            } else {
                                format!("组 {id} 已安装：{n} 个条目")
                            }
                        }
                        // #4 单条安装/卸载：走完整状态机（installed + status），
                        // 不再只改 enabled——那正是"点安装没变化/卸载显示禁用"的根因。
                        ("install", false) => {
                            if reg.is_installed(&id) {
                                format!("条目 {id} 已安装")
                            } else {
                                // 候选不在注册表里（账本只记已安装）：从扫描快照找回它再落库
                                let hit = scanned
                                    .groups
                                    .iter()
                                    .flat_map(|g| g.entries.iter().map(move |e| (g, e)))
                                    .find(|(_, e)| e.id == id);
                                match hit {
                                    Some((g, e)) => {
                                        match reg.install_candidate(&g.id, &g.source, e) {
                                            Ok(()) => format!("条目 {id} 已安装（组 {}）", g.id),
                                            Err(err) => err,
                                        }
                                    }
                                    None => format!(
                                        "找不到条目 {id}：既不在注册表里，也不在扫描结果里"
                                    ),
                                }
                            }
                        }
                        ("uninstall", true) => {
                            let n = reg.uninstall_group(&id);
                            format!("组 {id} 已卸载：{n} 个条目转为候选")
                        }
                        ("uninstall", false) => match reg.uninstall_entry(&id) {
                            Ok(()) => format!("条目 {id} 已卸载（未安装）"),
                            Err(e) => e,
                        },
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
                            // 组开关的唯一账本是注册表。插件组要先过依赖校验
                            // （requires_groups 的闭包与环检测），内置组没有依赖声明。
                            let enabled: std::collections::BTreeSet<String> = reg
                                .groups
                                .iter()
                                .filter(|g| g.enabled)
                                .map(|g| g.id.clone())
                                .collect();
                            let chk = match self.base.plugins.as_ref() {
                                Some(h) if h.report().group(&id).is_some() => {
                                    h.check_enable(&enabled, &id)
                                }
                                _ => Ok(vec![id.clone()]),
                            };
                            match chk {
                                Ok(more) => {
                                    let mut n = 0;
                                    for g in &more {
                                        if reg.set_group_enabled(g, true) {
                                            n += 1;
                                        }
                                    }
                                    if n == 0 {
                                        format!("组 {id} 已是启用状态")
                                    } else if more.len() > 1 {
                                        format!("组 {id} 已启用（连带依赖组：{}）", more.join(" → "))
                                    } else {
                                        format!("组 {id} 已启用")
                                    }
                                }
                                Err(e) => e,
                            }
                        }
                        ("group-off", _) => {
                            let enabled: std::collections::BTreeSet<String> = reg
                                .groups
                                .iter()
                                .filter(|g| g.enabled)
                                .map(|g| g.id.clone())
                                .collect();
                            let chk = match self.base.plugins.as_ref() {
                                Some(h) if h.report().group(&id).is_some() => {
                                    h.check_disable(&enabled, &id)
                                }
                                _ => Ok(()),
                            };
                            match chk {
                                Err(e) => e,
                                Ok(()) => {
                                    if reg.set_group_enabled(&id, false) {
                                        format!("组 {id} 已停用")
                                    } else {
                                        format!("组 {id} 已是停用状态")
                                    }
                                }
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
                // #4 操作后立即回填详情，按钮状态跟着变（不必等用户重新点选条目）
                self.push_ab_detail(&id);
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
                // 全局禁用的组不能新增引用：引用了也不会生效，只会让人以为"配了没反应"
                let enabled = snap
                    .reg_groups
                    .iter()
                    .find(|g| g.id == *gid)
                    .map(|g| g.enabled)
                    .unwrap_or(true);
                if !enabled && !groups.contains(&gid) {
                    toast(
                        &self.window,
                        format!("组 {gid} 已在「能力」页全局禁用；先启用它，才能被领域引用"),
                    );
                    return;
                }
                match groups.iter().position(|x| *x == *gid) {
                    Some(i) => {
                        groups.remove(i);
                    }
                    None => groups.push(gid.clone()),
                }
                self.push_domain_chips(&snap, true);
            }
            Cmd::DomainExtAdd { kind, path, usage } => {
                let Some((id, _)) = self.domain_edit.clone() else {
                    toast(&self.window, "先在左侧选中一个领域");
                    return;
                };
                let file = self.base.cfg.domains_root().join(&id).join("domain.toml");
                let mut decls = match wpa_engine::domain::load(&self.base.cfg.domains_root().join(&id)) {
                    Ok(p) => p.external,
                    Err(e) => {
                        toast(&self.window, format!("领域 {id} 装载失败：{e}"));
                        return;
                    }
                };
                // 路径统一转成相对领域目录的写法：领域目录整体搬走后仍然可用
                let path = relativize(&self.base.cfg.domains_root().join(&id), &path);
                if decls.iter().any(|d| d.path == path && d.kind_norm() == kind) {
                    toast(&self.window, format!("这条外部工具已经登记过了：{path}"));
                    return;
                }
                decls.push(wpa_engine::domain::ExternalDecl {
                    kind: kind.clone(),
                    path: path.clone(),
                    name: String::new(),
                    usage,
                    namespace: String::new(),
                    risk: String::new(),
                    args: Vec::new(),
                    params: Default::default(),
                    stdin: String::new(),
                });
                match wpa_engine::domain::set_external(&file, &decls) {
                    Ok(()) => {
                        self.reload_tree();
                        toast(&self.window, format!("已加外部工具：{path}（{} 条）", decls.len()));
                    }
                    Err(e) => toast(&self.window, format!("写入失败：{e}")),
                }
            }
            Cmd::DomainExtRemove(idx) => {
                let Some((id, _)) = self.domain_edit.clone() else {
                    toast(&self.window, "先在左侧选中一个领域");
                    return;
                };
                let file = self.base.cfg.domains_root().join(&id).join("domain.toml");
                let mut decls = match wpa_engine::domain::load(&self.base.cfg.domains_root().join(&id)) {
                    Ok(p) => p.external,
                    Err(e) => {
                        toast(&self.window, format!("领域 {id} 装载失败：{e}"));
                        return;
                    }
                };
                if idx >= decls.len() {
                    toast(&self.window, format!("第 {idx} 条外部工具不存在（共 {} 条）", decls.len()));
                    return;
                }
                let gone = decls.remove(idx);
                match wpa_engine::domain::set_external(&file, &decls) {
                    Ok(()) => {
                        self.reload_tree();
                        toast(
                            &self.window,
                            format!("已移除外部工具：{}（剩 {} 条）", gone.display_name(), decls.len()),
                        );
                    }
                    Err(e) => toast(&self.window, format!("写入失败：{e}")),
                }
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
            // #8 复制消息：写系统剪贴板（Windows 下用 Win32，见 clipboard 模块）
            Cmd::CopyMessage(text) => {
                match crate::clipboard::copy(&text) {
                    Ok(()) => toast(&self.window, "已复制到剪贴板"),
                    Err(e) => toast(&self.window, format!("复制失败：{e}")),
                }
            }

            // #11 任务看板 CRUD
            Cmd::TaskAdd(title) => {
                let title = title.trim().to_string();
                if title.is_empty() {
                    return;
                }
                // owner=agent：与模型维护的清单共用同一张表，能被 task.todo 回注/整表覆盖
                match self.base.tasks.create(&title, "", "", "agent", vec![]) {
                    Ok(id) => {
                        toast(&self.window, format!("已添加任务 #{id}"));
                        apply_snapshot(&self.base, &self.window);
                    }
                    Err(e) => toast(&self.window, format!("添加失败：{e}")),
                }
            }
            Cmd::TaskMove { id, col } => {
                match id.parse::<u64>().ok().and_then(|n| self.base.tasks.get(n)) {
                    Some(mut rec) => {
                        rec.status = match col {
                            0 => 0,       // 待规划 = pending
                            1 => 1,       // 待办
                            2 => 2,       // 进行中
                            _ => 3,       // 已完成（TaskStore 用 3 = done）
                        };
                        match self.base.tasks.update(&mut rec) {
                            Ok(()) => apply_snapshot(&self.base, &self.window),
                            Err(e) => toast(&self.window, format!("移动失败：{e}")),
                        }
                    }
                    None => toast(&self.window, format!("任务 {id} 不存在")),
                }
            }
            Cmd::TaskDelete(id) => {
                match id.parse::<u64>().map(|n| self.base.tasks.delete(n)) {
                    Ok(Ok(true)) => apply_snapshot(&self.base, &self.window),
                    Ok(Ok(false)) => toast(&self.window, "任务不存在"),
                    Ok(Err(e)) => toast(&self.window, format!("删除失败：{e}")),
                    Err(_) => toast(&self.window, format!("非法任务 id：{id}")),
                }
            }

            // #9 空间编辑保存
            Cmd::SpaceSave { name, cwd, whitelist, permission, sandbox, high_risk } => {
                let Some(mut w) = self.base.workspaces.get(&name) else {
                    toast(&self.window, format!("空间 {name} 不存在"));
                    return;
                };
                w.cwd = cwd.trim().to_string();
                w.plugins = whitelist.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                // 权限二值：unrestricted = 允许完全访问；其余 = 默认全局
                w.unrestricted = permission == "unrestricted";
                // 沙箱二值：on / off（UI 只有两项）
                w.sandbox_enabled = Some(sandbox == "on");
                // 高风险三态：domain（跟随领域）/ on / off
                w.allow_high_risk = match high_risk.as_str() {
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => None,
                };
                match self.base.workspaces.update(w) {
                    Ok(()) => {
                        // 空间级策略 / cwd 变了：必须是全新装配（file 工具根、shell 默认 cwd、
                        // AGENTS.md 工作区都在装配期固化，只重建会话不生效）。
                        self.reload_tree();
                        toast(&self.window, format!("空间 {name} 已保存（已重新装配）"));
                    }
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }

            // 打开工作区目录（资源管理器直达）
            Cmd::OpenSpaceDir(name) => {
                let root = self.base.workspaces.workspace_root_of(&name);
                if let Err(e) = std::fs::create_dir_all(&root) {
                    toast(&self.window, format!("打开目录失败：{e}"));
                    return;
                }
                #[cfg(windows)]
                let spawned = std::process::Command::new("explorer").arg(&root).spawn();
                #[cfg(not(windows))]
                let spawned = std::process::Command::new("xdg-open").arg(&root).spawn();
                if let Err(e) = spawned {
                    toast(&self.window, format!("打开目录失败：{e}"));
                }
            }

            // 初始化工作区骨架（scripts/、output/、AGENTS.md、空间记忆）
            Cmd::InitSpaceDir(name) => match self.base.workspaces.init_layout(&name) {
                Ok(v) => {
                    // 目录可能刚从"不存在"变为可用：重新装配让 file/shell 用上新工作区。
                    self.reload_tree();
                    if v.is_empty() {
                        toast(&self.window, "工作区骨架已存在，无需初始化");
                    } else {
                        toast(&self.window, format!("已初始化工作区（新建 {} 项）", v.len()));
                    }
                }
                Err(e) => toast(&self.window, format!("初始化失败：{e}")),
            },

            // #9.1 会话管理
            Cmd::SessionList(filter) => {
                let Some(store) = self.base.sessions.as_ref() else {
                    toast(&self.window, "会话库不可用");
                    return;
                };
                let filter = filter.clone();
                let rows: Vec<SessionRow> = store
                    .list_by_workspace(&filter)
                    .into_iter()
                    .map(|r| {
                        let preview = if r.title.is_empty() {
                            store
                                .recall_preview(&r.id, 1)
                                .ok()
                                .and_then(|v| v.first().cloned())
                                .unwrap_or_default()
                        } else {
                            r.title.clone()
                        };
                        SessionRow {
                            id: r.id.clone().into(),
                            preview: preview.into(),
                            turns: r.turns as i32,
                            updated: format_time(r.updated_ms).into(),
                            space: r.workspace.into(),
                        }
                    })
                    .collect();
                ui_set(&self.window, move |app| {
                    app.set_sessions(ModelRc::new(VecModel::from(rows)));
                });
            }
            Cmd::SessionView(id) => {
                // 只读预览：把这条会话的消息拉到会话页右侧面板。
                // 刻意不碰 self.session / 不重建引擎会话——「看」和「切」是两件事，
                // 预览顺手覆盖掉用户正在写的对话是最容易惹人烦的失败模式。
                let Some(store) = self.base.sessions.as_ref() else { return };
                let Some(rec) = store.get(&id) else {
                    toast(&self.window, format!("未找到会话 {id}"));
                    return;
                };
                let lines = if let Some(st) = &rec.state { &st.messages } else { &rec.messages };
                let title = if rec.title.is_empty() { id.clone() } else { rec.title.clone() };
                let space = if rec.workspace.is_empty() { "全局".to_string() } else { rec.workspace.clone() };
                let meta = format!(
                    "{} 轮 · 更新于 {} · 空间 {}",
                    rec.turns,
                    format_time(rec.updated_ms),
                    space
                );
                let msgs: Vec<ChatMsg> = lines
                    .iter()
                    .filter_map(|m| {
                        let role = match m.role {
                            0 => "user",
                            1 => "assistant",
                            3 => "system",
                            _ => return None,
                        };
                        Some(ChatMsg { role: role.into(), text: m.text.clone().into() })
                    })
                    .collect();
                let n = msgs.len();
                ui_set(&self.window, move |app| {
                    app.set_session_view(ModelRc::new(VecModel::from(msgs)));
                    app.set_session_view_title(title.into());
                    app.set_session_view_meta(meta.into());
                });
                toast(&self.window, format!("预览会话「{id}」：{n} 条消息（未切换当前对话）"));
            }
            Cmd::SessionOpen(id) => {
                // 真正切换会话：读完整记录 → 重建会话 → 恢复协议历史/轮次 → 续写原记录。
                // 不再像旧版那样把摘要注入当前会话（那会占用当前会话、丢失当前上下文）。
                let Some(store) = self.base.sessions.clone() else {
                    toast(&self.window, "会话库不可用");
                    return;
                };
                let Some(rec) = store.get(&id) else {
                    toast(&self.window, format!("未找到会话 {id}"));
                    return;
                };
                let snap = agent_core::ChatSession::session_snapshot(&rec);
                let turns = rec.turns;
                let label = if rec.title.is_empty() { id.clone() } else { rec.title.clone() };

                // 重建引擎会话（走 ensure_session 的统一装配路径，再覆盖状态）
                self.session = None;
                self.ensure_session();
                if let Some(s) = self.session.as_mut() {
                    s.restore_full(snap, turns);
                    s.set_persist_id(Some(id.clone()));
                    s.set_persist_identity(rec.workspace.clone(), rec.profile.clone());
                }

                // 重建 UI 消息列表：按展示行还原 user/assistant（轨迹/轮次分隔不进气泡）
                {
                    let lines = if let Some(st) = &rec.state { &st.messages } else { &rec.messages };
                    let mut msgs: Vec<ChatMsg> = Vec::new();
                    for m in lines {
                        let role = match m.role {
                            0 => "user",
                            1 => "assistant",
                            3 => "system",
                            _ => continue,
                        };
                        msgs.push(ChatMsg { role: role.into(), text: m.text.clone().into() });
                    }
                    *self.ui.messages.lock().unwrap() = msgs.clone();
                    *self.ui.traces.lock().unwrap() = Vec::new();
                    let w = self.window.clone();
                    ui_set(&w, move |app| {
                        app.set_messages(ModelRc::new(VecModel::from(msgs)));
                        app.set_traces(ModelRc::new(VecModel::from(Vec::<TraceRow>::new())));
                    });
                }
                if let Some(info) = self.info.clone() {
                    self.apply_info(&info);
                }
                toast(&self.window, format!("已打开会话「{label}」（{} 轮），当前对话已切换", turns));
            }
            Cmd::SessionRecall(id) => {
                // 召回：把旧会话摘要注入当前会话（不切换、不占用当前上下文窗口）
                let Some(store) = self.base.sessions.as_ref() else { return };
                match store.recall_preview(&id, 8) {
                    Ok(lines) => {
                        let text = lines.join("\n");
                        push_msg(&self.window, &self.ui.messages, "assistant", format!("【会话 {id} 召回】\n{text}"));
                        toast(&self.window, format!("已召回会话 {id} 的最近 {} 条", lines.len()));
                    }
                    Err(e) => toast(&self.window, format!("召回失败：{e}")),
                }
            }
            Cmd::SessionDelete(id) => {
                let Some(store) = self.base.sessions.as_ref() else { return };
                let removed_id = id.clone();
                match store.delete(&id) {
                    Ok(true) => {
                        toast(&self.window, "会话已删除");
                        ui_set(&self.window, move |app| {
                            let model = app.get_sessions();
                            let cur: Vec<SessionRow> = (0..model.row_count())
                                .filter_map(|i| model.row_data(i))
                                .filter(|s| s.id != removed_id.as_str())
                                .collect();
                            app.set_sessions(ModelRc::new(VecModel::from(cur)));
                        });
                    }
                    Ok(false) => toast(&self.window, "会话不存在"),
                    Err(e) => toast(&self.window, format!("删除失败：{e}")),
                }
            }
            Cmd::SessionExport(id) => {
                let Some(store) = self.base.sessions.as_ref() else { return };
                let dir = self.base.cfg.data_root().join("exports");
                let path = dir.join(format!("{id}.jsonl"));
                match store.export_jsonl(&id, &path) {
                    Ok(()) => toast(&self.window, format!("已导出：{}", path.display())),
                    Err(e) => toast(&self.window, format!("导出失败：{e}")),
                }
            }

            // #7.1 领域 CRUD + 文件编辑
            Cmd::DomainCreate { id, name } => {
                let root = self.base.cfg.domains_root();
                let display = if name.trim().is_empty() { id.clone() } else { name.clone() };
                match wpa_engine::domain::create_domain(&root, &id, &display) {
                    Ok(()) => {
                        self.reload_tree();
                        toast(&self.window, format!("领域 {id} 已创建"));
                    }
                    Err(e) => toast(&self.window, format!("创建失败：{e}")),
                }
            }
            Cmd::DomainRemove(id) => {
                let root = self.base.cfg.domains_root();
                match wpa_engine::domain::remove_domain(&root, &id) {
                    Ok(()) => {
                        if self.cfg.domain.active == id {
                            self.cfg.domain.active.clear();
                            let _ = self.cfg.commit(&self.cfg_path, &[ConfigSection::Domain]);
                        }
                        self.reload_tree();
                        toast(&self.window, format!("领域 {id} 已删除"));
                    }
                    Err(e) => toast(&self.window, format!("删除失败：{e}")),
                }
            }
            Cmd::DomainFileRead { id, file } => {
                let root = self.base.cfg.domains_root();
                match wpa_engine::domain::read_domain_file(&root, &id, &file) {
                    Ok(text) => ui_set(&self.window, move |app| {
                        app.set_dom_file_text(text.into());
                    }),
                    Err(e) => toast(&self.window, format!("读取失败：{e}")),
                }
            }
            Cmd::DomainFileSave { id, file, text } => {
                let root = self.base.cfg.domains_root();
                match wpa_engine::domain::write_domain_file(&root, &id, &file, &text) {
                    Ok(()) => toast(&self.window, format!("已保存 {file}")),
                    Err(e) => toast(&self.window, format!("保存失败：{e}")),
                }
            }

            // 领域概要（≤300 字）：只改 domain.toml 的 summary 一行
            Cmd::DomainSummarySave { id, summary } => {
                let root = self.base.cfg.domains_root();
                match wpa_engine::domain::set_summary(&root, &id, &summary) {
                    Ok(()) => {
                        self.reload_tree();
                        toast(&self.window, format!("领域 {id} 的概要已保存"));
                    }
                    Err(e) => toast(&self.window, format!("保存概要失败：{e}")),
                }
            }

            // AI 生成领域配置：概要 → 模型 → 四份文件（原文件 .bak 备份 + 装载校验 + 失败回滚）
            Cmd::DomainAiGenerate { id, summary } => {
                let root = self.base.cfg.domains_root();
                let dir = root.join(&id);
                let pack = match wpa_engine::domain::load(&dir) {
                    Ok(p) => p,
                    Err(e) => {
                        toast(&self.window, format!("装载领域失败：{e}"));
                        return;
                    }
                };
                // 用编辑框里的当前文本；它是空的才回落到已保存的概要
                let summary = if summary.trim().is_empty() {
                    pack.manifest.summary.trim().to_string()
                } else {
                    summary.trim().to_string()
                };
                if summary.is_empty() {
                    toast(&self.window, "领域概要为空：先在上面的概要框写清需求，再让 AI 生成");
                    return;
                }
                if summary.chars().count() > wpa_engine::domain::SUMMARY_MAX_CHARS {
                    toast(
                        &self.window,
                        format!(
                            "领域概要超过 {} 字，生成前请先压缩",
                            wpa_engine::domain::SUMMARY_MAX_CHARS
                        ),
                    );
                    return;
                }
                // 用当前生效的模型；没有就是「无法操作」，不做离线兜底
                let profile = self
                    .cfg
                    .llm
                    .active_profile()
                    .cloned()
                    .or_else(|| self.cfg.llm.resolve_chain(None).into_iter().next());
                let Some(profile) = profile else {
                    toast(
                        &self.window,
                        "无法生成：没有可用的模型。请先在「配置 · 模型」启用一个档案（填 base_url 与模型名）",
                    );
                    return;
                };
                let (system, user) = wpa_engine::domain::gen_prompt(&pack, &summary);
                ui_set(&self.window, |app| app.set_ai_busy(true));
                let provider_cfg = profile.to_provider_config();
                let outcome = (|| -> Result<Vec<String>, String> {
                    let provider = base_llm::build_provider(provider_cfg)
                        .map_err(|e| format!("构造模型调用失败：{e}"))?;
                    let msgs = vec![base_llm::system_message(system), base_llm::user_message(user)];
                    let done = self
                        .rt
                        .block_on(provider.complete(&msgs, &[]))
                        .map_err(|e| format!("调用模型失败：{e}"))?;
                    let raw = done.content.unwrap_or_default();
                    let json = wpa_engine::domain::extract_json_object(&raw)?;
                    wpa_engine::domain::apply_generated(&dir, &json)
                })();
                ui_set(&self.window, |app| app.set_ai_busy(false));
                match outcome {
                    Ok(files) => {
                        self.reload_tree();
                        toast(
                            &self.window,
                            format!("已生成并替换：{}（原文件备份为 .bak）", files.join("、")),
                        );
                    }
                    Err(e) => toast(&self.window, format!("生成失败：{e}")),
                }
            }

            // #7.2 知识管理
            Cmd::KnowledgeAdd { name, text } => {
                let dir = self.base.cfg.active_domain_dir();
                let provider = knowledge_core::FileKnowledgeProvider::from_dir("domain", &dir, &[], None);
                match provider.add_entry(&name, &text) {
                    Ok(()) => {
                        self.reload_tree();
                        toast(&self.window, format!("知识 {name} 已添加"));
                    }
                    Err(e) => toast(&self.window, format!("添加失败：{e}")),
                }
            }
            Cmd::KnowledgeRemove(name) => {
                let dir = self.base.cfg.active_domain_dir();
                let provider = knowledge_core::FileKnowledgeProvider::from_dir("domain", &dir, &[], None);
                match provider.remove_entry(&name) {
                    Ok(()) => {
                        self.reload_tree();
                        toast(&self.window, format!("知识 {name} 已删除"));
                    }
                    Err(e) => toast(&self.window, format!("删除失败：{e}")),
                }
            }

            // #13.1/#16 路径浏览结果：复用 SetField 的校验/落盘链路
            Cmd::SetFieldPath { key, path } => {
                self.handle(Cmd::SetField { key, value: path });
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
