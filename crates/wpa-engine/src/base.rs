//! 基座装配与会话构建：把配置、群组契约、通用工具、空间、模型链拼成一个可用的会话。
//!
//! 分层：`assemble` 只做"进程级"装配（工具/群组/存储），`build_session` 做"会话级"
//! 装配（空间生效面、模型链、上下文预算、记忆注入）。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{ApprovalCallback, ChatConfig, ChatEventCallback, ChatSession, SkillNote};
use base_llm::ToolSchema;
use kern_core::model::{RiskLevel, SessionId};
use kern_core::{Registry, ToolExecutor};
use shell_core::{JobStore, ShellJobs, ShellRun, ShellSessions, ShellSessionsTool};
use toolkit_core::builtin::{Echo, FileReadTool, FileWriteTool, PipTool, ProcExec, PySandboxTool};
use toolkit_core::{ToolRegistryExt, ToolkitExecutor};
use wpa_api::{GroupPlugin, GroupRegistry, GroupRuntime};
use workspace_core::{
    Workspace, WorkspaceComponent, WorkspaceRegistry, WsCreate, WsInfo, WsList, WsMemoryAppend,
    WsMemoryRead, WsRecord, WsRemove, WsSwitch,
};

use crate::config::{self, AppConfig, PlanMode};
use crate::host::{self, BaseRuntime};
use crate::llm::FallbackProvider;
use crate::snapshot::{CapGroupRow, DomainCardRow, SkillRow, Snapshot, SpaceRow, ToolRow};
use crate::tools::{ConstraintHook, KbSearch, PlanPhase};

/// 装配好的基座（进程级）。
pub struct Base {
    pub kern: Arc<Registry>,
    pub executor: Arc<dyn ToolExecutor>,
    pub api: GroupRegistry,
    pub runtime: Arc<BaseRuntime>,
    pub session_id: SessionId,
    /// 本次链接的群组实现（退出时反拓扑卸载）。
    pub linked: Vec<Arc<dyn GroupPlugin>>,
    pub workspaces: Arc<WorkspaceRegistry>,
    pub shells: Arc<ShellSessions>,
    pub jobs: Arc<JobStore>,
    pub tasks: Arc<base_store::TaskStore>,
    pub experience: Arc<base_store::ExperienceStore>,
    /// 拒绝清单（配置级；空间级在会话构建时合并）。
    pub denylist: Arc<Vec<String>>,
    pub cfg: AppConfig,
    /// 装配期告警（模型初始化失败、工具重名等），供 UI 状态栏提示。
    pub warnings: Vec<String>,
    /// 当前作业阶段（由逻辑群组声明；无逻辑群组时为空集，行为等同自由 ReAct）。
    pub phases: tree_core::SharedPhases,
    /// 知识约束（编译后的 guard 规则；会话构建时接成钩子）。
    pub constraints: Arc<tree_core::ConstraintSet>,
    /// 工具注册表（`<data>/registry.toml`）：UI 的启停/装卸都落到这里，
    /// 改完需要重新装配（`reload`）才影响工具面。
    pub registry: Arc<std::sync::RwLock<crate::registry::ToolRegistry>>,
    /// 原子插件宿主（`plugins/` 目录）；未启用时为 None。
    pub plugins: Option<Arc<plugin_host::PluginHost>>,
    /// 激活的领域（描述包）；None = 纯基座模式（无角色注入、无领域常识）。
    pub domain: Option<Arc<crate::domain::DomainPack>>,
    /// 结构化计划状态（规划层；与 plan.* 工具共享）。
    pub plan: agent_core::plan_state::SharedPlan,
    /// 子代理使用的 provider 槽（模型切换后由 build_session 更新）。
    pub provider_cell: agent_core::subagent::ProviderCell,
    /// 会话库（逐轮落盘 + 跨会话检索；None = 打不开，记忆层降级）。
    pub sessions: Option<Arc<base_store::SessionStore>>,
    /// 向量库使用的 embedding 档案槽（模型切换后由上层刷新，调用时读最新值）。
    pub embedder: Arc<crate::embedder::SharedEmbedder>,
}

impl Base {
    /// 角色设定：领域 persona 优先，其次树群组注入，最后无（用基座默认）。
    ///
    /// 领域是唯一"允许出现你是谁"的地方；基座代码里不写任何角色措辞。
    pub fn persona(&self) -> Option<&str> {
        self.domain
            .as_ref()
            .and_then(|d| d.persona())
            .or_else(|| self.api.persona())
    }
}

/// 按注册表注册工具的辅助器。
///
/// 为什么要有它：`register_base_tools` 里 20 多个 `kern.register_tool(...)` 是硬编码的，
/// 而 P0-1 要求"注册表是唯一真相源、内置工具也能被单独禁用"。把"是否生效"收敛到
/// 一处判断，比在 20 个调用点各写一遍 `if` 可靠。
struct Reg<'a> {
    kern: &'a Registry,
    reg: &'a crate::registry::ToolRegistry,
    /// 被注册表跳过（未安装 / 已禁用 / broken）的工具，用于装配报告。
    skipped: Vec<String>,
}

impl<'a> Reg<'a> {
    fn new(kern: &'a Registry, reg: &'a crate::registry::ToolRegistry) -> Self {
        Self { kern, reg, skipped: Vec::new() }
    }

    fn add(&mut self, name: &str, tool: Arc<dyn toolkit_core::tools::Tool>) -> anyhow::Result<()> {
        // 双闸门：账本里必须"已安装"，且组/条目开关都开着。
        // 单看 is_active 不够——它对"查不到"默认放行（运行时工具的口子）。
        if !self.reg.is_installed(name) || !self.reg.is_active(name) {
            self.skipped.push(name.to_string());
            return Ok(());
        }
        self.kern.register_tool(tool)?;
        Ok(())
    }
}

