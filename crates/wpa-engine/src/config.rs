//! 基座配置（`config.toml`）。
//!
//! 设计原则（G-1/G-2）：
//! - 全部字段可缺省，缺省即合法（不带配置文件也能跑）；
//! - **不含任何业务字段**：领域策略/规则/知识都在树（`trees/`）里；
//! - 分五个配置域：运行 / 模型 / 空间（`workspaces.toml`）/ 树包（`trees/registry.toml`）/ 外观；
//!   本文件承载「运行 + 模型 + 外观 + 记忆 + 树清单指针」，空间与树包各有独立文件；
//! - 界面是主入口，`config.toml` 是高级双通道（UI 保存前跑同一套 [`AppConfig::validate`]）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

fn default_llm_mode() -> String {
    "agent".into()
}
fn default_max_tool_rounds() -> usize {
    8
}
fn default_theme() -> String {
    "light".into()
}
fn default_font_size() -> u32 {
    13
}
fn default_window_w() -> f32 {
    1180.0
}
fn default_window_h() -> f32 {
    780.0
}
fn default_curated_chars() -> usize {
    800
}

/// 顶层配置。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AppConfig {
    pub llm: LlmSection,
    pub scope: ScopeSection,
    pub run: RunSection,
    pub sandbox: SandboxSection,
    /// 树清单指针（`trees/registry.toml` 的入口；也支持单文件 `tree.json`）。
    /// 外观与窗口（UI 主入口，无需手写配置）。
    pub app: AppSection,
    /// 记忆注入策略（L2 空间记忆 / L3 经验库）。
    pub memory: MemorySection,
    /// 上下文五段预算 + 压缩策略（M3）。
    pub context: ContextSection,
    /// 智能体循环（轮数 / 计划 / 反思 / 子代理 / 并发池）。
    pub agent: AgentSection,
    /// 领域（默认领域目录与激活项）。
    pub domain: DomainSection,
    /// 网络与请求（并发 / 超时 / 重试 / UA / 代理）。
    pub net: NetSection,
    /// 日志与数据落点。
    pub logging: LoggingSection,
    /// 原子插件（`plugins/` 目录）。
    pub plugins: PluginsSection,
}

/// 智能体循环参数（原 `[llm].max_tool_rounds` 升级为独立分区，旧字段继续生效）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentSection {
    /// tool-calling 最大轮数（1..=64）。0 表示"沿用 `[llm].max_tool_rounds`"。
    pub max_tool_rounds: usize,
    /// 计划模式：`auto`（模型自己决定何时建计划）/ `always`（先出计划并过评审门）/ `off`（不装计划层）。
    pub plan_mode: String,
    /// 反思层开关：一轮结束后对失败/偏差做复盘并沉淀经验。
    pub reflect: bool,
    /// 触发反思的最小连续失败数。
    pub reflect_min_failures: usize,
    /// 子代理开关（`subagent.spawn`）。
    pub subagent_enabled: bool,
    /// 子代理工具轮数上限（2..=8）。
    pub subagent_max_rounds: usize,
    /// 并行工具池大小（同一轮多个并发安全工具时的并发度）。
    pub parallel_pool: usize,
    /// 重复调用守卫：同一工具+参数在短窗口内重复时升级为需审批。
    pub dup_guard: bool,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            max_tool_rounds: 0,
            plan_mode: "auto".into(),
            reflect: true,
            reflect_min_failures: 2,
            subagent_enabled: true,
            subagent_max_rounds: 6,
            parallel_pool: 8,
            dup_guard: true,
        }
    }
}

impl AgentSection {
    /// 实际生效的轮数上限（`[agent]` 优先，0 回落到 `[llm]`）。
    pub fn effective_rounds(&self, llm_rounds: usize) -> usize {
        if self.max_tool_rounds == 0 {
            llm_rounds.clamp(1, 64)
        } else {
            self.max_tool_rounds.clamp(1, 64)
        }
    }

    /// 计划层行为（唯一的消费入口）。
    ///
    /// 为什么用枚举而不是在装配处 `if plan_enabled`：字符串配置项一旦被多处各自
    /// `==` 比较，就会漂移成"某处认 `always`、某处只认布尔"。这里一处解析，
    /// 装配与会话都拿同一份判定。
    pub fn plan_behavior(&self) -> PlanMode {
        match self.plan_mode.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => PlanMode::Off,
            "always" | "force" | "on" => PlanMode::Always,
            // auto 是缺省：写错的值不静默变成"关"，而是回落为最常用的 auto
            _ => PlanMode::Auto,
        }
    }
}

/// 计划层行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    /// 不装计划层：不注册 `plan.*`，不注入计划视图。
    Off,
    /// 模型自主：装 `plan.*`，由模型判断要不要建计划。
    Auto,
    /// 强制：装 `plan.*` + 评审门 `plan.exit`，并在提示里要求"先出计划再动手"。
    Always,
}

impl PlanMode {
    pub fn label(self) -> &'static str {
        match self {
            PlanMode::Off => "关闭",
            PlanMode::Auto => "自动",
            PlanMode::Always => "强制",
        }
    }
}

/// 领域（描述包）配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DomainSection {
    /// 领域根目录；空 = exe 同级 `domains/`（开发态向上找项目根）。
    pub dir: Option<String>,
    /// 激活的领域 id（缺省 `default`）。
    pub active: String,
    /// 切换领域时是否保留当前会话（false = 切换即开新会话）。
    pub keep_session: bool,
}

impl Default for DomainSection {
    fn default() -> Self {
        Self { dir: None, active: "default".into(), keep_session: false }
    }
}

/// 网络与请求。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NetSection {
    /// 全局并发上限（跨工具）。
    pub concurrency: usize,
    /// 单请求超时（秒）。
    pub timeout_secs: u64,
    /// 连接超时（秒）。
    pub connect_timeout_secs: u64,
    /// 失败重试次数（幂等请求）。
    pub retries: u32,
    /// 全局 User-Agent；空 = 内置默认（便于识别自身流量）。
    pub user_agent: String,
    /// 代理：`http://host:port` / `socks5://host:port`；空 = 直连。
    pub proxy: Option<String>,
    /// 单域名每秒请求上限（0 = 不限）。
    pub per_host_qps: u32,
}

impl Default for NetSection {
    fn default() -> Self {
        Self {
            concurrency: 4,
            timeout_secs: 30,
            connect_timeout_secs: 10,
            retries: 1,
            user_agent: String::new(),
            proxy: None,
            per_host_qps: 0,
        }
    }
}

impl NetSection {
    pub fn effective_user_agent(&self) -> String {
        if self.user_agent.trim().is_empty() {
            format!("windpencilagent/{}", env!("CARGO_PKG_VERSION"))
        } else {
            self.user_agent.trim().to_string()
        }
    }
}

/// 日志与数据落点。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LoggingSection {
    /// `error` / `warn` / `info` / `debug` / `trace`。
    pub level: String,
    /// 日志目录；空 = `<data>/logs`。
    pub dir: Option<String>,
    /// 保留天数（按天滚动）。
    pub keep_days: u32,
    /// 单文件上限（MB），超过即滚动。
    pub max_mb: u32,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self { level: "info".into(), dir: None, keep_days: 7, max_mb: 20 }
    }
}

/// 插件体系配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PluginsSection {
    /// 插件根目录；空 = 自动探测（exe 同级 `plugins/`，开发态向上找项目根）。
    pub dir: Option<String>,
    /// 是否在装配时加载插件（关掉可快速判断"问题是不是插件引起的"）。
    pub enabled: bool,
    /// 单个插件调用的兜底超时（毫秒）；插件内部应有更短的超时。
    pub timeout_ms: u64,
    /// 一次装载最多加载多少个实例（LRU 上限）。
    pub max_cached: usize,
}

impl Default for PluginsSection {
    fn default() -> Self {
        Self {
            dir: None,
            enabled: true,
            timeout_ms: 120_000,
            max_cached: 32,
        }
    }
}

/// LLM 网关配置。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmSection {
    pub driver: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// `agent`（携带工具，默认）/ `chat`（纯对话，不携带工具）。
    #[serde(default = "default_llm_mode")]
    pub mode: String,
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: usize,
    /// 多档案：`[[llm.profiles]]`。
    #[serde(default)]
    pub profiles: Vec<base_llm::LlmProfile>,
    /// 当前激活档案名。
    #[serde(default)]
    pub active: Option<String>,
    /// fallback 链：主档不可用（未配置/初始化失败/调用失败）时按序遍历的档案名。
    #[serde(default)]
    pub fallback: Vec<String>,
}

impl Default for LlmSection {
    fn default() -> Self {
        Self {
            driver: String::new(),
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            temperature: None,
            max_tokens: None,
            mode: default_llm_mode(),
            max_tool_rounds: default_max_tool_rounds(),
            profiles: vec![],
            active: None,
            fallback: vec![],
        }
    }
}

impl LlmSection {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// 选中生效配置：激活档案 > 顶层字段。
    pub fn effective(&self) -> base_llm::ProviderConfig {
        if let Some(active) = &self.active {
            if let Some(p) = self.profiles.iter().find(|p| &p.name == active && p.enabled) {
                return p.to_provider_config();
            }
        }
        base_llm::ProviderConfig {
            driver: if self.driver.is_empty() { "openai-compatible".into() } else { self.driver.clone() },
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            mode: self.mode.clone(),
            max_retries: 3,
        }
    }

    /// 当前生效档案（无档案名时 None）。
    pub fn active_profile(&self) -> Option<&base_llm::LlmProfile> {
        self.active
            .as_ref()
            .and_then(|a| self.profiles.iter().find(|p| &p.name == a && p.enabled))
    }

    /// 解析可用模型链：`preferred`（空间绑定）→ 激活档案 → fallback 链 → 其余已启用档案。
    ///
    /// 只返回「填了 base_url + model」的档案；顶层字段视为一条匿名档案排在最后兜底。
    /// 首个即主档，其余在调用失败时按序降级（见 `engine::llm::FallbackProvider`）。
    pub fn resolve_chain(&self, preferred: Option<&str>) -> Vec<base_llm::LlmProfile> {
        let mut out: Vec<base_llm::LlmProfile> = Vec::new();
        let push = |p: &base_llm::LlmProfile, out: &mut Vec<base_llm::LlmProfile>| {
            if p.base_url.trim().is_empty() || p.model.trim().is_empty() {
                return;
            }
            if out.iter().any(|x| x.name == p.name) {
                return;
            }
            out.push(p.clone());
        };

        if let Some(name) = preferred {
            if let Some(p) = self.profiles.iter().find(|p| p.name == name && p.enabled) {
                push(p, &mut out);
            }
        }
        if let Some(p) = self.active_profile() {
            push(p, &mut out);
        }
        for name in &self.fallback {
            if let Some(p) = self.profiles.iter().find(|p| &p.name == name && p.enabled) {
                push(p, &mut out);
            }
        }
        for p in &self.profiles {
            if p.enabled {
                push(p, &mut out);
            }
        }
        // 顶层字段兜底（没有匹配到任何档案时）
        if out.is_empty() && self.is_configured() {
            let cfg = self.effective();
            out.push(base_llm::LlmProfile {
                name: "default".into(),
                driver: cfg.driver,
                base_url: cfg.base_url,
                model: cfg.model,
                api_key: cfg.api_key,
                temperature: cfg.temperature,
                max_tokens: cfg.max_tokens,
                enabled: true,
                mode: cfg.mode,
                context_window: None,
                supports_tools: None,
                supports_vision: None,
                cost_per_1k_in: None,
                cost_per_1k_out: None,
            });
        }
        out
    }
}