/// 基座通用工具注册（无业务语义）：回显 / 文件 / 命令 / Python 沙箱 / shell / 空间。
fn register_base_tools(
    kern: &Registry,
    cfg: &AppConfig,
    workspaces: &Arc<WorkspaceRegistry>,
    shells: &Arc<ShellSessions>,
    jobs: &Arc<JobStore>,
    reg: &crate::registry::ToolRegistry,
    embedder: Arc<crate::embedder::SharedEmbedder>,
) -> anyhow::Result<()> {
    let restricted = cfg.sandbox.restricted_token;
    let mut r = Reg::new(kern, reg);
    r.add("util.echo", Arc::new(Echo))?;
    r.add("file.read", Arc::new(FileReadTool::new()))?;
    r.add("file.write", Arc::new(FileWriteTool::new()))?;
    r.add("os.exec", Arc::new(ProcExec::new().with_restricted(restricted)))?;
    r.add("sandbox.python", Arc::new(PySandboxTool::default().with_restricted(restricted)))?;
    r.add("sandbox.pip", Arc::new(PipTool))?;

    // shell 会话（持久 cwd / env）
    if let Some(w) = workspaces.active() {
        if !w.cwd.trim().is_empty() {
            shells.set_default_cwd(&w.cwd);
        }
    }
    r.add("shell.run", Arc::new(ShellRun(shells.clone(), jobs.clone())))?;
    r.add("shell.jobs", Arc::new(ShellJobs(jobs.clone())))?;
    r.add("shell.sessions", Arc::new(ShellSessionsTool(shells.clone())))?;

    // 工作空间（8 个 ws.* 工具）
    r.add("ws.create", Arc::new(WsCreate(workspaces.clone())))?;
    r.add("ws.list", Arc::new(WsList(workspaces.clone())))?;
    r.add("ws.switch", Arc::new(WsSwitch(workspaces.clone())))?;
    r.add("ws.remove", Arc::new(WsRemove(workspaces.clone())))?;
    r.add("ws.info", Arc::new(WsInfo(workspaces.clone())))?;
    r.add("ws.record", Arc::new(WsRecord(workspaces.clone())))?;
    r.add("ws.memory.append", Arc::new(WsMemoryAppend(workspaces.clone())))?;
    r.add("ws.memory.read", Arc::new(WsMemoryRead(workspaces.clone())))?;

    // 组件登记（一切皆插件，供 UI 组件页与审计展示）
    let _ = kern.register(Arc::new(WorkspaceComponent { registry: workspaces.clone() }));

    // 网络原语（自研：TCP 探测 / DNS 报文 / HTTP），通用能力，无业务语义
    for t in net_core::tools::all() {
        let n = t.name().to_string();
        r.add(&n, t)?;
    }
    // 文本原语（编解码 / 哈希 / 正则 / 线索提取 / 变换）
    for t in text_core::tools::all() {
        let n = t.name().to_string();
        r.add(&n, t)?;
    }
    // 知识读写（kb）：本地 sqlite + 向量库。基础组，任何领域都可用，
    // 因此不参与领域装配；向量写入/检索时读共享 embedding 槽（没有模型就报错）。
    for t in data_core::tools::all(cfg.data_root(), Some(embedder as Arc<dyn data_core::vector::Embedder>)) {
        let n = t.name().to_string();
        r.add(&n, t)?;
    }
    if !r.skipped.is_empty() {
        tracing::info!(skipped = ?r.skipped, "注册表跳过了未启用/未安装的内置工具");
    }
    Ok(())
}
pub fn apply_net_settings(cfg: &AppConfig) {
    net_core::settings::install(net_core::NetSettings {
        timeout_secs: cfg.net.timeout_secs,
        connect_timeout_secs: cfg.net.connect_timeout_secs,
        user_agent: cfg.net.effective_user_agent(),
        proxy: cfg.net.proxy.clone().filter(|p| !p.trim().is_empty()),
        concurrency: cfg.net.concurrency,
        per_host_qps: cfg.net.per_host_qps,
    });
}

/// 把配置里的 python/pip 路径注入环境变量（#13.1）。
/// 留空则清掉变量，让 pysandbox 回退自动探测——这样"删掉路径"能立即生效，
/// 不必重启进程才恢复默认。
pub fn apply_python_settings(cfg: &AppConfig) {
    match cfg.sandbox.python_path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => std::env::set_var("WPA_PYTHON_PATH", p),
        _ => std::env::remove_var("WPA_PYTHON_PATH"),
    }
    match cfg.sandbox.pip_path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => std::env::set_var("WPA_PIP_PATH", p),
        _ => std::env::remove_var("WPA_PIP_PATH"),
    }
}