/// 授权范围（红线）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ScopeSection {
    /// 允许操作的目标；空表示未声明（fail-closed：工具调用会被拒）。
    pub authorized: Vec<String>,
    /// 拒绝清单（「除这些之外」），优先级高于 authorized。
    pub denylist: Vec<String>,
}

/// 运行参数。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RunSection {
    /// 非交互自动批准（自动化场景；默认 false = 逐次询问）。
    pub auto_approve: bool,
    /// 数据目录（会话库/结果落盘）；缺省 = exe 同级 data/。
    pub data_dir: Option<String>,
    /// 审批超时秒数（超时视为拒绝，fail-closed）。
    pub approval_timeout_secs: Option<u64>,
}

/// 沙箱配置。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SandboxSection {
    /// true = os.exec / sandbox.python 经受限令牌执行（fail-closed）。
    pub restricted_token: bool,
    /// 是否尝试 AppContainer 隔离（Win8+；失败回落受限令牌）。
    pub app_container: bool,
    /// Python 解释器路径；留空/None = 自动探测（程序子目录 python/python.exe）。
    /// 装配时写入环境变量 WPA_PYTHON_PATH 供 sandbox.python / sandbox.pip 读取。
    #[serde(default)]
    pub python_path: Option<String>,
    /// pip 可执行文件路径；留空/None = 用 python 同目录的 pip（`python -m pip`）。
    /// 装配时写入环境变量 WPA_PIP_PATH 供 sandbox.pip 读取。
    #[serde(default)]
    pub pip_path: Option<String>,
}

/// 上下文五段预算（比例；和为 1，写歪了会自动归一化）+ 压缩策略。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ContextSection {
    /// system prompt（角色 + 注入段的合计上限）
    pub system: f32,
    /// 知识注入（L4 领域知识摘要）
    pub knowledge: f32,
    /// 经验注入（L3）
    pub experience: f32,
    /// 工具 schema
    pub tools: f32,
    /// 对话 + 观察（必须是大头）
    pub dialog: f32,
    /// 自动压缩触发占比（0.5..=0.95，默认 0.8）。
    pub compact_threshold: f64,
    /// 压缩时保留的最近原始轮数（1..=12）。
    pub keep_recent_turns: usize,
    /// 历史窗口条数（不含 system）。
    pub history_cap: usize,
    /// 历史超窗时是否滚动总结（需 LLM 在线）。
    pub summarize_history: bool,
    /// 对话摘要间隔（每 N 轮沉淀一次经验；0 = 关闭）。
    pub extract_every: usize,
}

impl Default for ContextSection {
    fn default() -> Self {
        Self {
            system: 0.15,
            knowledge: 0.10,
            experience: 0.10,
            tools: 0.10,
            dialog: 0.55,
            compact_threshold: 0.8,
            keep_recent_turns: 4,
            history_cap: 24,
            summarize_history: true,
            extract_every: 5,
        }
    }
}

impl ContextSection {
    pub fn to_budget(&self) -> crate::context::Budget {
        crate::context::Budget {
            system: self.system,
            knowledge: self.knowledge,
            experience: self.experience,
            tools: self.tools,
            dialog: self.dialog,
        }
        .normalized()
    }
}

/// 外观与窗口（UI 设置页写入，无需手编）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AppSection {
    /// `light` / `dark`
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    #[serde(default = "default_window_w")]
    pub window_w: f32,
    #[serde(default = "default_window_h")]
    pub window_h: f32,
    /// 关闭窗口时是否记住窗口尺寸（M2 恒为 true，字段留给后续）
    pub remember_window: bool,
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            font_size: default_font_size(),
            window_w: default_window_w(),
            window_h: default_window_h(),
            remember_window: true,
        }
    }
}

/// 记忆注入策略。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MemorySection {
    /// 是否把空间精炼笔记（MEMORY.md）注入 system prompt
    pub inject_curated: bool,
    /// 注入前多少字符（默认 800）
    #[serde(default = "default_curated_chars")]
    pub curated_chars: usize,
    /// 经验召回 token 预算（L3，默认 1500）
    pub experience_token_budget: usize,
    /// system prompt 注入的经验条数上限（默认 6）
    pub experience_inject: usize,
}

impl Default for MemorySection {
    fn default() -> Self {
        Self {
            inject_curated: true,
            curated_chars: default_curated_chars(),
            experience_token_budget: 1500,
            experience_inject: 6,
        }
    }
}

/// 配置域（G-3）：一次写盘归属于哪个域，决定谁来热更新、是否需要重启。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSection {
    /// 外观（字号、窗口尺寸、记忆注入开关等需要重建界面的项）。
    App,
    /// 主题（`Theme.is_dark` 是运行期属性，改完立刻重算调色板 ⇒ 热更新）。
    Theme,
    /// 模型档案与降级链。
    Llm,
    /// 记忆注入预算。
    Memory,
    /// 上下文预算与压缩策略。
    Context,
    /// 智能体循环（轮数 / 计划 / 反思 / 子代理 / 并发池）。
    Agent,
    /// 运行期（数据目录、审批策略）。
    Run,
    /// 授权范围与拒绝清单。
    Scope,
    /// 沙箱与隔离。
    Sandbox,
    /// 原子插件运行时（加载/缓存/超时）。
    Plugin,
    /// 网络与请求。
    Net,
    /// 日志与数据落点。
    Logging,
    /// 领域（描述包）选择。
    Domain,
    /// 树包选择与版本要求（兼容保留）。
    Tree,
}

impl ConfigSection {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigSection::App => "app",
            ConfigSection::Theme => "theme",
            ConfigSection::Llm => "llm",
            ConfigSection::Memory => "memory",
            ConfigSection::Context => "context",
            ConfigSection::Agent => "agent",
            ConfigSection::Run => "run",
            ConfigSection::Scope => "scope",
            ConfigSection::Sandbox => "sandbox",
            ConfigSection::Plugin => "plugins",
            ConfigSection::Net => "net",
            ConfigSection::Logging => "logging",
            ConfigSection::Domain => "domain",
            ConfigSection::Tree => "tree",
        }
    }

    /// 能否在不重启进程的前提下生效。
    ///
    /// - 主题：`ui/kit/tokens.slint` 的 `is_dark` 是 in-out 属性，改完 palette 立即重算；
    /// - 模型/记忆/上下文/循环/授权/网络：下一次构建会话就能读到（工具与 HTTP 客户端按新值重建）；
    /// - 领域：切换领域要换工具面 + 换 prompt，属于装配层的事，只能重新装配（桌面端点「重新装配」）；
    /// - 字号落在各处硬编码的 px 上，数据目录/沙箱/插件/日志在装配阶段固化，都要重启。
    pub fn is_hot(self) -> bool {
        matches!(
            self,
            ConfigSection::Theme
                | ConfigSection::Llm
                | ConfigSection::Memory
                | ConfigSection::Context
                | ConfigSection::Agent
                | ConfigSection::Scope
                | ConfigSection::Net
        )
    }
}

/// 一次配置写盘的结果 —— 订阅方据此决定热更新还是提示重启。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub sections: Vec<ConfigSection>,
}

impl ChangeSet {
    pub fn new(sections: &[ConfigSection]) -> Self {
        let mut d: Vec<ConfigSection> = Vec::new();
        for x in sections {
            if !d.contains(x) {
                d.push(*x);
            }
        }
        Self { sections: d }
    }

    /// 全部域都能热更新。
    pub fn hot(&self) -> bool {
        self.sections.iter().all(|d| d.is_hot())
    }

    /// 需要重启才能生效的域。
    pub fn restart_sections(&self) -> Vec<ConfigSection> {
        self.sections.iter().copied().filter(|d| !d.is_hot()).collect()
    }