/// 进程级装配。
pub async fn assemble(cfg: &AppConfig, _cli_tree: Option<PathBuf>) -> anyhow::Result<Base> {
    // 配置先自检：非法配置不进入装配（fail-closed）
    let cfg_errs = cfg.validate();
    if !cfg_errs.is_empty() {
        anyhow::bail!("配置校验失败：\n  {}", cfg_errs.join("\n  "));
    }

    let kern = Arc::new(Registry::new());
    // 网络参数安装：net.* 工具由内核统一构造，拿不到配置引用，只能由装配层灌进去。
    // 放在装配最前面 —— 任何工具第一次调用前生效。
    apply_net_settings(cfg);
    // python/pip 可配置路径（#13.1）：写进环境变量供 pysandbox 消费；
    // 留空时不设置，由 pysandbox 回退到"程序子目录自动探测"。
    // 用 set_var 是进程级的，但装配只在启动/会话边界发生，且值来自同一份配置。
    apply_python_settings(cfg);
    // 风险规则注入：基座内置"只读探测"白名单。
    // 不注入的话所有命令都判 High，再撞上知识约束的 min_risk=high 规则——
    // 结果就是"授权了仍被拦"（用户实测的核心问题）。
    toolkit_core::risk_policy::install_rules(toolkit_core::risk_policy::builtin_rules());
    let workspaces = Arc::new(
        WorkspaceRegistry::open(cfg.workspaces_root())
            .map_err(|e| anyhow::anyhow!("打开空间注册表失败：{e}"))?,
    );
    let shells = ShellSessions::new();
    let jobs = JobStore::new();
    let mut warnings = Vec::new();

    // 注册表：**全局唯一**，与领域无关。它是「组 ⇄ 插件」的管理树，
    // 组启用/插件装卸都是全局状态；领域只在 `domain.toml` 里引用组 id。
    // 装配顺序：先做一次旧账本合并（幂等，无旧目录时零开销），再加载。
    let data_root = cfg.data_root();
    let reg_path = crate::registry::registry_path(&data_root);
    match crate::registry::consolidate(&data_root) {
        Ok(rep) if !rep.is_empty() => {
            tracing::info!(report = %rep.describe(), "旧领域账本已合并进全局注册表");
            if let Some(p) = &rep.archived {
                tracing::info!(path = %p.display(), "旧账本目录已归档");
            }
        }
        Ok(_) => {}
        Err(e) => warnings.push(format!("旧注册表合并失败（已跳过，按全局账本继续）：{e}")),
    }
    let (mut tool_reg, reg_created) = crate::registry::ToolRegistry::load_or_init(&reg_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if reg_created {
        tracing::info!(path = %reg_path.display(), "已按内置工具生成注册表");
    }
    // 向量库的 embedding 档案槽：模型切换时由上层刷新，不重新装配工具也能生效
    let embedder = crate::embedder::SharedEmbedder::new(cfg);
    register_base_tools(&kern, cfg, &workspaces, &shells, &jobs, &tool_reg, embedder.clone())?;

    // 执行器只需 kern，提前建好，规划层/协作层注册时要用
    let executor: Arc<dyn ToolExecutor> = Arc::new(ToolkitExecutor::new(kern.clone()));

    // 规划层（plan.*）：状态与工具共享同一个 Arc，模型改的进度会话立刻可见
    //
    // 三档行为由 `[agent].plan_mode` 一处决定（见 `PlanMode`）：
    //   off    → 一个都不装，会话也不注入计划视图
    //   auto   → 装 create/update/view，模型自主决定何时建计划
    //   always → 再加评审门 plan.exit：复杂动作先出计划待人工批准
    let plan = agent_core::plan_state::shared();
    let plan_mode = cfg.agent.plan_behavior();
    if plan_mode != PlanMode::Off {
        let mut plan_tools: Vec<Arc<dyn toolkit_core::tools::Tool>> = vec![
            Arc::new(agent_core::plan_state::PlanCreateTool::new(plan.clone())),
            Arc::new(agent_core::plan_state::PlanUpdateTool::new(plan.clone())),
            Arc::new(agent_core::plan_state::PlanViewTool::new(plan.clone())),
        ];
        if plan_mode == PlanMode::Always {
            // 评审门：High 风险 ⇒ 必走审批链
            plan_tools.push(Arc::new(agent_core::ExitPlanTool));
        }
        for t in plan_tools {
            if let Err(e) = kern.register_tool(t) {
                warnings.push(format!("注册 plan 工具失败：{e}"));
            }
        }
    }

    // 协作层（subagent.spawn）：provider 用可替换槽，换模型后子代理跟着换
    let provider_cell: agent_core::subagent::ProviderCell =
        Arc::new(std::sync::RwLock::new(None));
    if cfg.agent.subagent_enabled {
        let sub = agent_core::SubagentTool::new(
            provider_cell.clone(),
            kern.clone(),
            executor.clone(),
            cfg.scope.authorized.clone(),
            cfg.agent.subagent_max_rounds,
        );
        if let Err(e) = kern.register_tool(Arc::new(sub)) {
            warnings.push(format!("注册 subagent.spawn 失败：{e}"));
        }
    }

    // 存储（L3 经验库 / 任务库）
    let _ = std::fs::create_dir_all(&data_root);
    let tasks = Arc::new(
        base_store::TaskStore::open(data_root.join("tasks"))
            .map_err(|e| anyhow::anyhow!("打开任务库失败：{e}"))?,
    );
    let experience = Arc::new(
        base_store::ExperienceStore::open(data_root.join("experience"))
            .map_err(|e| anyhow::anyhow!("打开经验库失败：{e}"))?,
    );
    // 会话库：逐轮落盘 + 跨会话检索（session.search）。
    // 打不开就降级 —— 记忆是增强能力，不该让基座起不来。
    let sessions = match base_store::SessionStore::open(data_root.join("sessions")) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            warnings.push(format!("会话库打开失败，跨会话检索不可用：{e}"));
            None
        }
    };
    if let Some(store) = sessions.clone() {
        if let Err(e) = kern.register_tool(Arc::new(
            toolkit_core::builtin::SessionSearchTool::new(store),
        )) {
            warnings.push(format!("注册 session.search 失败：{e}"));
        }
    }

    let session_id = SessionId::new();
    let runtime = Arc::new(BaseRuntime::new(session_id.to_string(), Some(data_root.clone())));

    let mut api = GroupRegistry::new();
    let phases: Vec<wpa_api::Phase> = Vec::new();

    let linked = host::linked_groups();

    // 插件宿主（plugins/）：扫描只读清单，一个 dll 都不打开；
    // 装不装、开不开由注册表（P0-1）决定，加载发生在 build_tools()。
    let mut plugin_host: Option<plugin_host::PluginHost> = None;
    let plugins_root = cfg.plugins_root();
    if plugins_root.is_dir() {
        // 自举：先把 `plugins/` 根下的裸 dll 收编进 `unassigned/`，再扫描。
        // 顺序不能反——扫描只读、只认目录，裸文件在根目录下永远不会成为候选。
        let boot = plugin_host::bootstrap(&plugins_root, cfg.plugins.auto_probe);
        if !boot.adopted.is_empty() {
            tracing::info!(report = %boot.describe(), "已收编 plugins 根目录的裸插件");
        }
        for p in &boot.problems {
            warnings.push(format!("插件自举：{p}"));
        }
        let env = Arc::new(crate::plugin_rt::PluginEnv::new(
            tokio::runtime::Handle::current(),
            data_root.clone(),
        ));
        plugin_host = Some(plugin_host::PluginHost::open(
            &plugins_root,
            crate::plugin_rt::factory(env),
        ));
    }

    if let Some(ref host) = plugin_host {
        // 对账：只更新已安装条目的状态（ok / broken），**不新增**。
        // 候选不落库——它们只在快照里可见，用户点"安装"才进注册表。
        let snap = crate::registry::snapshot_from_groups(host.groups());
        let rep = tool_reg.reconcile(&snap);
        if !rep.is_empty() {
            tracing::info!(report = %rep.describe(), "注册表已与扫描结果对账");
            if let Err(e) = tool_reg.save(&reg_path) {
                warnings.push(format!("注册表保存失败：{e}"));
            }
        }
        // 只有「装了 + 条目启用 + 组启用」的条目才允许被 dlopen。
        // 这是硬闸门：未安装的 dll 连库都不会被打开（不是加载后再拒）。
        let wanted: std::collections::BTreeSet<String> = tool_reg
            .groups
            .iter()
            .filter(|g| g.enabled)
            .flat_map(|g| {
                g.entries
                    .iter()
                    .filter(|e| e.active(true))
                    .map(|e| e.id.clone())
            })
            .collect();
        let (ptools, pproblems) = host.build_tools(&wanted);
        for p in &pproblems {
            warnings.push(p.clone());
        }
        for (gid, t) in ptools {
            let name = t.name().to_string();
            // `is_active` 对"注册表里查不到"的默认放行（为运行时工具留的口子），
            // 未安装的插件同样查不到 ⇒ 必须再用 is_installed 兜一道。
            if !tool_reg.is_installed(&name) || !tool_reg.is_active(&name) {
                tracing::info!(tool = %name, "注册表未安装/未启用，跳过插件工具");
                continue;
            }
            if let Err(e) = api.register_tool_in(t, Some(&gid)) {
                warnings.push(format!("插件工具 {name} 注册失败：{e}"));
            }
        }
    }
    // 契约注册表 → 内核 Registry（隔离区工具不进入）
    let denylist = Arc::new(cfg.scope.denylist.clone());
    let dyn_rt: Arc<dyn GroupRuntime> = runtime.clone();
    for w in host::sync_registry(&kern, &api, dyn_rt, denylist.clone()) {
        tracing::warn!("{w}");
        warnings.push(w);
    }

    // 领域（描述包）：角色 / 工作范式 / 常识 / 红线。
    //
    // 必须在约束收集之前装载：领域的 knowledge.md 与 constraints.toml 要一起进
    // 同一条编译链（否则领域红线编译不成 guard，只会变成一段靠自觉的 prompt）。
    let mut domain_pack: Option<Arc<crate::domain::DomainPack>> = None;
    {
        // 首次运行（整个默认领域目录都不在）时落盘模板：分发包只有一个 exe，
        // 没有这一步用户看到的是"领域不存在"，也就没有可改的样板。
        if cfg.domain.active == "default" && !cfg.active_domain_dir().exists() {
            match crate::domain::ensure_default(&cfg.domains_root()) {
                Ok(w) if !w.is_empty() => {
                    tracing::info!(files = ?w, "已写出默认领域模板");
                }
                Ok(_) => {}
                Err(e) => warnings.push(format!("默认领域模板写出失败：{e}")),
            }
        }
        let (pack, dwarns) = crate::domain::load_active(cfg);
        warnings.extend(dwarns);
        if let Some(p) = pack {
            let has_kb = p
                .manifest
                .knowledge
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            let has_rules = p
                .manifest
                .constraints
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if has_kb || has_rules {
                let entries: Vec<String> = p.manifest.knowledge.clone().into_iter().collect();
                let provider = knowledge_core::FileKnowledgeProvider::from_dir(
                    format!("domain:{}", p.manifest.id),
                    &p.dir,
                    &entries,
                    p.manifest.constraints.clone().as_deref(),
                );
                for w in provider.warnings() {
                    warnings.push(format!("领域知识：{w}"));
                }
                api.register_knowledge(Arc::new(provider));
            }
            tracing::info!(
                domain = %p.manifest.id,
                groups = p.manifest.groups.len(),
                knowledge = p.knowledge_entries,
                "领域已装载"
            );
            // 领域**私有**外部工具：只在这个领域生效，不进 plugins/、不进全局注册表、
            // 不出现在能力页。领域切换 = 这批工具跟着换掉。
            let invoker: Option<Arc<dyn tree_core::exec::PrimitiveInvoker>> =
                Some(Arc::new(crate::tools::KernPrimitiveInvoker::new(kern.clone(), session_id)));
            let (ext_tools, ext_problems) =
                crate::domain::external_tools(&p.dir, &p.external, &p.manifest.id, invoker);
            let ext_group = format!("ext:{}", p.manifest.id);
            for t in &ext_tools {
                // 用独立群组登记：不挤占任何已引用组，也便于 UI 区分来源
                if let Err(e) = api.register_tool_in(t.clone(), Some(&ext_group)) {
                    warnings.push(format!("领域外部工具 {} 注册失败：{e}", t.name()));
                }
            }
            if !ext_tools.is_empty() {
                tracing::info!(
                    domain = %p.manifest.id,
                    count = ext_tools.len(),
                    "领域外部工具已注册"
                );
            }
            for w in ext_problems {
                warnings.push(w);
            }
            domain_pack = Some(Arc::new(p));
        }
    }

    // 知识约束：从所有 provider 收集并编译为可执行 guard
    let mut raw_constraints = Vec::new();
    if !api.knowledge().is_empty() {
        let q = wpa_api::ConstraintQuery {
            task: String::new(),
            stage: phases.first().map(|p| p.id.clone()),
            // 用全量工具名求约束并集：宁可多给约束，也不能因为当前工具面窄而漏掉红线
            tools: kern.tools(),
        };
        for p in api.knowledge() {
            raw_constraints.extend(p.constraints(&q).await);
        }
    }
    let (constraint_set, cw) =
        tree_core::ConstraintSet::from_constraints(&raw_constraints, tree_core::compile_default);
    warnings.extend(cw);
    if !raw_constraints.is_empty() {
        tracing::info!(
            total = raw_constraints.len(),
            guards = constraint_set.rules().len(),
            "知识约束已编译"
        );
    }
    let constraints = Arc::new(constraint_set);

    // 基座内建工具（需要装配结果）：知识检索 / 阶段推进
    if !api.knowledge().is_empty() {
        let providers = api.knowledge().to_vec();
        if let Err(e) = kern.register_tool(Arc::new(KbSearch::new(providers))) {
            warnings.push(format!("注册 kb.search 失败：{e}"));
        }
    }
    let shared_phases = tree_core::phases::shared(phases.clone());
    if let Some(first) = phases.first() {
        // 树声明了阶段 ⇒ 从第一阶段起步。这是**确定性初始化**，不是隐式推断：
        // 让"先受理再动手"成为默认行为，而不是等模型想起来推进阶段。
        // 阶段外的业务工具此时不进 schema，模型只能用基座工具 + plan.phase。
        let first_id = first.id.clone();
        match shared_phases.lock() {
            Ok(mut g) => {
                if let Err(e) = g.advance(Some(&first_id)) {
                    warnings.push(format!("初始化阶段 {first_id} 失败：{e}"));
                }
            }
            Err(_) => warnings.push("阶段状态锁中毒，未能初始化起始阶段".into()),
        }
        if let Err(e) = kern.register_tool(Arc::new(PlanPhase::new(shared_phases.clone()))) {
            warnings.push(format!("注册 plan.phase 失败：{e}"));
        }
    }

    Ok(Base {
        kern,
        executor,
        api,
        runtime,
        session_id,
        linked,
        workspaces,
        shells,
        jobs,
        tasks,
        experience,
        denylist,
        cfg: cfg.clone(),
        warnings,
        phases: shared_phases,
        constraints,
        registry: Arc::new(std::sync::RwLock::new(tool_reg)),
        plugins: plugin_host.map(Arc::new),
        domain: domain_pack,
        plan,
        provider_cell,
        sessions,
        embedder,
    })
}