    /// 给用户看的一句话（写清楚"生效时机"，避免"保存了但没反应"的困惑）。
    pub fn describe(&self) -> String {
        if self.sections.is_empty() {
            return "配置未变化".into();
        }
        let names: Vec<&str> = self.sections.iter().map(|d| d.as_str()).collect();
        let list = names.join(" / ");
        let restart: Vec<&str> = self
            .restart_sections()
            .iter()
            .map(|d| d.as_str())
            .collect();
        if restart.is_empty() {
            format!("已保存 [{list}]，下一轮会话生效")
        } else {
            format!("已保存 [{list}]；其中 [{}] 需重启生效", restart.join(" / "))
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配置失败 {}: {e}", path.display()))?;
        toml::from_str(&raw).map_err(|e| format!("解析 config.toml 失败：{e}"))
    }

    /// 保存（原子写：先写 .tmp 再 rename），保存前跑 [`Self::validate`]。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let errs = self.validate();
        if !errs.is_empty() {
            return Err(format!("配置校验失败：\n  {}", errs.join("\n  ")));
        }
        let text = toml::to_string_pretty(self).map_err(|e| format!("序列化配置失败：{e}"))?;
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
            }
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("写入临时配置失败：{e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换配置文件失败：{e}"))?;
        Ok(())
    }

    /// 带回执的保存：校验 → 原子写 → 返回 [`ChangeSet`]（G-3 配置变更事件）。
    ///
    /// 调用方把 `domains` 声明清楚，订阅方（UI/引擎）据此各自动自己的热更新。
    pub fn commit(&self, path: &Path, domains: &[ConfigSection]) -> Result<ChangeSet, String> {
        self.save(path)?;
        Ok(ChangeSet::new(domains))
    }

    /// 配置校验（字段级错误；UI 保存前与手工编辑后共用）。
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !matches!(self.llm.mode.as_str(), "agent" | "chat") {
            errs.push(format!("[llm].mode 只能是 agent 或 chat（当前 {:?}）", self.llm.mode));
        }
        if self.llm.max_tool_rounds == 0 || self.llm.max_tool_rounds > 64 {
            errs.push(format!("[llm].max_tool_rounds 需在 1..=64（当前 {}）", self.llm.max_tool_rounds));
        }
        if !matches!(self.app.theme.as_str(), "light" | "dark") {
            errs.push(format!("[app].theme 只能是 light 或 dark（当前 {:?}）", self.app.theme));
        }
        if !(10..=24).contains(&self.app.font_size) {
            errs.push(format!("[app].font_size 需在 10..=24（当前 {}）", self.app.font_size));
        }
        if self.app.window_w < 900.0 || self.app.window_h < 600.0 {
            errs.push(format!(
                "[app] 窗口尺寸不得小于 900x600（当前 {}x{}）",
                self.app.window_w, self.app.window_h
            ));
        }
        // 重名档案
        let mut seen: Vec<&str> = Vec::new();
        for p in &self.llm.profiles {
            if p.name.trim().is_empty() {
                errs.push("[llm.profiles] 存在空 name".into());
            } else if seen.contains(&p.name.as_str()) {
                errs.push(format!("[llm.profiles] 档案重名：{}", p.name));
            } else {
                seen.push(&p.name);
            }
            if p.base_url.trim().is_empty() || p.model.trim().is_empty() {
                errs.push(format!("[llm.profiles] 档案 {} 缺少 base_url 或 model", p.name));
            }
            if let Some(cw) = p.context_window {
                if cw < 1024 {
                    errs.push(format!("[llm.profiles] 档案 {} 的 context_window 过小（{cw}）", p.name));
                }
            }
        }
        // fallback 引用必须存在
        for name in &self.llm.fallback {
            if !self.llm.profiles.iter().any(|p| &p.name == name) {
                errs.push(format!("[llm].fallback 引用了不存在的档案：{name}"));
            }
        }
        if let Some(active) = &self.llm.active {
            if !self.llm.profiles.iter().any(|p| &p.name == active) {
                errs.push(format!("[llm].active 引用了不存在的档案：{active}"));
            }
        }
        // 上下文预算：五项必须为正且和接近 1（归一化只是兜底，不该默默吃掉手误）
        let ctx = [self.context.system, self.context.knowledge, self.context.experience, self.context.tools, self.context.dialog];
        if ctx.iter().any(|v| !v.is_finite() || *v <= 0.0) {
            errs.push("[context] 五项比例必须为正数".into());
        } else {
            let sum: f32 = ctx.iter().sum();
            if (sum - 1.0).abs() > 0.05 {
                errs.push(format!("[context] 五项比例之和应约为 1（当前 {sum:.2}）"));
            }
            if self.context.dialog < 0.4 {
                errs.push(format!(
                    "[context].dialog 不应低于 0.4（当前 {}）：对话与观察被压得太狠会频繁压历史",
                    self.context.dialog
                ));
            }
        }
        if self.memory.curated_chars == 0 {
            errs.push("[memory].curated_chars 不能为 0".into());
        }
        // 智能体循环
        if self.agent.max_tool_rounds > 64 {
            errs.push(format!(
                "[agent].max_tool_rounds 不得超过 64（当前 {}）",
                self.agent.max_tool_rounds
            ));
        }
        if !matches!(self.agent.plan_mode.as_str(), "auto" | "always" | "off") {
            errs.push(format!(
                "[agent].plan_mode 只能是 auto / always / off（当前 {:?}）",
                self.agent.plan_mode
            ));
        }
        if !(2..=8).contains(&self.agent.subagent_max_rounds) {
            errs.push(format!(
                "[agent].subagent_max_rounds 需在 2..=8（当前 {}）",
                self.agent.subagent_max_rounds
            ));
        }
        if !(1..=32).contains(&self.agent.parallel_pool) {
            errs.push(format!(
                "[agent].parallel_pool 需在 1..=32（当前 {}）",
                self.agent.parallel_pool
            ));
        }
        if !(1..=10).contains(&self.agent.reflect_min_failures) {
            errs.push(format!(
                "[agent].reflect_min_failures 需在 1..=10（当前 {}）",
                self.agent.reflect_min_failures
            ));
        }
        // 压缩策略
        if !(0.5..=0.95).contains(&self.context.compact_threshold) {
            errs.push(format!(
                "[context].compact_threshold 需在 0.5..=0.95（当前 {}）",
                self.context.compact_threshold
            ));
        }
        if !(1..=12).contains(&self.context.keep_recent_turns) {
            errs.push(format!(
                "[context].keep_recent_turns 需在 1..=12（当前 {}）",
                self.context.keep_recent_turns
            ));
        }
        if !(8..=200).contains(&self.context.history_cap) {
            errs.push(format!(
                "[context].history_cap 需在 8..=200（当前 {}）",
                self.context.history_cap
            ));
        }
        if self.context.extract_every > 100 {
            errs.push(format!(
                "[context].extract_every 不得超过 100（当前 {}）",
                self.context.extract_every
            ));
        }
        // 网络
        if !(1..=64).contains(&self.net.concurrency) {
            errs.push(format!("[net].concurrency 需在 1..=64（当前 {}）", self.net.concurrency));
        }
        if !(1..=600).contains(&self.net.timeout_secs) {
            errs.push(format!("[net].timeout_secs 需在 1..=600（当前 {}）", self.net.timeout_secs));
        }
        if !(1..=120).contains(&self.net.connect_timeout_secs) {
            errs.push(format!(
                "[net].connect_timeout_secs 需在 1..=120（当前 {}）",
                self.net.connect_timeout_secs
            ));
        }
        if self.net.retries > 10 {
            errs.push(format!("[net].retries 不得超过 10（当前 {}）", self.net.retries));
        }
        if let Some(p) = &self.net.proxy {
            let t = p.trim();
            if !t.is_empty()
                && !(t.starts_with("http://")
                    || t.starts_with("https://")
                    || t.starts_with("socks5://")
                    || t.starts_with("socks5h://"))
            {
                errs.push("[net].proxy 需以 http:// / https:// / socks5:// 开头".into());
            }
        }
        // 日志
        if !matches!(
            self.logging.level.as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            errs.push(format!(
                "[logging].level 只能是 error/warn/info/debug/trace（当前 {:?}）",
                self.logging.level
            ));
        }
        if !(1..=365).contains(&self.logging.keep_days) {
            errs.push(format!(
                "[logging].keep_days 需在 1..=365（当前 {}）",
                self.logging.keep_days
            ));
        }
        // 领域
        if self.domain.active.trim().is_empty() {
            errs.push("[domain].active 不能为空（缺省 default）".into());
        }
        errs
    }

    /// 只校验单个字段：复用 [`Self::validate`] 的逐字段规则，再按 key 过滤无关错误。
    ///
    /// SetField 场景里用户一次只改一项；若整表还有别的存量错误（例如手编配置写歪了
    /// context 比例），不该因此卡住这一次修改。返回空 Vec = 该字段通过。
    ///
    /// 匹配口径：校验错误统一以 `[section]` 或 `[section.field]` 开头，取 key 的前两段
    /// 比对；段级聚合错误（`[app] 窗口尺寸`、`[context] 五项比例`）单独映射到相关字段。
    pub fn validate_field(&self, key: &str) -> Vec<String> {
        let key = key.trim();
        let mut parts = key.split('.');
        let section = parts.next().unwrap_or("");
        let field = parts.next().unwrap_or("");
        if section.is_empty() {
            return Vec::new();
        }
        let token = if field.is_empty() {
            String::new()
        } else {
            format!(".{field}")
        };
        self.validate()
            .into_iter()
            .filter(|e| {
                let err_section = e
                    .strip_prefix('[')
                    .and_then(|s| s.split_once(']'))
                    .map(|(s, _)| s)
                    .unwrap_or("");
                let section_ok = err_section == section
                    || (!field.is_empty() && err_section == format!("{section}.{field}"));
                if !section_ok {
                    return false;
                }
                if token.is_empty() || e.contains(&token) {
                    return true;
                }
                match (section, field) {
                    ("app", "window_w" | "window_h") => e.contains("窗口尺寸"),
                    (
                        "context",
                        "system" | "knowledge" | "experience" | "tools" | "dialog",
                    ) => e.contains("五项比例"),
                    _ => false,
                }
            })
            .collect()
    }

    /// 授权范围优先级：CLI `--target` > 空间授权范围 > 配置文件。
    pub fn merged_scope(&self, cli_targets: &[String]) -> Vec<String> {
        if cli_targets.is_empty() {
            self.scope.authorized.clone()
        } else {
            cli_targets.to_vec()
        }
    }

    pub fn data_root(&self) -> PathBuf {
        match &self.run.data_dir {
            Some(d) if !d.trim().is_empty() => PathBuf::from(d),
            _ => app_root().join("data"),
        }
    }

    /// 空间注册表根目录（exe 同级 `workspaces/`）。
    pub fn workspaces_root(&self) -> PathBuf {
        app_root().join("workspaces")
    }

    /// 树包根目录（exe 同级 `trees/`）。
    pub fn trees_root(&self) -> PathBuf {
        app_root().join("trees")
    }

    /// 按内置预设新增并激活一个模型档案。
    ///
    /// 语义刻意保守：同名档案已存在时**只切换，不覆盖**——用户填过的密钥和改过的
    /// 模型名比"重置成预设"重要得多。要重置请先在「模型」页删掉该档案。
    pub fn add_llm_preset(&mut self, preset: &str) -> Result<(), String> {
        let p = base_llm::builtin_profiles()
            .into_iter()
            .find(|x| x.name == preset)
            .ok_or_else(|| format!("未知模型预设：{preset}"))?;
        let name = p.name.clone();
        if !self.llm.profiles.iter().any(|x| x.name == name) {
            self.llm.profiles.push(p);
        }
        self.llm.active = Some(name);
        Ok(())
    }

    /// 领域根目录。
    ///
    /// 顺序：配置项 → exe 同级 `domains/` → 向上 1~3 级找 `domains/`（开发态
    /// exe 在 `target/<profile>/`，项目根才是真位置）。
    pub fn domains_root(&self) -> PathBuf {
        if let Some(d) = &self.domain.dir {
            if !d.trim().is_empty() {
                return PathBuf::from(d);
            }
        }
        let root = app_root();
        let direct = root.join("domains");
        if direct.is_dir() {
            return direct;
        }
        for up in [2usize, 1, 3] {
            let mut p = root.clone();
            for _ in 0..up {
                match p.parent() {
                    Some(parent) => p = parent.to_path_buf(),
                    None => break,
                }
            }
            let cand = p.join("domains");
            if cand.is_dir() {
                return cand;
            }
        }
        direct
    }

    /// 当前激活领域的目录。
    pub fn active_domain_dir(&self) -> PathBuf {
        self.domains_root().join(&self.domain.active)
    }

    /// 日志目录（缺省 `<data>/logs`）。
    pub fn logs_dir(&self) -> PathBuf {
        match &self.logging.dir {
            Some(d) if !d.trim().is_empty() => PathBuf::from(d),
            _ => self.data_root().join("logs"),
        }
    }

    /// 插件根目录。
    ///
    /// 顺序：配置项 → exe 同级 `plugins/` → 向上 1~3 级找 `plugins/`（开发态
    /// exe 在 `target/<profile>/`，项目根才是真位置）。
    pub fn plugins_root(&self) -> PathBuf {
        if let Some(d) = &self.plugins.dir {
            if !d.trim().is_empty() {
                return PathBuf::from(d);
            }
        }
        let root = app_root();
        let direct = root.join("plugins");
        if direct.is_dir() {
            return direct;
        }
        for up in [2usize, 1, 3] {
            let mut p = root.clone();
            for _ in 0..up {
                match p.parent() {
                    Some(parent) => p = parent.to_path_buf(),
                    None => break,
                }
            }
            let cand = p.join("plugins");
            if cand.is_dir() {
                return cand;
            }
        }
        direct
    }

    /// 插件状态文件（`<data>/plugins.toml`）。
    pub fn plugins_state_path(&self) -> PathBuf {
        self.data_root().join("plugins.toml")
    }

}