// ---------------------------------------------------------------------------
// 会话构建
// ---------------------------------------------------------------------------

/// 会话构建选项。
pub struct SessionOpts {
    /// 授权目标（CLI `--target` 覆盖）
    pub targets: Vec<String>,
    /// 审批回调（CLI=终端询问，UI=审批卡片）
    pub approval: ApprovalCallback,
    /// 过程事件回调（UI 实时上屏）
    pub events: Option<ChatEventCallback>,
    /// 自动批准（自动化场景）
    pub auto_approve: bool,
}

/// 会话装配结果摘要（UI 状态栏/审计）。
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub space: String,
    pub model: String,
    pub models_chain: Vec<String>,
    pub tool_count: usize,
    pub agent_mode: bool,
    pub context_window: u64,
    pub scope: Vec<String>,
    pub skills: usize,
    /// 激活领域 id（空 = 纯基座模式）
    pub domain_id: String,
    /// 激活领域展示名（空 = 纯基座模式）
    pub domain_name: String,
    pub warnings: Vec<String>,
}

/// 组 id → 工具名映射（供空间白名单按组展开）。
fn group_tool_map(api: &GroupRegistry) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in api.tool_schemas() {
        if let Some(g) = s.group {
            map.entry(g).or_default().push(s.name);
        }
    }
    map
}

/// 会话级装配：空间生效面 + 模型链 + 上下文预算 + 记忆注入。
pub fn build_session(base: &Base, opts: SessionOpts) -> (ChatSession, SessionInfo) {
    let cfg = &base.cfg;
    let mut warnings: Vec<String> = Vec::new();

    // 1) 空间生效面
    let active: Option<Workspace> = base.workspaces.active();
    let space_name = active.as_ref().map(|w| w.name.clone()).unwrap_or_default();

    // 2) 授权范围：CLI > 空间权限模式 > 配置
    //    空间权限只有两档：
    //      - 默认全局：直接继承全局授权/红线（空间不再有自己的授权目标）
    //      - 允许完全访问：授权通配一切（`*`）+ 清空红线 + 跳过约束钩子与审批
    //    注意空清单是 fail-closed，完全访问必须靠显式 `*` 规则表达。
    let cli_targets = &opts.targets;
    let unrestricted = active.as_ref().map(|w| w.unrestricted).unwrap_or(false);
    let (scope, deny) = if !cli_targets.is_empty() {
        (cli_targets.clone(), base.denylist.as_ref().clone())
    } else if let Some(w) = active.as_ref() {
        (
            w.effective_scope(&cfg.scope.authorized),
            w.effective_deny(&base.denylist),
        )
    } else {
        (cfg.scope.authorized.clone(), base.denylist.as_ref().clone())
    };

    // 2.5) 沙箱开关随空间生效：None = 工具沿用全局默认，Some = 强制覆盖。
    //      工具在执行时读取该全局标记（装配期无法预知之后切到哪个空间）。
    toolkit_core::sandbox::set_space_sandbox(
        active.as_ref().and_then(|w| w.sandbox_enabled),
    );

    // 3.5) 授权范围自检：写错的条目在装配期就报出来，不用等调工具时才发现。
    // 解析规则与执行层完全一致（ScopeGuard::from_lists_lenient），避免两处口径不同。
    let (_, scope_notes) = kern_core::ScopeGuard::from_lists_lenient(&scope, &deny);
    if !scope_notes.is_empty() {
        for n in &scope_notes {
            tracing::warn!("{n}");
        }
        warnings.extend(scope_notes);
    }

    // 4) 模型链：空间绑定 > 激活档案 > fallback > 其余档案
    let preferred = active.as_ref().map(|w| w.llm_profile.clone()).unwrap_or_default();
    let preferred = (!preferred.trim().is_empty()).then_some(preferred.as_str());
    if let (Some(w), Some(name)) = (active.as_ref(), preferred) {
        if cfg.llm.profiles.iter().all(|p| p.name != name) {
            warnings.push(format!("空间 {} 绑定的模型档案不存在：{name}（已回退全局）", w.name));
        }
    }
    let chain_profiles = cfg.llm.resolve_chain(preferred);
    let (provider, w) = FallbackProvider::build(&chain_profiles);
    warnings.extend(w);
    // 子代理共用同一份 provider：换模型（重建会话）时这里一并更新
    if let Ok(mut cell) = base.provider_cell.write() {
        *cell = provider.clone();
    }
    if provider.is_none() {
        warnings.push("未配置可用模型：进入降级模式（仅通用命令）".into());
    }
    let primary = chain_profiles.first().cloned();

    // 5) 工具面：空间 plugins 白名单（支持组 id 展开）→ 阶段收敛 → 模型能力
    let (all_schemas, risks) = host::collect_tool_schemas(&base.kern);
    let all_names: Vec<String> = all_schemas.iter().map(|s| s.name.clone()).collect();
    let group_map = group_tool_map(&base.api);
    let mut tool_group: HashMap<String, String> = HashMap::new();
    for (g, tools) in &group_map {
        for t in tools {
            tool_group.insert(t.clone(), g.clone());
        }
    }
    // 基座工具的组归属来自 catalog（契约组优先：插件/树的组能覆盖同名工具）
    for s in &all_schemas {
        if !tool_group.contains_key(&s.name) {
            if let Some(g) = crate::catalog::group_of(&s.name) {
                tool_group.insert(s.name.clone(), g.to_string());
            }
        }
    }
    let mut allow: Vec<String> = match &active {
        Some(ws) => ws.effective_tools(&group_map, &all_names),
        None => all_names.clone(),
    };
    // 基础组（core/shell/sandbox/space/memory/kb）永远可用。
    // 空间白名单只决定"额外"放行哪些可配置组，不能因为没勾就把基座的运行前提砍掉
    // ——那样用户会得到一个"记忆、知识检索凭空消失"的系统，且从界面上看不出原因。
    for name in &all_names {
        if let Some(g) = tool_group.get(name) {
            if crate::catalog::is_base_group(g) && !allow.contains(name) {
                allow.push(name.clone());
            }
        }
    }
    for name in &allow {
        if !all_names.contains(name) {
            warnings.push(format!("空间白名单引用了不存在的组或工具：{name}"));
        }
    }
    let schemas: Vec<ToolSchema> = all_schemas
        .iter()
        .filter(|s| allow.contains(&s.name))
        .cloned()
        .collect();

    // 5.5) 领域工具面：`domain.toml` 的 `groups` 声明「这个领域要用哪些组」。
    //
    // 四条硬规则（顺序执行，且必须都满足才"可见"）：
    //   0. **基础组永远保留**（core/shell/sandbox/space/memory/kb）。它们是基座的
    //      运行前提，不参与领域装配——用户在 `groups` 里写不写都不影响。
    //   1. 智能体层工具（plan.* / subagent.spawn）永不受领域约束 —— 它们由
    //      [agent] 分区控制。让领域能关掉规划层，等于埋一个"计划消失"的坑。
    //   2. 能判定归属、且组不在声明里 ⇒ 剔除。这是领域收敛的本意。
    //   3. 无法判定归属 ⇒ **保留**。宁可多给一个工具，也不要因为归属表漏登记
    //      让某个能力凭空消失（用户看不到工具只会以为"坏了"）。
    // 另外 `[tool_filter] drop` 在这里一并生效（领域主动剔除）。
    let mut hidden_by_domain = 0usize;
    let mut domain_drop: Vec<String> = Vec::new();
    let schemas: Vec<ToolSchema> = if let Some(d) = base.domain.as_ref() {
        let declared = &d.manifest.groups;
        domain_drop = d.tool_drop.clone();
        // 声明了但一个工具都不对应 ⇒ 多半是写错了组名，必须报出来
        let known: Vec<String> = base.kern.tools();
        for g in declared {
            if crate::catalog::is_base_group(g) {
                // 基础组不需要声明（旧文件里还写着也照常工作，只是不再生效）
                continue;
            }
            let hits = known
                .iter()
                .filter(|t| tool_group.get(*t).map(|x| x == g).unwrap_or(false))
                .count();
            if hits == 0 {
                warnings.push(format!(
                    "领域 {} 声明了组 `{g}`，但没有任何工具属于它（组名写错？或该组尚未实现）",
                    d.manifest.id
                ));
            }
        }
        schemas
            .into_iter()
            .filter(|s| {
                if crate::catalog::is_agent_tool(&s.name) {
                    return true;
                }
                if domain_drop.contains(&s.name) {
                    hidden_by_domain += 1;
                    return false;
                }
                match tool_group.get(&s.name) {
                    // 基础组：不论领域是否声明都保留
                    Some(g) if crate::catalog::is_base_group(g) => true,
                    Some(g) if !declared.contains(g) => {
                        hidden_by_domain += 1;
                        false
                    }
                    _ => true,
                }
            })
            .collect()
    } else {
        schemas
    };
    if hidden_by_domain > 0 {
        tracing::info!(hidden = hidden_by_domain, "按领域工具面收敛");
    }
    if !domain_drop.is_empty() {
        tracing::info!(drop = ?domain_drop, "领域 tool_filter.drop 生效");
    }

    // 阶段收敛（S-6）：这里只**核算**本阶段可见面，不物理裁剪 schemas。
    //
    // 为什么不裁：会话装了 per-round 过滤器（见第 8 步），模型中途用 plan.phase
    // 推进后工具面要能"回来"。若建会话时就把阶段外工具删掉，之后推进也只能看到
    // 剩下的那把，等于阶段只能单向收缩。
    let mut phase_prompt: Option<String> = None;
    let mut hidden_by_phase = 0usize;
    match base.phases.lock() {
        Ok(g) => {
            let visible: Vec<String> = g.visible_tools(
                schemas
                    .iter()
                    .map(|s| (s.name.as_str(), tool_group.get(&s.name).map(|v| v.as_str()))),
            );
            hidden_by_phase = schemas.iter().filter(|s| !visible.contains(&s.name)).count();
            if hidden_by_phase > 0 {
                tracing::info!(
                    phase = ?g.current_id(),
                    visible = schemas.len() - hidden_by_phase,
                    hidden = hidden_by_phase,
                    "按阶段收敛工具面"
                );
            }
            phase_prompt = g.prompt_section();
        }
        Err(_) => warnings.push("阶段状态锁中毒，本轮不收敛工具面".into()),
    }

    // 模型能力：不支持 function calling（或 chat 模式）→ 不下发工具面
    let tools_enabled = primary.as_ref().map(|p| p.tools_enabled()).unwrap_or(true);
    let agent_mode = cfg.llm.mode != "chat" && tools_enabled;
    if !tools_enabled {
        if let Some(p) = &primary {
            warnings.push(format!(
                "模型档案 {} 不支持 function calling，本轮不发送工具面（纯对话模式）",
                p.name
            ));
        }
    }
    // 不支持工具调用时整体清空：保留 schema 只会让 prompt 里出现"可用工具"却调不通
    let schemas: Vec<ToolSchema> = if tools_enabled { schemas } else { Vec::new() };

    // 6) 技能：树提供的技能包（空间可用技能名过滤 + 依赖闸 + 关键词预筛素材）
    //
    // 依赖闸在这里做：`requires` 里的工具组没装配成功，该技能不进目录。
    // 关键词预筛交给会话层（每轮输入不同），这里只把 keywords 递下去。
    // 已装配成功的群组 id（含知识/逻辑群组——它们不一定带工具）
    let installed_groups: Vec<String> =
        base.api.groups().iter().map(|g| g.id.clone()).collect();
    let mut notes: Vec<SkillNote> = base
        .api
        .skills()
        .into_iter()
        .filter(|s| {
            active
                .as_ref()
                .map(|w| w.skills.is_empty() || w.skills.contains(&s.name))
                .unwrap_or(true)
        })
        .filter(|s| {
            let ok = s.deps_ok(&installed_groups);
            if !ok {
                warnings.push(format!(
                    "技能 {} 依赖的工具组未装配（需要：{}），本轮不进技能目录",
                    s.name,
                    s.requires.join("、")
                ));
            }
            ok
        })
        .map(|s| SkillNote {
            name: s.name,
            trigger: s.trigger,
            body: s.body,
            keywords: s.keywords,
            available: true,
        })
        .collect();
    if notes.is_empty() {
        notes = Vec::new();
    }

    // 7) 注入段（按优先级）：知识约束 → 领域工作范式 → 当前阶段 → 树的 prompt → 知识目录 → 空间记忆
    let mut prompt_sections: Vec<String> = Vec::new();
    if let Some(sec) = base.constraints.prompt_section() {
        prompt_sections.push(sec);
    }
    // 计划模式 = always：把"先出计划再动手"写成硬要求（否则 plan.exit 只是个可选工具）
    if cfg.agent.plan_behavior() == PlanMode::Always {
        prompt_sections.push(
            "## 计划门（本次会话强制）\n\
             收到任务后先调用 `plan.create` 把步骤拆成可执行条目（每条一句话、可验收），\
             再用 `plan.exit` 提交待批；未获批准不要调用任何有副作用的工具。\
             执行过程中用 `plan.update` 推进状态，受阻标 `blocked` 并说明原因。"
                .to_string(),
        );
    }
    // 领域工作范式紧跟在红线之后：先知道"不许做什么"，再知道"该怎么做"
    if let Some(sec) = base.domain.as_ref().and_then(|d| d.workflow_section()) {
        prompt_sections.push(sec);
    }
    if let Some(sec) = phase_prompt {
        prompt_sections.push(sec);
    }
    prompt_sections.extend(base.api.prompt_sections().iter().cloned());
    if !base.api.knowledge().is_empty() {
        let names: Vec<String> = base.api.knowledge().iter().map(|p| p.name().to_string()).collect();
        prompt_sections.push(format!(
            "## 领域知识库\n已装入知识库：{}。**涉及具体技术事实前先调用 `kb.search` 查证**，查不到再说不知道。",
            names.join("、")
        ));
    }
    if cfg.memory.inject_curated && !space_name.is_empty() {
        let curated = base.workspaces.read_memory_curated(&space_name);
        let text: String = curated.chars().take(cfg.memory.curated_chars).collect();
        if !text.trim().is_empty() {
            prompt_sections.push(format!("## 项目记忆（空间 {space_name}）\n{text}"));
        }
    }

    // 7.5) 上下文预算：装订前先核算，超出即按段截断（先裁注入段，不动骨架）
    let context_window = primary
        .as_ref()
        .map(|p| p.context_window_or_default() as u64)
        .unwrap_or(32_000);
    let budget = cfg.context.to_budget();
    let persona_tokens = agent_core::estimate_text_tokens(base.persona().unwrap_or(""));
    let tools_tokens: usize = schemas
        .iter()
        .map(|s| {
            agent_core::estimate_text_tokens(&s.name)
                + agent_core::estimate_text_tokens(&s.description)
                + agent_core::estimate_text_tokens(&serde_json::to_string(&s.parameters).unwrap_or_default())
        })
        .sum();
    let injected_tokens: usize =
        prompt_sections.iter().map(|s| agent_core::estimate_text_tokens(s)).sum();
    let load = crate::context::Load {
        system: (persona_tokens + injected_tokens) as u64,
        knowledge: 0,
        experience: cfg.memory.experience_token_budget as u64,
        tools: tools_tokens as u64,
        dialog: 0,
    };
    let ctx_plan = crate::context::plan(context_window, &budget, &load);
    warnings.extend(ctx_plan.warnings.iter().cloned());
    if !ctx_plan.has_room() {
        warnings.push(format!(
            "装订前估算占用 {} / {} tokens（含工具 schema）；对话空间会被压缩机制进一步收缩",
            ctx_plan.used_total(),
            ctx_plan.window
        ));
    }
    // 注入段按 system 预算裁剪（约束/阶段段在最前，天然优先保留）
    let system_cap = ctx_plan.slice("system").map(|s| s.cap).unwrap_or(u64::MAX);
    let inject_cap = system_cap.saturating_sub(persona_tokens as u64);
    let mut used = 0u64;
    let mut kept: Vec<String> = Vec::new();
    for sec in prompt_sections.drain(..) {
        let t = agent_core::estimate_text_tokens(&sec) as u64;
        if used + t <= inject_cap {
            used += t;
            kept.push(sec);
        } else {
            let remain = inject_cap.saturating_sub(used);
            if remain >= 64 {
                let (fitted, _) = crate::context::fit_text(&sec, remain);
                kept.push(fitted);
            }
            warnings.push(format!(
                "注入段超出 system 预算（{inject_cap} tokens），后续注入已截断/丢弃"
            ));
            break;
        }
    }
    let prompt_sections = kept;

    // 8) 会话配置
    // 轮数优先级：领域参数（专业流程需要更多轮）> [agent] > [llm]（旧字段）
    let domain_rounds = base
        .domain
        .as_ref()
        .map(|d| d.params.max_tool_rounds)
        .unwrap_or(0);
    let max_tool_rounds = if domain_rounds > 0 {
        domain_rounds.clamp(1, 64)
    } else {
        cfg.agent.effective_rounds(cfg.llm.max_tool_rounds)
    };
    let chat_cfg = ChatConfig {
        session_id: base.session_id,
        authorized_scope: scope.clone(),
        denylist: deny,
        max_tool_rounds,
        agent_mode,
        context_window_tokens: context_window,
        // 以下全部来自「配置 → 内置能力」卡片，不再吃 ChatConfig 的硬编码默认值
        history_cap: cfg.context.history_cap,
        summarize_history: cfg.context.summarize_history,
        extract_every: cfg.context.extract_every,
        compact_threshold: cfg.context.compact_threshold,
        keep_recent_turns: cfg.context.keep_recent_turns,
        experience_inject: cfg.memory.experience_inject,
        experience_token_budget: cfg.memory.experience_token_budget,
        reflect: cfg.agent.reflect,
        reflect_min_failures: cfg.agent.reflect_min_failures,
        persona: base.persona().map(|s| s.to_string()),
        prompt_sections,
        ..Default::default()
    };

    // 对外报告的是**本阶段真正下发的**工具数（不是注册总数）
    let tool_count = schemas.len().saturating_sub(hidden_by_phase);
    let mut session = ChatSession::new(
        provider.clone(),
        base.executor.clone(),
        schemas,
        risks,
        chat_cfg,
        opts.approval,
    );
    // 9) 存储与记忆接线（L1 快照 / L3 经验 / 任务板 / 大结果转存）
    session = session
        .with_tasks(base.tasks.clone())
        .with_memory(base.experience.clone())
        .with_spill(Arc::new(base_store::SpillStore::with_data_root(cfg.data_root())));
    // 计划视图只在计划层真的装起来时才注入：off 档塞一段空计划只会误导模型
    if cfg.agent.plan_behavior() != PlanMode::Off {
        session = session.with_plan(base.plan.clone());
    }
    // 会话库：/recall 召回 + 逐轮落盘（同一个 store，读写一致）
    if let Some(store) = base.sessions.clone() {
        session = session.with_sessions(store);
    }
    // 知识约束钩子（S-5）：denylist 之后、审批之前裁决；只可能更严，不会更松。
    // 完全访问模式下不装钩子——用户显式选择了"解除全部限制"，红线一并放开。
    if !unrestricted && !base.constraints.rules().is_empty() {
        session = session.with_hook(Arc::new(ConstraintHook::new(base.constraints.clone())));
    }
    // 审批短路：完全访问或自动化场景（auto_approve）一次性全部放行，不再逐次询问。
    if unrestricted || opts.auto_approve {
        session = session.with_approval_mode(Some(true));
    }
    if let Some(w) = &active {
        if !w.cwd.trim().is_empty() {
            session = session.with_workspace_root(Some(PathBuf::from(&w.cwd)));
        }
    }
    if !notes.is_empty() {
        session = session.with_skills(notes.clone());
    }
    // 阶段收敛过滤器（S-6 的**持续生效**形态）：建会话时裁一次只能管到下一次重建，
    // 模型中途用 plan.phase 推进后，工具面必须当场跟着变——所以在每轮调用前求值。
    if !base.phases.lock().map(|g| g.is_empty()).unwrap_or(true) {
        let phases = base.phases.clone();
        let group_of = tool_group.clone();
        session = session.with_tool_filter(Arc::new(move |tool: &str| {
            // 基座工具（无归属组）不受阶段约束：收掉会让模型连编码/查知识都做不了
            let Some(group) = group_of.get(tool) else {
                return true;
            };
            match phases.lock() {
                Ok(g) => g.allows_group(group),
                Err(_) => true, // 锁中毒时保守放行（不因状态故障扩大封锁面）
            }
        }));
    }
    if let Some(cb) = opts.events {
        session = session.with_events(cb);
    }

    let info = SessionInfo {
        space: space_name,
        model: primary.as_ref().map(|p| p.model.clone()).unwrap_or_default(),
        models_chain: chain_profiles.iter().map(|p| p.name.clone()).collect(),
        tool_count,
        agent_mode,
        context_window,
        scope,
        skills: notes.len(),
        domain_id: base.domain.as_ref().map(|d| d.manifest.id.clone()).unwrap_or_default(),
        domain_name: base.domain.as_ref().map(|d| d.manifest.name.clone()).unwrap_or_default(),
        warnings,
    };
    (session, info)
}