/// exe 同级目录（数据/配置默认落点，便于整体搬移）。
pub fn app_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `windpencilagent-cli init` 输出的示例配置。
///
/// 每一项都在「配置 → 内置能力」的卡片里有对应控件；本文件是高级双通道入口，
/// 保存时 UI 会跑同一套 [`AppConfig::validate`]。
pub const SAMPLE_CONFIG: &str = r#"# windpencilagent 基座配置（TOML）
# 界面是主入口；本文件是高级双通道入口，保存时 UI 会跑同一套校验。
# 基座不含任何业务配置：领域策略/规则/知识都在 domains/ 里。

[app]
theme = "light"          # light / dark
font_size = 13           # 10-24
window_w = 1180
window_h = 780
remember_window = true

[llm]
driver = "openai"        # openai / anthropic / gemini
base_url = "http://127.0.0.1:11434/v1"
model = "qwen2.5:7b-instruct"
# api_key = "sk-..."
mode = "agent"           # agent（携带工具）/ chat（纯对话）
# max_tool_rounds = 8    # 已迁移到 [agent]，此字段保留兼容
# fallback = ["ollama"]  # 主档不可用时按序降级（引用下面的档案名）

# 模型档案（界面「配置 → 模型」可增删改；此处为示例）
# [[llm.profiles]]
# name = "deepseek"
# driver = "openai"
# base_url = "https://api.deepseek.com/v1"
# model = "deepseek-chat"
# api_key = "sk-..."
# context_window = 65536
# supports_tools = true
# supports_vision = false