// ---------------------------------------------------------------------------
// 快照（UI 数据源）
// ---------------------------------------------------------------------------

impl Base {
    /// 只读快照：工具面 / 群组 / 隔离区 / 空间 / 树构成。
    pub fn snapshot(&self) -> Snapshot {
        let tools: Vec<ToolRow> = host::collect_tool_schemas(&self.kern)
            .0
            .into_iter()
            .map(|s| {
                let risk = self
                    .kern
                    .tool(&s.name)
                    .map(|t| t.risk())
                    .unwrap_or(RiskLevel::Low);
                ToolRow {
                    group: self
                        .api
                        .tool_schemas()
                        .into_iter()
                        .find(|x| x.name == s.name)
                        .and_then(|x| x.group)
                        // 基座工具没有"契约组"，但能力归属在 catalog 里有唯一真相；
                        // 都不认识的（智能体层 plan.* / subagent.spawn）标 agent。
                        .or_else(|| crate::catalog::group_of(&s.name).map(str::to_string))
                        .unwrap_or_else(|| {
                            if crate::catalog::is_agent_tool(&s.name) {
                                "agent".to_string()
                            } else {
                                "base".to_string()
                            }
                        }),
                    name: s.name,
                    description: s.description.lines().next().unwrap_or("").to_string(),
                    risk: format!("{risk:?}"),
                }
            })
            .collect();


        let skills: Vec<SkillRow> = self
            .api
            .skills()
            .into_iter()
            .map(|s| SkillRow {
                name: s.name,
                trigger: s.trigger,
                domain: s.domain.unwrap_or_default(),
            })
            .collect();

        let active = self.workspaces.active_name().unwrap_or_default();
        let global_sandbox = self.cfg.sandbox.restricted_token;
        let spaces: Vec<SpaceRow> = self
            .workspaces
            .list()
            .into_iter()
            .map(|w| SpaceRow {
                active: w.name == active,
                cwd: w.cwd.clone(),
                trees: w.trees.clone(),
                whitelist: w.plugins.clone(),
                permission: w.permission_label().to_string(),
                sandbox: w.sandbox_label(global_sandbox).to_string(),
                plugins: w.plugins.len(),
                skills: w.skills.len(),
                note: w.note.clone(),
                name: w.name,
            })
            .collect();

        // 领域页：列出 `domains/` 下全部领域，并按"当前注册表 ∩ 该领域声明的组"
        // 预估工具面大小。这一步不重新装配 —— 用户需要的是"切过去能看到什么"，
        // 不是"现在就切过去"。
        let domains: Vec<DomainCardRow> =
            crate::domain::list(&self.cfg.domains_root(), &self.cfg.domain.active)
                .into_iter()
                .map(|d| {
                    let tools = if d.error.is_some() {
                        0
                    } else {
                        crate::snapshot::tool_surface(&tools, &d.group_ids)
                    };
                    DomainCardRow {
                        id: d.id,
                        name: d.name,
                        kind: d.kind,
                        summary: d.summary,
                        groups: d.group_ids,
                        tools,
                        knowledge_entries: d.knowledge_entries,
                        kb: d.kb,
                        external: d.external,
                        has_workflow: d.has_workflow,
                        active: d.active,
                        error: d.error.unwrap_or_default(),
                    }
                })
                .collect();

        // 能力页：基座组永远列出来（含 0 工具、被领域收敛掉的），
        // 否则用户看到"组 0 个"会以为基座没能力，其实只是本模式没有契约群组。
        let declared: Vec<String> = self
            .domain
            .as_ref()
            .map(|d| d.manifest.groups.clone())
            .unwrap_or_default();
        let cap_groups: Vec<CapGroupRow> = crate::catalog::CAP_GROUPS
            .iter()
            .map(|g| CapGroupRow {
                id: g.id.to_string(),
                label: g.label.to_string(),
                source: g.source.to_string(),
                tools: tools.iter().filter(|t| t.group == g.id).count(),
                // 基础组（core/shell/sandbox/space/memory/kb）装配层无条件保留，
                // 所以它们**永远算已生效**——写成"被领域收敛"会误导用户去 domain.toml 里找开关。
                // 其余组：无领域（纯基座模式）= 不做收敛，全部算声明。
                declared: crate::catalog::is_base_group(g.id)
                    || declared.is_empty()
                    || declared.iter().any(|d| d == g.id),
            })
            .collect();

        // 能力页（注册表视图）：组与条目来自全局 `data/registry.toml`（只记已安装），
        // 再把扫描候选合并进来（未安装、文件齐备 → 可点"安装"）。
        // 与上面的 cap_groups 不同——那是"基座有哪些能力组"（静态），这里才是"装了什么"。
        let (reg_groups, reg_entries, reg_summary) = {
            let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
            let st = reg.stats();
            let mut groups = reg
                .groups
                .iter()
                .map(|g| crate::snapshot::RegGroupRow {
                    id: g.id.clone(),
                    label: g.name.clone(),
                    description: g.description.clone(),
                    source: g.source.clone(),
                    enabled: g.enabled,
                    total: g.entries.len(),
                    active: g.entries.iter().filter(|e| e.active(g.enabled)).count(),
                    broken: g.entries.iter().filter(|e| e.status == "broken").count(),
                    discovered: 0,
                })
                .collect::<Vec<_>>();
            let mut entries = reg
                .groups
                .iter()
                .flat_map(|g| {
                    g.entries.iter().map(move |e| crate::snapshot::RegEntryRow {
                        id: e.id.clone(),
                        group: g.id.clone(),
                        name: e.name.clone(),
                        source: e.source.clone(),
                        risk: e.risk.clone(),
                        status: e.status.clone(),
                        enabled: e.enabled,
                        installed: e.installed,
                        installable: false,
                        group_enabled: g.enabled,
                        note: e.note.clone(),
                        capabilities: e.capabilities.clone(),
                    })
                })
                .collect::<Vec<_>>();

            // 扫描候选：只展示，不落库。用户点"安装"才进注册表。
            let mut candidates = 0usize;
            if let Some(h) = self.plugins.as_ref() {
                for sg in h.groups() {
                    let gid = sg.manifest.id.clone();
                    let missing: Vec<_> = sg
                        .members
                        .iter()
                        .filter(|m| reg.entry(&m.id).is_none())
                        .collect();
                    if groups.iter().all(|x| x.id != gid) {
                        groups.push(crate::snapshot::RegGroupRow {
                            id: gid.clone(),
                            label: if sg.manifest.name.is_empty() {
                                gid.clone()
                            } else {
                                sg.manifest.name.clone()
                            },
                            description: sg.manifest.description.clone(),
                            source: "plugin".to_string(),
                            enabled: false,
                            total: sg.members.len(),
                            active: 0,
                            broken: missing.iter().filter(|m| !m.has_binary()).count(),
                            discovered: missing.len(),
                        });
                    } else if let Some(row) = groups.iter_mut().find(|x| x.id == gid) {
                        // 已存在的组（如内置 file 组里也放了插件）：补上候选计数
                        row.discovered = missing.len();
                    }
                    for m in missing {
                        let has_file = m.has_binary();
                        candidates += 1;
                        entries.push(crate::snapshot::RegEntryRow {
                            id: m.id.clone(),
                            group: gid.clone(),
                            name: m.name.clone(),
                            source: if m.declarative.is_some() {
                                "declarative".to_string()
                            } else {
                                "plugin".to_string()
                            },
                            risk: m.risk.clone(),
                            status: if has_file { "discovered" } else { "broken" }.to_string(),
                            enabled: false,
                            installed: false,
                            installable: has_file,
                            group_enabled: false,
                            note: if has_file {
                                "自动发现，尚未安装".to_string()
                            } else {
                                format!("缺 dll：{}", m.dll_path().display())
                            },
                            capabilities: if m.capabilities.is_empty() {
                                m.tags.clone()
                            } else {
                                m.capabilities.clone()
                            },
                        });
                    }
                }
            }

            let summary = format!(
                "组 {}/{} 启用 · 已装 {}（可用 {} · 缺文件 {}）· 未安装 {}",
                st.groups_enabled, st.groups, st.entries, st.ok, st.broken, candidates
            );
            (groups, entries, summary)
        };

        Snapshot {
            version: env!("CARGO_PKG_VERSION").to_string(),
            model: self
                .cfg
                .llm
                .resolve_chain(None)
                .first()
                .map(|p| p.model.clone())
                .unwrap_or_default(),
            space: active,
            tool_count: tools.len(),
            tools,
            isolated: self.api.isolated_names(),
            missing_impls: Vec::new(),
            spaces,
            skills,
            scope_rules: self.cfg.scope.authorized.clone(),
            warnings: self.warnings.clone(),
            data_root: self.cfg.data_root().display().to_string(),
            domain: self
                .domain
                .as_ref()
                .map(|d| d.manifest.id.clone())
                .unwrap_or_default(),
            domains,
            cap_groups,
            reg_groups,
            reg_entries,
            reg_summary,
        }
    }

    /// 反拓扑序卸载群组（退出时调用）。
    pub async fn shutdown(&self) {
        host::shutdown_groups(&self.linked, &[]).await;
        // 插件卸载：反序 shutdown + 释放 dll
        if let Some(p) = &self.plugins {
            p.unload_all();
        }
    }

    /// 经验库规模（UI 记忆页展示）。
    pub fn experience_count(&self) -> usize {
        self.experience.count()
    }

    /// 任务板统计（UI 右栏看板）。
    pub fn board_counts(&self) -> base_store::BoardCounts {
        self.tasks.board()
    }

    /// 空间记忆：日期列表（新的在前）。
    pub fn memory_dates(&self) -> Vec<String> {
        let space = self.workspaces.active_name().unwrap_or_default();
        if space.is_empty() {
            return Vec::new();
        }
        self.workspaces.list_memory_dates(&space)
    }

    /// 空间记忆：读某日日志。
    pub fn memory_daily(&self, date: &str) -> String {
        let space = self.workspaces.active_name().unwrap_or_default();
        if space.is_empty() {
            return String::new();
        }
        self.workspaces.read_memory_daily(&space, date).unwrap_or_default()
    }

    /// 空间记忆：读精炼笔记。
    pub fn memory_curated(&self) -> String {
        let space = self.workspaces.active_name().unwrap_or_default();
        if space.is_empty() {
            return String::new();
        }
        self.workspaces.read_memory_curated(&space)
    }