[agent]
max_tool_rounds = 0      # 0 = 沿用 [llm].max_tool_rounds
plan_mode = "auto"       # auto / always / off
reflect = true
reflect_min_failures = 2
subagent_enabled = true
subagent_max_rounds = 6
parallel_pool = 8
dup_guard = true

[domain]
active = "default"
keep_session = false

[scope]
# 红线：只允许操作列出的目标（空 = 未声明，工具调用会被拒）
authorized = []
denylist = []

[run]
auto_approve = false
# data_dir = "data"
# approval_timeout_secs = 300

[sandbox]
# true = os.exec / sandbox.python 经 Windows 受限令牌执行
restricted_token = false
app_container = false
# python_path = "C:/path/to/python.exe"  # 留空 = 自动探测（程序子目录 python/python.exe）
# pip_path = "C:/path/to/pip.exe"        # 留空 = 用 python 同目录的 pip

[net]
concurrency = 4
timeout_secs = 30
connect_timeout_secs = 10
retries = 1
user_agent = ""          # 空 = 内置默认
# proxy = "http://127.0.0.1:7890"
per_host_qps = 0         # 0 = 不限

[logging]
level = "info"
# dir = "data/logs"
keep_days = 7
max_mb = 20

[memory]
inject_curated = true
curated_chars = 800
experience_inject = 6
experience_token_budget = 1500

[context]
# 五段预算（和为 1）。裁剪顺序：先裁可重建观察 → 再裁早期对话 → 最后才动 system；
# Must 约束与基座骨架永不裁。
system = 0.15
knowledge = 0.10
experience = 0.10
tools = 0.10
dialog = 0.55
compact_threshold = 0.8   # 窗口占用超过该比例即压缩
keep_recent_turns = 4
history_cap = 24
summarize_history = true
extract_every = 5

[plugins]
enabled = true
timeout_ms = 120000
max_cached = 32

[tree]
# 单文件树清单（兼容保留）；新项目用 [domain] 表达领域
path = "tree.json"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample() {
        let cfg: AppConfig = toml::from_str(SAMPLE_CONFIG).unwrap();
        assert!(cfg.scope.authorized.is_empty());
        assert_eq!(cfg.llm.mode, "agent");
        assert!(!cfg.run.auto_approve);
        assert_eq!(cfg.app.theme, "light");
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn empty_defaults_are_valid() {
        let cfg = AppConfig::default();
        assert!(!cfg.llm.is_configured());
        assert_eq!(cfg.llm.max_tool_rounds, 8);
        assert_eq!(cfg.app.font_size, 13);
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn cli_target_overrides_config() {
        let cfg: AppConfig = toml::from_str("[scope]\nauthorized=[\"127.0.0.1\"]\n").unwrap();
        assert_eq!(cfg.merged_scope(&[]), vec!["127.0.0.1".to_string()]);
        assert_eq!(cfg.merged_scope(&["10.0.0.1".into()]), vec!["10.0.0.1".to_string()]);
    }

    #[test]
    fn active_profile_wins() {
        let raw = r#"
[llm]
base_url = "http://a/v1"
model = "m1"
active = "b"

[[llm.profiles]]
name = "a"
driver = "openai-compatible"
base_url = "http://a/v1"
model = "m1"

[[llm.profiles]]
name = "b"
driver = "openai-compatible"
base_url = "http://b/v1"
model = "m2"
"#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        let eff = cfg.llm.effective();
        assert_eq!(eff.base_url, "http://b/v1");
        assert_eq!(eff.model, "m2");
    }

    #[test]
    fn resolve_chain_prefers_space_then_active_then_fallback() {
        let raw = r#"
[llm]
active = "a"
fallback = ["c"]

[[llm.profiles]]
name = "a"
base_url = "http://a/v1"
model = "ma"

[[llm.profiles]]
name = "b"
base_url = "http://b/v1"
model = "mb"

[[llm.profiles]]
name = "c"
base_url = "http://c/v1"
model = "mc"
"#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        let chain = cfg.llm.resolve_chain(Some("b"));
        let names: Vec<&str> = chain.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a", "c"]);
        // 未激活、无 fallback 时取已启用档案顺序
        let chain = cfg.llm.resolve_chain(None);
        assert_eq!(chain[0].name, "a");
    }

    #[test]
    fn capability_fields_are_backward_compatible() {
        // 旧配置（无能力字段）仍可解析，缺省 abilities 走保守默认
        let p: base_llm::LlmProfile = toml::from_str(
            r#"name="old"
base_url="http://x/v1"
model="m""#,
        )
        .unwrap();
        assert!(p.context_window.is_none());
        assert!(p.supports_tools());
        assert_eq!(p.context_window_or_default(), 32_000);
    }

    #[test]
    fn validate_rejects_bad_values() {
        let mut cfg = AppConfig::default();
        cfg.llm.mode = "weird".into();
        cfg.app.font_size = 40;
        cfg.app.window_w = 100.0;
        let errs = cfg.validate();
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("mode")));
        assert!(errs.iter().any(|e| e.contains("font_size")));
        assert!(errs.iter().any(|e| e.contains("窗口尺寸")));
    }

    #[test]
    fn validate_checks_profile_refs() {
        let raw = r#"
[llm]
active = "nope"
fallback = ["ghost"]

[[llm.profiles]]
name = "a"
base_url = "http://a/v1"
model = "ma"
"#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("active")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("fallback")), "{errs:?}");
    }

    #[test]
    fn validate_field_only_reports_the_edited_field() {
        let mut cfg = AppConfig::default();
        assert!(cfg.validate_field("llm.mode").is_empty());
        assert!(cfg.validate_field("sandbox.python_path").is_empty());
        assert!(cfg.validate_field("sandbox.pip_path").is_empty());

        // 两个字段同时非法：整表报两条，单字段各只报自己的一条
        cfg.llm.mode = "weird".into();
        cfg.app.font_size = 40;
        assert_eq!(cfg.validate().len(), 2, "{:?}", cfg.validate());
        let mode_errs = cfg.validate_field("llm.mode");
        assert_eq!(mode_errs.len(), 1, "{mode_errs:?}");
        assert!(mode_errs[0].contains("mode"));
        let font_errs = cfg.validate_field("app.font_size");
        assert_eq!(font_errs.len(), 1, "{font_errs:?}");
        assert!(font_errs[0].contains("font_size"));
        // 同一段内的别的字段、以及无校验规则的新路径字段都不被牵连
        assert!(cfg.validate_field("app.theme").is_empty());
        assert!(cfg.validate_field("sandbox.python_path").is_empty());
        assert!(cfg.validate_field("sandbox.pip_path").is_empty());

        cfg.llm.mode = "agent".into();
        assert!(cfg.validate_field("llm.mode").is_empty());
        assert_eq!(cfg.validate_field("app.font_size").len(), 1);
    }

    #[test]
    fn validate_field_maps_section_level_aggregate_errors() {
        let mut cfg = AppConfig::default();
        cfg.app.window_w = 100.0;
        assert_eq!(cfg.validate_field("app.window_w").len(), 1);
        assert_eq!(cfg.validate_field("app.window_h").len(), 1);
        assert!(cfg.validate_field("app.theme").is_empty());

        let mut cfg = AppConfig::default();
        cfg.context.dialog = 0.1; // 破坏五段耦合约束 + dialog 下限
        let errs = cfg.validate_field("context.dialog");
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("五项比例")));
        assert!(errs.iter().any(|e| e.contains("dialog")));
        assert!(cfg.validate_field("net.concurrency").is_empty());
    }

    #[test]
    fn save_roundtrip() {
        let dir = std::env::temp_dir().join(format!("windpencilagent_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.llm.base_url = "http://127.0.0.1:1/v1".into();
        cfg.llm.model = "m".into();
        cfg.app.theme = "dark".into();
        cfg.save(&path).unwrap();
        let back = AppConfig::load(&path).unwrap();
        assert_eq!(back.app.theme, "dark");
        assert_eq!(back.llm.model, "m");
        assert!(!dir.join("config.toml.tmp").exists(), "临时文件应已 rename");
    }

    #[test]
    fn commit_reports_change_domains() {
        let dir = std::env::temp_dir().join(format!("windpencilagent_cfg_cs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let cfg = AppConfig::default();

        // 模型域是热更新：不提示重启
        let cs = cfg.commit(&path, &[ConfigSection::Llm]).unwrap();
        assert!(cs.hot());
        assert!(cs.describe().contains("下一轮"));
        assert!(!cs.describe().contains("重启"));

        // 外观域必须提示重启
        let cs = cfg.commit(&path, &[ConfigSection::App, ConfigSection::Llm]).unwrap();
        assert!(!cs.hot());
        assert_eq!(cs.restart_sections(), vec![ConfigSection::App]);
        assert!(cs.describe().contains("重启"));

        // 重复域去重
        let cs = cfg.commit(&path, &[ConfigSection::Memory, ConfigSection::Memory]).unwrap();
        assert_eq!(cs.sections, vec![ConfigSection::Memory]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_domain_roundtrips_through_commit() {
        // 设置页保存授权范围走的就是这条路径：改内存 → commit([Scope]) → 重新加载
        let dir = std::env::temp_dir().join(format!("windpencilagent_cfg_scope_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.scope.authorized = vec!["127.0.0.1".into(), "10.0.0.0/24".into()];
        let cs = cfg.commit(&path, &[ConfigSection::Scope]).unwrap();
        assert!(cs.hot(), "授权范围下次会话即生效，不该提示重启");
        let back = AppConfig::load(&path).unwrap();
        assert_eq!(back.scope.authorized, vec!["127.0.0.1", "10.0.0.0/24"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_rejects_invalid_config_before_writing() {
        let dir = std::env::temp_dir().join(format!("windpencilagent_cfg_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.app.font_size = 99; // 非法
        assert!(cfg.commit(&path, &[ConfigSection::App]).is_err());
        assert!(!path.exists(), "校验失败不得落盘");
    }
}