    /// 配置路径（UI「打开配置文件」）。
    pub fn config_path(&self) -> PathBuf {
        config::app_root().join("config.toml")
    }

    /// 模型档案列表（模型页）。
    pub fn models(&self) -> Vec<crate::snapshot::ModelRow> {
        let llm = &self.cfg.llm;
        let active = llm
            .active
            .clone()
            .or_else(|| llm.profiles.first().map(|p| p.name.clone()))
            .unwrap_or_default();
        if llm.profiles.is_empty() {
            let cfg = llm.effective();
            if cfg.base_url.trim().is_empty() && cfg.model.trim().is_empty() {
                return Vec::new();
            }
            return vec![crate::snapshot::ModelRow {
                name: "default".into(),
                model: cfg.model,
                base_url: cfg.base_url,
                context_window: 32_000,
                tools_ok: cfg.mode != "chat",
                vision: false,
                active: true,
                enabled: true,
                fallback: false,
            }];
        }
        llm.profiles
            .iter()
            .map(|p| crate::snapshot::ModelRow {
                name: p.name.clone(),
                model: p.model.clone(),
                base_url: p.base_url.clone(),
                context_window: p.context_window_or_default(),
                tools_ok: p.supports_tools(),
                vision: p.supports_vision(),
                active: p.name == active,
                enabled: p.enabled,
                fallback: llm.fallback.contains(&p.name),
            })
            .collect()
    }
}

/// 工具 → 风险表（会话构建用；对外暴露便于测试）。
pub fn risk_table(api: &GroupRegistry) -> HashMap<String, RiskLevel> {
    api.tool_schemas()
        .into_iter()
        .map(|s| (s.name, host::to_kern_risk(s.risk)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端：从仓库里的真树包装配 → 会话 → 阶段收敛。
    ///
    /// 这里跑的是"基座 + 渗透树"的完整链路（清单 → 声明式群组 → 约束编译 →
    /// 技能依赖闸 → 阶段收敛），任何一环断了都会在这里失败。
    #[test]
    fn group_map_groups_tools_by_group_id() {
        let mut api = GroupRegistry::new();
        let tool = Arc::new(StubTool);
        api.register_tool_in(tool.clone(), Some("tools.demo")).unwrap();
        api.register_tool_isolated(tool).unwrap_err(); // 重名拒绝（隔离区也去重）
        let map = group_tool_map(&api);
        assert_eq!(map.get("tools.demo"), Some(&vec!["demo.echo".to_string()]));
    }

    struct StubTool;

    #[async_trait::async_trait]
    impl wpa_api::ApiTool for StubTool {
        fn name(&self) -> &str {
            "demo.echo"
        }
        fn description(&self) -> String {
            "stub".into()
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn risk(&self) -> wpa_api::RiskTier {
            wpa_api::RiskTier::Low
        }
        async fn call(&self, _req: wpa_api::ToolRequest) -> wpa_api::ToolReply {
            wpa_api::ToolReply::ok("ok", 0)
        }
    }
}
