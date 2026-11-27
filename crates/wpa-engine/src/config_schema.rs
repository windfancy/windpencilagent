//! 配置元数据：**一份 schema 同时驱动「内置能力设置卡片」「取值/写值」「校验」**。
//!
//! 为什么要有这一层：过去参数散落在 `config.toml`、`ChatConfig` 默认值、代码常量三处，
//! 用户想改一个压缩阈值必须手改 toml 还不知道有没有生效。现在 ——
//! ① 每个可调参数必须在 [`cards`] 里登记一次；② UI 按卡片渲染，不再手写控件；
//! ③ 读写统一走 [`AppConfig::get_field`] / [`AppConfig::set_field`]，
//! ④ 单元测试遍历整张表做「读→写→读」往返，新增参数忘了接线会直接红。
//!
//! 新增参数的顺序（缺一不可）：字段 → Default → validate → 本文件 cards()+get/set → 消费点。

use crate::config::{AppConfig, ConfigSection};

/// 控件类型。UI 按此渲染，CLI 按此解析文本。
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Bool,
    Int { min: i64, max: i64, step: i64 },
    /// 滑杆（界面拖拽设置；语义与 Int 相同，仅展示方式不同）
    Slider { min: i64, max: i64 },
    Float { min: f64, max: f64, step: f64 },
    /// 单行文本
    Text,
    /// 密钥（UI 掩码显示，不随快照外传明文）
    Secret,
    /// 路径（`dir` = 目录选择器，否则文件选择器）
    Path { dir: bool },
    /// 枚举（下拉，取值在编译期确定）
    Enum { options: &'static [&'static str] },
    /// 动态下拉：候选项在运行期解析（领域列表 / 模型预设 / 已有档案）。
    /// 语义仍是"存一个值"，只是候选项不能硬编码。
    Pick { from: PickSource },
    /// 动作型下拉：**选择即执行**，不保存取值（用于"从预设新增档案"这类一键操作）。
    /// 与 [`FieldKind::Pick`] 分开，是为了让"读→写→读"往返测试能正确跳过它。
    Action { from: PickSource },
    /// 字符串列表（UI 一行一条，空行忽略）
    List,
}

/// 动态候选项来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickSource {
    /// `domains/` 下的领域目录
    Domains,
    /// `[[llm.profiles]]` 里已有的档案名
    LlmProfiles,
    /// 内置模型预设（各家 API 的地址/模型/驱动三元组）
    LlmPresets,
}

/// 配置二级页归属。
///
/// 为什么要有这一层：11 张卡平铺在一个滚动页里，用户找"日志在哪"要翻到底。
/// 拆成 4 个二级页后，找参数靠"我想改的是模型 / 基座行为 / 安全 / 磁盘"这个判断。
/// `Domain` 特殊：领域是一级页，卡片内嵌在领域页而不是配置组里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPage {
    Model,
    Capability,
    Security,
    Data,
    Domain,
}

impl ConfigPage {
    /// 导航里的 4 个配置二级页（`Domain` 不在此列：领域是一级页，卡片内嵌在领域页）。
    pub fn nav() -> [ConfigPage; 4] {
        [
            ConfigPage::Model,
            ConfigPage::Capability,
            ConfigPage::Security,
            ConfigPage::Data,
        ]
    }

    /// 稳定 id（UI 用它做过滤，改标题不影响路由）。
    pub fn id(self) -> &'static str {
        match self {
            ConfigPage::Model => "model",
            ConfigPage::Capability => "capability",
            ConfigPage::Security => "security",
            ConfigPage::Data => "data",
            ConfigPage::Domain => "domain",
        }
    }

    /// 导航显示名。
    pub fn label(self) -> &'static str {
        match self {
            ConfigPage::Model => "模型",
            ConfigPage::Capability => "内置能力",
            ConfigPage::Security => "插件与安全",
            ConfigPage::Data => "数据与路径",
            ConfigPage::Domain => "领域",
        }
    }

    /// 一句话职责（页头副标题用；解释"这一页该不该来"）。
    pub fn hint(self) -> &'static str {
        match self {
            ConfigPage::Model => "可插拔的模型档案：一份档案 = 一个厂商 + 一个模型 + 凭证。",
            ConfigPage::Capability => "基座固有行为：上下文、循环、记忆、网络、外观。全局只有一份。",
            ConfigPage::Security => "有安全后果的开关：沙箱、授权红线、插件运行时。",
            ConfigPage::Data => "落盘位置与诊断：数据目录、日志级别与留存。",
            ConfigPage::Domain => "决定这套工具面干什么用：领域、组引用与工作范式。",
        }
    }
}

impl PickSource {
    /// 候选项（运行期解析；文件系统读失败时返回空列表，由调用方决定如何提示）。
    pub fn options(self, cfg: &AppConfig) -> Vec<String> {
        match self {
            PickSource::Domains => {
                let rows = crate::domain::list(&cfg.domains_root(), &cfg.domain.active);
                rows.into_iter().filter(|r| r.error.is_none()).map(|r| r.id).collect()
            }
            PickSource::LlmProfiles => cfg.llm.profiles.iter().map(|p| p.name.clone()).collect(),
            PickSource::LlmPresets => {
                base_llm::builtin_profiles().into_iter().map(|p| p.name).collect()
            }
        }
    }
}

/// 单个可调参数。
#[derive(Debug, Clone)]
pub struct Field {
    /// 点分键，形如 `context.compact_threshold`（与 toml 段名一致）
    pub key: &'static str,
    pub label: &'static str,
    /// 一句话说明「改了会怎样」，显示在控件下方
    pub help: &'static str,
    pub kind: FieldKind,
    /// 归属配置分区（保存回执用）
    pub section: ConfigSection,
}

impl Field {
    /// 改完是否需要重启进程（比 `section.is_hot()` 更精确：同段里可能个别项需重启）。
    fn restart(&self) -> bool {
        !self.section.is_hot()
    }
}

/// 一张配置卡片 = 内置能力设置页里的一个分组。
#[derive(Debug, Clone)]
pub struct Card {
    pub id: &'static str,
    pub title: &'static str,
    pub desc: &'static str,
    /// 归属的配置二级页（导航用）。
    pub page: ConfigPage,
    pub fields: Vec<Field>,
}

fn f(
    key: &'static str,
    label: &'static str,
    help: &'static str,
    kind: FieldKind,
    section: ConfigSection,
) -> Field {
    Field { key, label, help, kind, section }
}

/// 全部卡片（顺序即界面顺序）。
///
/// 分组原则：**按"用户想调什么"分，不按 toml 段分**。
/// 例如「压缩阈值」在 `[context]`、「保留轮数」也在 `[context]`，
/// 但「经验注入条数」在 `[memory]` —— 它们对用户是同一件事「上下文与记忆怎么用」，
/// 所以能力卡按业务含义重新聚合成一张卡，而不是照抄 toml 分段。
pub fn cards() -> Vec<Card> {
    vec![
        Card {
            id: "appearance",
            page: ConfigPage::Capability,
            title: "外观与窗口",
            desc: "主题、字号与窗口记忆。主题即时生效，字号需重启。",
            fields: vec![
                f(
                    "app.theme",
                    "主题",
                    "深色/浅色。改完立刻重算调色板，不需要重启。",
                    FieldKind::Enum { options: &["light", "dark"] },
                    ConfigSection::Theme,
                ),
                f(
                    "app.font_size",
                    "界面字号",
                    "10~24 px。拖动滑杆设置；改完需重启。",
                    FieldKind::Slider { min: 10, max: 24 },
                    ConfigSection::App,
                ),
                f(
                    "app.remember_window",
                    "记住窗口尺寸",
                    "关闭时写回当前窗口大小，下次启动沿用。",
                    FieldKind::Bool,
                    ConfigSection::App,
                ),
            ],
        },
        Card {
            id: "llm",
            page: ConfigPage::Model,
            title: "模型接入",
            desc: "接哪个模型、走哪家协议、密钥是什么。改完下一轮会话生效（会自动重建会话）。\
                    多档案的增删仍在「模型」页；这里编辑的是**当前激活档案**。",
            fields: vec![
                f(
                    "llm.add_preset",
                    "从预设新增",
                    "选一家即自动建好该家的地址/协议/模型并启用（已有同名档案则只切换，不覆盖你的密钥）。\
                     建好后在下面填密钥即可。",
                    FieldKind::Action { from: PickSource::LlmPresets },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.active",
                    "当前档案",
                    "新建会话用哪个档案。空 = 用 [llm] 顶层字段的单一配置（旧式写法）。",
                    FieldKind::Pick { from: PickSource::LlmProfiles },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.driver",
                    "协议",
                    "openai = OpenAI 兼容（DeepSeek/通义/豆包/智谱/Kimi/Ollama/vLLM/OpenRouter 全走这条）；\
                     anthropic = Claude 原生；gemini = Gemini 原生。",
                    FieldKind::Enum { options: &["openai", "anthropic", "gemini"] },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.base_url",
                    "接口地址",
                    "带版本路径，例：https://api.deepseek.com/v1 。本地 Ollama 用 http://127.0.0.1:11434/v1 。",
                    FieldKind::Text,
                    ConfigSection::Llm,
                ),
                f(
                    "llm.model",
                    "模型名",
                    "按厂商文档填，例：deepseek-chat / claude-sonnet-4 / gemini-2.0-flash。",
                    FieldKind::Text,
                    ConfigSection::Llm,
                ),
                f(
                    "llm.api_key",
                    "API 密钥",
                    "只存本地配置文件；界面掩码显示。Ollama/vLLM 这类本地端点可留空。",
                    FieldKind::Secret,
                    ConfigSection::Llm,
                ),
                f(
                    "llm.context_window",
                    "上下文窗口",
                    "该模型能吃多少 token。填小了会过早压缩，填大了会超出模型限制报错。",
                    FieldKind::Int { min: 2_000, max: 2_000_000, step: 1_000 },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.supports_tools",
                    "支持工具调用",
                    "关闭后不下发工具面（纯对话）。不确定就保持开启：真调不通会在状态栏报错。",
                    FieldKind::Bool,
                    ConfigSection::Llm,
                ),
                f(
                    "llm.mode",
                    "工作模式",
                    "agent = 携带工具执行任务；chat = 纯对话（不下发工具，省 token）。",
                    FieldKind::Enum { options: &["agent", "chat"] },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.temperature",
                    "温度",
                    "0 最稳、1 最发散。写代码/改文件建议 0.2 以下。",
                    FieldKind::Float { min: 0.0, max: 2.0, step: 0.05 },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.max_tokens",
                    "单次回复上限",
                    "单位 token。太小会在长回答中途被截断（表现为输出突然结束）。",
                    FieldKind::Int { min: 64, max: 200_000, step: 64 },
                    ConfigSection::Llm,
                ),
                f(
                    "llm.fallback",
                    "降级链",
                    "主档案不可用时按顺序尝试的档案名，一行一个。留空 = 不降级。",
                    FieldKind::List,
                    ConfigSection::Llm,
                ),
            ],
        },
        Card {
            id: "context",
            page: ConfigPage::Capability,
            title: "上下文与压缩",
            desc: "五段预算决定 system/知识/经验/工具/对话各占多少；阈值决定何时压缩历史。",
            fields: vec![
                f(
                    "context.system",
                    "system 预算占比",
                    "角色 + 注入段的合计上限。Must 约束与基座骨架永不裁剪。",
                    FieldKind::Float { min: 0.05, max: 0.4, step: 0.01 },
                    ConfigSection::Context,
                ),
                f(
                    "context.knowledge",
                    "知识注入占比",
                    "领域知识摘要可占的窗口比例。",
                    FieldKind::Float { min: 0.0, max: 0.3, step: 0.01 },
                    ConfigSection::Context,
                ),
                f(
                    "context.experience",
                    "经验注入占比",
                    "历史经验召回可占的窗口比例。",
                    FieldKind::Float { min: 0.0, max: 0.3, step: 0.01 },
                    ConfigSection::Context,
                ),
                f(
                    "context.tools",
                    "工具 schema 占比",
                    "工具越多这一项越大；压缩到 0 会导致工具面被裁。",
                    FieldKind::Float { min: 0.02, max: 0.4, step: 0.01 },
                    ConfigSection::Context,
                ),
                f(
                    "context.dialog",
                    "对话与观察占比",
                    "必须是大头（≥0.4）。压得太狠会频繁压历史、丢失细节。",
                    FieldKind::Float { min: 0.4, max: 0.9, step: 0.01 },
                    ConfigSection::Context,
                ),
                f(
                    "context.compact_threshold",
                    "压缩触发阈值",
                    "窗口占用超过该比例即压缩历史。0.8 = 用到 80% 才开始压。",
                    FieldKind::Float { min: 0.5, max: 0.95, step: 0.01 },
                    ConfigSection::Context,
                ),
                f(
                    "context.keep_recent_turns",
                    "压缩保留最近轮数",
                    "压缩时完整保留的最近对话轮数（1~12）。太小会切断工具调用链。",
                    FieldKind::Int { min: 1, max: 12, step: 1 },
                    ConfigSection::Context,
                ),
                f(
                    "context.history_cap",
                    "历史窗口条数",
                    "不进 system 的历史消息条数上限（每轮 user+assistant 各一条）。",
                    FieldKind::Int { min: 8, max: 200, step: 2 },
                    ConfigSection::Context,
                ),
                f(
                    "context.summarize_history",
                    "滚动总结历史",
                    "历史超窗时用 LLM 生成滚动摘要（需要模型在线）。关闭后只做硬裁剪。",
                    FieldKind::Bool,
                    ConfigSection::Context,
                ),
                f(
                    "context.extract_every",
                    "经验沉淀间隔",
                    "每 N 轮把对话摘要写入经验库；0 = 关闭。",
                    FieldKind::Int { min: 0, max: 100, step: 1 },
                    ConfigSection::Context,
                ),
            ],
        },
        Card {
            id: "agent",
            page: ConfigPage::Capability,
            title: "智能体循环",
            desc: "规划 / 行动 / 反思 / 协作四层的运行参数。改完下一轮会话生效。",
            fields: vec![
                f(
                    "agent.max_tool_rounds",
                    "最大工具轮数",
                    "一轮提问里最多连续调用工具的轮数（1~64）。0 = 沿用 [llm] 里的旧值。",
                    FieldKind::Int { min: 0, max: 64, step: 1 },
                    ConfigSection::Agent,
                ),
                f(
                    "agent.plan_mode",
                    "计划模式",
                    "auto = 模型自己决定何时建计划；always = 先出计划并过评审门（plan.exit）再动手；off = 不装计划层。",
                    FieldKind::Enum { options: &["auto", "always", "off"] },
                    ConfigSection::Agent,
                ),
                f(
                    "agent.reflect",
                    "启用反思层",
                    "一轮结束后对失败/偏差做复盘，结论写入经验库供后续召回。",
                    FieldKind::Bool,
                    ConfigSection::Agent,
                ),
                f(
                    "agent.reflect_min_failures",
                    "触发反思的失败数",
                    "连续失败达到该次数才触发反思（1~10），避免每次都插一段反思文本。",
                    FieldKind::Int { min: 1, max: 10, step: 1 },
                    ConfigSection::Agent,
                ),
                f(
                    "agent.subagent_enabled",
                    "启用子代理",
                    "允许派生受限子代理（独立上下文 + 仅低/中风险工具 + 不可再嵌套）。",
                    FieldKind::Bool,
                    ConfigSection::Agent,
                ),
                f(
                    "agent.subagent_max_rounds",
                    "子代理轮数上限",
                    "子代理最多执行多少轮工具（2~8）。",
                    FieldKind::Int { min: 2, max: 8, step: 1 },
                    ConfigSection::Agent,
                ),
                f(
                    "agent.parallel_pool",
                    "并行工具池",
                    "同一轮多个并发安全工具时的并发度（1~32）。",
                    FieldKind::Int { min: 1, max: 32, step: 1 },
                    ConfigSection::Agent,
                ),
                f(
                    "agent.dup_guard",
                    "重复调用守卫",
                    "同一工具 + 同参数在短窗口内重复时升级为需审批，防模型原地打转。",
                    FieldKind::Bool,
                    ConfigSection::Agent,
                ),
            ],
        },
        Card {
            id: "memory",
            page: ConfigPage::Capability,
            title: "记忆与知识",
            desc: "四层记忆里「注入哪一层、注入多少」的开关。",
            fields: vec![
                f(
                    "memory.inject_curated",
                    "注入空间精炼笔记",
                    "把工作空间的 MEMORY.md 精炼纪要注入 system prompt。",
                    FieldKind::Bool,
                    ConfigSection::Memory,
                ),
                f(
                    "memory.curated_chars",
                    "精炼笔记字符上限",
                    "注入前截断到多少字符，避免挤占对话空间。",
                    FieldKind::Int { min: 100, max: 8000, step: 100 },
                    ConfigSection::Memory,
                ),
                f(
                    "memory.experience_inject",
                    "经验注入条数",
                    "system prompt 里最多列几条召回经验。",
                    FieldKind::Int { min: 0, max: 30, step: 1 },
                    ConfigSection::Memory,
                ),
                f(
                    "memory.experience_token_budget",
                    "经验注入 token 预算",
                    "召回条目逐条累加，超出预算即停，防止经验把窗口吃满。",
                    FieldKind::Int { min: 200, max: 8000, step: 100 },
                    ConfigSection::Memory,
                ),
            ],
        },
        Card {
            id: "net",
            page: ConfigPage::Capability,
            title: "网络与请求",
            desc: "全局出网策略。插件与内置 HTTP 工具共用这套参数。",
            fields: vec![
                f(
                    "net.concurrency",
                    "全局并发上限",
                    "跨工具的并发请求数上限（1~64）。太高会被目标限流。",
                    FieldKind::Int { min: 1, max: 64, step: 1 },
                    ConfigSection::Net,
                ),
                f(
                    "net.timeout_secs",
                    "请求超时（秒）",
                    "单请求总超时（1~600）。",
                    FieldKind::Int { min: 1, max: 600, step: 5 },
                    ConfigSection::Net,
                ),
                f(
                    "net.connect_timeout_secs",
                    "连接超时（秒）",
                    "TCP/TLS 建连超时（1~120），应小于总超时。",
                    FieldKind::Int { min: 1, max: 120, step: 1 },
                    ConfigSection::Net,
                ),
                f(
                    "net.retries",
                    "失败重试次数",
                    "仅对幂等请求重试（0~10）。写操作不应重试。",
                    FieldKind::Int { min: 0, max: 10, step: 1 },
                    ConfigSection::Net,
                ),
                f(
                    "net.user_agent",
                    "User-Agent",
                    "空 = 内置默认（便于在目标侧识别自身流量）。",
                    FieldKind::Text,
                    ConfigSection::Net,
                ),
                f(
                    "net.proxy",
                    "代理",
                    "http://host:port 或 socks5://host:port；空 = 直连。",
                    FieldKind::Text,
                    ConfigSection::Net,
                ),
                f(
                    "net.per_host_qps",
                    "单域 QPS 上限",
                    "同一域名每秒请求上限，0 = 不限。用于礼貌抓取。",
                    FieldKind::Int { min: 0, max: 100, step: 1 },
                    ConfigSection::Net,
                ),
            ],
        },
        Card {
            id: "sandbox",
            page: ConfigPage::Security,
            title: "沙箱与审批",
            desc: "命令执行隔离与人工审批策略。安全相关，默认保守。",
            fields: vec![
                f(
                    "sandbox.restricted_token",
                    "受限令牌执行",
                    "os.exec / sandbox.python 用 Windows 受限令牌运行（降权、去特权）。",
                    FieldKind::Bool,
                    ConfigSection::Sandbox,
                ),
                f(
                    "sandbox.app_container",
                    "AppContainer 隔离",
                    "额外套一层 AppContainer（Win8+）。失败会自动回落受限令牌。",
                    FieldKind::Bool,
                    ConfigSection::Sandbox,
                ),
                f(
                    "sandbox.python_path",
                    "Python 解释器路径",
                    "留空自动探测（程序子目录 python/python.exe）。填绝对路径可指定解释器；改完需重启生效。",
                    FieldKind::Path { dir: false },
                    ConfigSection::Sandbox,
                ),
                f(
                    "sandbox.pip_path",
                    "pip 路径",
                    "留空自动探测（程序子目录 python/python.exe）——用其同目录的 pip（python -m pip）。填绝对路径可指定 pip 可执行文件；改完需重启生效。",
                    FieldKind::Path { dir: false },
                    ConfigSection::Sandbox,
                ),
                f(
                    "run.auto_approve",
                    "自动批准全部工具",
                    "危险：跳过人工审批。仅自动化脚本场景使用，交互使用请保持关闭。",
                    FieldKind::Bool,
                    ConfigSection::Run,
                ),
                f(
                    "run.approval_timeout_secs",
                    "审批超时（秒）",
                    "超时视为拒绝（fail-closed）。空 = 不超时。",
                    FieldKind::Int { min: 10, max: 3600, step: 10 },
                    ConfigSection::Run,
                ),
            ],
        },
        Card {
            id: "scope",
            page: ConfigPage::Security,
            title: "授权与红线",
            desc: "只允许操作列出的目标（空 = 未声明，工具调用会被拒）；拒绝清单优先级更高。",
            fields: vec![
                f(
                    "scope.authorized",
                    "授权目标",
                    "一行一个：IP / 域名 / CIDR（如 10.0.0.5、example.com、10.0.0.0/24）。空表示未声明 ⇒ fail-closed。",
                    FieldKind::List,
                    ConfigSection::Scope,
                ),
                f(
                    "scope.denylist",
                    "拒绝清单",
                    "一行一个。「除这些之外」——命中即拒，优先于授权目标。",
                    FieldKind::List,
                    ConfigSection::Scope,
                ),
            ],
        },
        Card {
            id: "plugins",
            page: ConfigPage::Security,
            title: "插件运行时",
            desc: "原子工具插件（C ABI dll）的加载与调度。关闭后可快速判断问题是否由插件引起。",
            fields: vec![
                f(
                    "plugins.enabled",
                    "启用插件",
                    "关闭后只保留内置工具。",
                    FieldKind::Bool,
                    ConfigSection::Plugin,
                ),
                f(
                    "plugins.dir",
                    "插件目录",
                    "空 = 自动探测（exe 同级 plugins/，开发态向上找项目根）。",
                    FieldKind::Path { dir: true },
                    ConfigSection::Plugin,
                ),
                f(
                    "plugins.timeout_ms",
                    "单次调用超时（毫秒）",
                    "兜底超时；插件内部应有更短的超时。",
                    FieldKind::Int { min: 1000, max: 600_000, step: 1000 },
                    ConfigSection::Plugin,
                ),
                f(
                    "plugins.max_cached",
                    "实例缓存上限",
                    "一次装配最多保留多少个插件实例（LRU）。",
                    FieldKind::Int { min: 1, max: 128, step: 1 },
                    ConfigSection::Plugin,
                ),
            ],
        },
        Card {
            id: "logging",
            page: ConfigPage::Data,
            title: "日志与数据",
            desc: "日志落点与保留策略。桌面端不再往控制台打日志，全部写文件。",
            fields: vec![
                f(
                    "logging.level",
                    "日志级别",
                    "debug/trace 会产生大量输出，排查完记得调回 info。",
                    FieldKind::Enum { options: &["error", "warn", "info", "debug", "trace"] },
                    ConfigSection::Logging,
                ),
                f(
                    "logging.dir",
                    "日志目录",
                    "空 = <数据目录>/logs。按天滚动。",
                    FieldKind::Path { dir: true },
                    ConfigSection::Logging,
                ),
                f(
                    "logging.keep_days",
                    "日志保留天数",
                    "超过天数的日志文件在启动时清理（1~365）。",
                    FieldKind::Int { min: 1, max: 365, step: 1 },
                    ConfigSection::Logging,
                ),
                f(
                    "logging.max_mb",
                    "单文件上限（MB）",
                    "单个日志文件超过该大小即滚动。",
                    FieldKind::Int { min: 1, max: 500, step: 1 },
                    ConfigSection::Logging,
                ),
                f(
                    "run.data_dir",
                    "数据目录",
                    "会话库 / 任务 / 经验 / 索引的落点。空 = exe 同级 data/。改完需重启。",
                    FieldKind::Path { dir: true },
                    ConfigSection::Run,
                ),
            ],
        },
        Card {
            id: "domain",
            page: ConfigPage::Domain,
            title: "领域",
            desc: "领域是描述包（角色 / 工作范式 / 常识 / 红线 / 工具面），切换不改变程序本体。",
            fields: vec![
                f(
                    "domain.active",
                    "激活领域",
                    "domains/ 下的目录名。切换后需要重新装配（桌面端点「重新装配」）才换工具面与角色。",
                    FieldKind::Pick { from: PickSource::Domains },
                    ConfigSection::Domain,
                ),
                f(
                    "domain.keep_session",
                    "切换保留会话",
                    "开启后换领域不新开会话；旧领域的注入段会在下一轮更新。",
                    FieldKind::Bool,
                    ConfigSection::Domain,
                ),
                f(
                    "domain.dir",
                    "领域目录",
                    "空 = 自动探测（exe 同级 domains/，开发态向上找项目根）。",
                    FieldKind::Path { dir: true },
                    ConfigSection::Domain,
                ),
            ],
        },
    ]
}

/// 全部字段（扁平），顺序与卡片一致。
pub fn fields() -> Vec<Field> {
    cards().into_iter().flat_map(|c| c.fields).collect()
}

/// 按 key 找字段定义。
pub fn field(key: &str) -> Option<Field> {
    fields().into_iter().find(|f| f.key == key)
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "是" | "开" => Ok(true),
        "false" | "0" | "off" | "no" | "否" | "关" => Ok(false),
        other => Err(format!("不是布尔值：{other}（用 true/false）")),
    }
}

fn parse_list(v: &str) -> Vec<String> {
    v.split(['\n', ',', '，'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl AppConfig {
    /// 当前激活的模型档案（按名字找；找不到或未指定 = None，此时编辑落在 `[llm]` 顶层字段）。
    pub fn active_llm_profile(&self) -> Option<&base_llm::LlmProfile> {
        let name = self.llm.active.as_deref()?;
        self.llm.profiles.iter().find(|p| p.name == name)
    }

    fn active_llm_profile_mut(&mut self) -> Option<&mut base_llm::LlmProfile> {
        let name = self.llm.active.clone()?;
        self.llm.profiles.iter_mut().find(|p| p.name == name)
    }

    /// 取"用于写入的档案"：没有激活档案时用 `[llm]` 顶层字段现建一个。
    ///
    /// 为什么允许自动建：`context_window` / `supports_tools` 只存在于档案上，
    /// 若严格要求"先选档案"，用户点这两项会撞到一个没有出口的错误框。
    /// 但自动建**必须**有地址和模型名可继承——否则建出来的是一个校验不过的空档案，
    /// 用户下次保存会收到"档案 默认 缺少 base_url 或 model"这种莫名其妙的错误。
    /// 所以这里宁可报一个能照做的错：先去"从预设新增"选一家。
    fn ensure_active_llm_profile(&mut self) -> Result<&mut base_llm::LlmProfile, String> {
        let name = self.llm.active.clone().unwrap_or_else(|| "默认".to_string());
        let idx = match self.llm.profiles.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                if self.llm.base_url.trim().is_empty() || self.llm.model.trim().is_empty() {
                    return Err(
                        "还没有模型档案：先在「从预设新增」里选一家厂商（或填好接口地址与模型名）再改这项"
                            .into(),
                    );
                }
                let driver = base_llm::canonical_driver(&self.llm.driver)
                    .unwrap_or("openai")
                    .to_string();
                self.llm.profiles.push(base_llm::LlmProfile {
                    name: name.clone(),
                    driver,
                    base_url: self.llm.base_url.clone(),
                    model: self.llm.model.clone(),
                    api_key: self.llm.api_key.clone(),
                    temperature: self.llm.temperature,
                    max_tokens: self.llm.max_tokens,
                    enabled: true,
                    mode: self.llm.mode.clone(),
                    context_window: None,
                    supports_tools: None,
                    supports_vision: None,
                    cost_per_1k_in: None,
                    cost_per_1k_out: None,
                });
                self.llm.profiles.len() - 1
            }
        };
        if self.llm.active.is_none() {
            self.llm.active = Some(name);
        }
        Ok(&mut self.llm.profiles[idx])
    }

    /// 读取某个可调参数的当前值（文本形态，供 UI 回填 / CLI 展示）。
    ///
    /// 模型类字段的读写口径：**有激活档案就落在档案上，否则落在 `[llm]` 顶层**。
    /// 这条口径必须读写一致，否则会出现"改完看着变了、实际存在别处"。
    pub fn get_field(&self, key: &str) -> Result<String, String> {
        let p = self.active_llm_profile();
        let v = match key {
            // 动作型字段没有"当前值"：下拉选完就执行，选中项随即清空
            "llm.add_preset" => String::new(),
            "llm.active" => self.llm.active.clone().unwrap_or_default(),
            "llm.driver" => p.map(|p| p.driver.clone()).unwrap_or_else(|| self.llm.driver.clone()),
            "llm.base_url" => p.map(|p| p.base_url.clone()).unwrap_or_else(|| self.llm.base_url.clone()),
            "llm.model" => p.map(|p| p.model.clone()).unwrap_or_else(|| self.llm.model.clone()),
            "llm.api_key" => p
                .and_then(|p| p.api_key.clone())
                .or_else(|| self.llm.api_key.clone())
                .unwrap_or_default(),
            "llm.context_window" => p.and_then(|p| p.context_window).unwrap_or(32_000).to_string(),
            "llm.supports_tools" => {
                p.and_then(|p| p.supports_tools).unwrap_or(true).to_string()
            }
            "llm.mode" => p.map(|p| p.mode.clone()).unwrap_or_else(|| self.llm.mode.clone()),
            "llm.temperature" => p
                .and_then(|p| p.temperature)
                .or(self.llm.temperature)
                .map(fmt_f32)
                .unwrap_or_default(),
            "llm.max_tokens" => p
                .and_then(|p| p.max_tokens)
                .or(self.llm.max_tokens)
                .map(|t| t.to_string())
                .unwrap_or_default(),
            "llm.fallback" => self.llm.fallback.join("\n"),
            "app.theme" => self.app.theme.clone(),
            "app.font_size" => self.app.font_size.to_string(),
            "app.remember_window" => self.app.remember_window.to_string(),
            "context.system" => fmt_f32(self.context.system),
            "context.knowledge" => fmt_f32(self.context.knowledge),
            "context.experience" => fmt_f32(self.context.experience),
            "context.tools" => fmt_f32(self.context.tools),
            "context.dialog" => fmt_f32(self.context.dialog),
            "context.compact_threshold" => fmt_f64(self.context.compact_threshold),
            "context.keep_recent_turns" => self.context.keep_recent_turns.to_string(),
            "context.history_cap" => self.context.history_cap.to_string(),
            "context.summarize_history" => self.context.summarize_history.to_string(),
            "context.extract_every" => self.context.extract_every.to_string(),
            "agent.max_tool_rounds" => self.agent.max_tool_rounds.to_string(),
            "agent.plan_mode" => self.agent.plan_mode.clone(),
            "agent.reflect" => self.agent.reflect.to_string(),
            "agent.reflect_min_failures" => self.agent.reflect_min_failures.to_string(),
            "agent.subagent_enabled" => self.agent.subagent_enabled.to_string(),
            "agent.subagent_max_rounds" => self.agent.subagent_max_rounds.to_string(),
            "agent.parallel_pool" => self.agent.parallel_pool.to_string(),
            "agent.dup_guard" => self.agent.dup_guard.to_string(),
            "memory.inject_curated" => self.memory.inject_curated.to_string(),
            "memory.curated_chars" => self.memory.curated_chars.to_string(),
            "memory.experience_inject" => self.memory.experience_inject.to_string(),
            "memory.experience_token_budget" => self.memory.experience_token_budget.to_string(),
            "net.concurrency" => self.net.concurrency.to_string(),
            "net.timeout_secs" => self.net.timeout_secs.to_string(),
            "net.connect_timeout_secs" => self.net.connect_timeout_secs.to_string(),
            "net.retries" => self.net.retries.to_string(),
            "net.user_agent" => self.net.user_agent.clone(),
            "net.proxy" => self.net.proxy.clone().unwrap_or_default(),
            "net.per_host_qps" => self.net.per_host_qps.to_string(),
            "sandbox.restricted_token" => self.sandbox.restricted_token.to_string(),
            "sandbox.app_container" => self.sandbox.app_container.to_string(),
            "sandbox.python_path" => self.sandbox.python_path.clone().unwrap_or_default(),
            "sandbox.pip_path" => self.sandbox.pip_path.clone().unwrap_or_default(),
            "run.auto_approve" => self.run.auto_approve.to_string(),
            "run.approval_timeout_secs" => {
                self.run.approval_timeout_secs.map(|v| v.to_string()).unwrap_or_default()
            }
            "scope.authorized" => self.scope.authorized.join("\n"),
            "scope.denylist" => self.scope.denylist.join("\n"),
            "plugins.enabled" => self.plugins.enabled.to_string(),
            "plugins.dir" => self.plugins.dir.clone().unwrap_or_default(),
            "plugins.timeout_ms" => self.plugins.timeout_ms.to_string(),
            "plugins.max_cached" => self.plugins.max_cached.to_string(),
            "logging.level" => self.logging.level.clone(),
            "logging.dir" => self.logging.dir.clone().unwrap_or_default(),
            "logging.keep_days" => self.logging.keep_days.to_string(),
            "logging.max_mb" => self.logging.max_mb.to_string(),
            "run.data_dir" => self.run.data_dir.clone().unwrap_or_default(),
            "domain.active" => self.domain.active.clone(),
            "domain.keep_session" => self.domain.keep_session.to_string(),
            "domain.dir" => self.domain.dir.clone().unwrap_or_default(),
            other => return Err(format!("未知配置项：{other}")),
        };
        Ok(v)
    }

    /// 写入某个可调参数。值先按字段类型解析，范围交给 [`AppConfig::validate`] 兜底。
    pub fn set_field(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        let int = |s: &str| -> Result<i64, String> {
            s.parse::<i64>().map_err(|_| format!("需要整数：{s}"))
        };
        let uint = |s: &str| -> Result<u64, String> {
            let n = int(s)?;
            if n < 0 {
                return Err(format!("不能为负数：{n}"));
            }
            Ok(n as u64)
        };
        let usize_ = |s: &str| -> Result<usize, String> { Ok(uint(s)? as usize) };
        let opt_text = |s: &str| -> Option<String> {
            if s.is_empty() { None } else { Some(s.to_string()) }
        };
        match key {
            // 动作型：选择即执行（不保存取值）
            "llm.add_preset" => {
                if !v.is_empty() {
                    self.add_llm_preset(v)?;
                }
            }
            "llm.active" => self.llm.active = opt_text(v),
            "llm.driver" => {
                // 驱动名先归一化再落盘：写 "claude" 也认，但存的是 "anthropic"，
                // 避免配置里出现同一协议的多种写法（分派时要逐个比对）
                let d = base_llm::canonical_driver(v)?;
                match self.active_llm_profile_mut() {
                    Some(p) => p.driver = d.to_string(),
                    None => self.llm.driver = d.to_string(),
                }
            }
            "llm.base_url" => match self.active_llm_profile_mut() {
                Some(p) => p.base_url = v.to_string(),
                None => self.llm.base_url = v.to_string(),
            },
            "llm.model" => match self.active_llm_profile_mut() {
                Some(p) => p.model = v.to_string(),
                None => self.llm.model = v.to_string(),
            },
            "llm.api_key" => match self.active_llm_profile_mut() {
                Some(p) => p.api_key = opt_text(v),
                None => self.llm.api_key = opt_text(v),
            },
            "llm.context_window" => {
                let n = uint(v)?.clamp(2_000, 2_000_000) as u32;
                self.ensure_active_llm_profile()?.context_window = Some(n);
            }
            "llm.supports_tools" => {
                let b = parse_bool(v)?;
                self.ensure_active_llm_profile()?.supports_tools = Some(b);
            }
            "llm.mode" => match self.active_llm_profile_mut() {
                Some(p) => p.mode = v.to_string(),
                None => self.llm.mode = v.to_string(),
            },
            "llm.temperature" => {
                let t: f32 = if v.is_empty() {
                    0.2
                } else {
                    v.parse().map_err(|_| format!("需要小数：{v}"))?
                };
                match self.active_llm_profile_mut() {
                    Some(p) => p.temperature = Some(t),
                    None => self.llm.temperature = Some(t),
                }
            }
            "llm.max_tokens" => {
                let n = uint(v)? as u32;
                match self.active_llm_profile_mut() {
                    Some(p) => p.max_tokens = Some(n),
                    None => self.llm.max_tokens = Some(n),
                }
            }
            "llm.fallback" => self.llm.fallback = parse_list(v),
            "app.theme" => self.app.theme = v.to_string(),
            "app.font_size" => self.app.font_size = uint(v)? as u32,
            "app.remember_window" => self.app.remember_window = parse_bool(v)?,
            "context.system" => self.context.system = v.parse().map_err(|_| "需要小数")?,
            "context.knowledge" => self.context.knowledge = v.parse().map_err(|_| "需要小数")?,
            "context.experience" => self.context.experience = v.parse().map_err(|_| "需要小数")?,
            "context.tools" => self.context.tools = v.parse().map_err(|_| "需要小数")?,
            "context.dialog" => self.context.dialog = v.parse().map_err(|_| "需要小数")?,
            "context.compact_threshold" => {
                self.context.compact_threshold = v.parse().map_err(|_| "需要小数")?
            }
            "context.keep_recent_turns" => self.context.keep_recent_turns = usize_(v)?,
            "context.history_cap" => self.context.history_cap = usize_(v)?,
            "context.summarize_history" => self.context.summarize_history = parse_bool(v)?,
            "context.extract_every" => self.context.extract_every = usize_(v)?,
            "agent.max_tool_rounds" => self.agent.max_tool_rounds = usize_(v)?,
            "agent.plan_mode" => self.agent.plan_mode = v.to_string(),
            "agent.reflect" => self.agent.reflect = parse_bool(v)?,
            "agent.reflect_min_failures" => self.agent.reflect_min_failures = usize_(v)?,
            "agent.subagent_enabled" => self.agent.subagent_enabled = parse_bool(v)?,
            "agent.subagent_max_rounds" => self.agent.subagent_max_rounds = usize_(v)?,
            "agent.parallel_pool" => self.agent.parallel_pool = usize_(v)?,
            "agent.dup_guard" => self.agent.dup_guard = parse_bool(v)?,
            "memory.inject_curated" => self.memory.inject_curated = parse_bool(v)?,
            "memory.curated_chars" => self.memory.curated_chars = usize_(v)?,
            "memory.experience_inject" => self.memory.experience_inject = usize_(v)?,
            "memory.experience_token_budget" => {
                self.memory.experience_token_budget = usize_(v)?
            }
            "net.concurrency" => self.net.concurrency = usize_(v)?,
            "net.timeout_secs" => self.net.timeout_secs = uint(v)?,
            "net.connect_timeout_secs" => self.net.connect_timeout_secs = uint(v)?,
            "net.retries" => self.net.retries = uint(v)? as u32,
            "net.user_agent" => self.net.user_agent = v.to_string(),
            "net.proxy" => self.net.proxy = opt_text(v),
            "net.per_host_qps" => self.net.per_host_qps = uint(v)? as u32,
            "sandbox.restricted_token" => self.sandbox.restricted_token = parse_bool(v)?,
            "sandbox.app_container" => self.sandbox.app_container = parse_bool(v)?,
            "sandbox.python_path" => self.sandbox.python_path = opt_text(v),
            "sandbox.pip_path" => self.sandbox.pip_path = opt_text(v),
            "run.auto_approve" => self.run.auto_approve = parse_bool(v)?,
            "run.approval_timeout_secs" => self.run.approval_timeout_secs = Some(uint(v)?),
            "scope.authorized" => self.scope.authorized = parse_list(v),
            "scope.denylist" => self.scope.denylist = parse_list(v),
            "plugins.enabled" => self.plugins.enabled = parse_bool(v)?,
            "plugins.dir" => self.plugins.dir = opt_text(v),
            "plugins.timeout_ms" => self.plugins.timeout_ms = uint(v)?,
            "plugins.max_cached" => self.plugins.max_cached = usize_(v)?,
            "logging.level" => self.logging.level = v.to_string(),
            "logging.dir" => self.logging.dir = opt_text(v),
            "logging.keep_days" => self.logging.keep_days = uint(v)? as u32,
            "logging.max_mb" => self.logging.max_mb = uint(v)? as u32,
            "run.data_dir" => self.run.data_dir = opt_text(v),
            "domain.active" => self.domain.active = v.to_string(),
            "domain.keep_session" => self.domain.keep_session = parse_bool(v)?,
            "domain.dir" => self.domain.dir = opt_text(v),
            other => return Err(format!("未知配置项：{other}")),
        }
        Ok(())
    }

    /// 需要重启才能生效的配置分区（按本次修改涉及的 key 汇总）。
    pub fn restart_sections_for(&self, keys: &[String]) -> Vec<ConfigSection> {
        let mut out: Vec<ConfigSection> = Vec::new();
        for k in keys {
            if let Some(f) = field(k) {
                if f.restart() && !out.contains(&f.section) {
                    out.push(f.section);
                }
            }
        }
        out
    }
}

fn fmt_f32(v: f32) -> String {
    // 0.10 而不是 0.1：五段比例并排展示时对齐才好读
    format!("{v:.2}")
}
fn fmt_f64(v: f64) -> String {
    format!("{v:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_roundtrips() {
        // 每个登记项都必须能读、能用样例写回、再读回同一个值。
        //
        // 两类字段例外，各自有专项测试：
        // - `Action`：选择即执行，本来就不存值（`add_preset_creates_then_switches`）；
        // - `Pick`：候选值依赖运行时（领域目录/已存在档案），默认配置下不一定有候选，
        //   这里用"写一个已存在的合法值"的方式跳过取值校验（`pick_sources_resolve`）。
        let mut cfg = AppConfig::default();
        // 先建一个档案，让 `llm.*` 的档案级字段有落点
        cfg.add_llm_preset("openai").unwrap();
        for f in fields() {
            let before = cfg.get_field(f.key).unwrap_or_else(|e| panic!("{} 读失败：{e}", f.key));
            assert!(!f.label.is_empty(), "{} 缺 label", f.key);
            assert!(!f.help.is_empty(), "{} 缺说明", f.key);
            let sample = match &f.kind {
                FieldKind::Action { .. } => continue,
                FieldKind::Pick { from } => match from.options(&cfg).first() {
                    Some(v) => v.clone(),
                    None => continue, // 无候选（如 domains 目录还不存在）
                },
                FieldKind::Bool => "true".to_string(),
                FieldKind::Int { min, .. } | FieldKind::Slider { min, .. } => min.to_string(),
                FieldKind::Float { min, .. } => format!("{min}"),
                FieldKind::Enum { options } => options.last().unwrap().to_string(),
                FieldKind::List => "a\nb".to_string(),
                FieldKind::Text | FieldKind::Secret => "sample".to_string(),
                FieldKind::Path { .. } => "C:/tmp".to_string(),
            };
            cfg.set_field(f.key, &sample)
                .unwrap_or_else(|e| panic!("{} 写失败：{e}", f.key));
            let after = cfg.get_field(f.key).unwrap();
            assert_ne!(after, "", "{} 写后读为空", f.key);
            assert!(before != after || sample == before, "{} 写入未生效", f.key);
        }
    }

    #[test]
    fn sandbox_path_fields_none_roundtrip_as_empty_string() {
        let mut cfg = AppConfig::default();
        assert!(cfg.sandbox.python_path.is_none());
        assert!(cfg.sandbox.pip_path.is_none());
        assert_eq!(cfg.get_field("sandbox.python_path").unwrap(), "");
        assert_eq!(cfg.get_field("sandbox.pip_path").unwrap(), "");

        // 卡片登记为文件选择器（非目录），归属沙箱分区
        for key in ["sandbox.python_path", "sandbox.pip_path"] {
            let f = field(key).unwrap_or_else(|| panic!("{key} 未登记卡片"));
            assert_eq!(f.kind, FieldKind::Path { dir: false });
            assert_eq!(f.section, ConfigSection::Sandbox);
        }

        cfg.set_field("sandbox.python_path", "C:/tools/python.exe").unwrap();
        cfg.set_field("sandbox.pip_path", "C:/tools/Scripts/pip.exe").unwrap();
        assert_eq!(cfg.sandbox.python_path.as_deref(), Some("C:/tools/python.exe"));
        assert_eq!(cfg.sandbox.pip_path.as_deref(), Some("C:/tools/Scripts/pip.exe"));
        assert_eq!(cfg.get_field("sandbox.python_path").unwrap(), "C:/tools/python.exe");
        assert_eq!(cfg.get_field("sandbox.pip_path").unwrap(), "C:/tools/Scripts/pip.exe");

        // 清空（含空白）回到 None，回填给 UI 的是空串
        cfg.set_field("sandbox.python_path", "").unwrap();
        cfg.set_field("sandbox.pip_path", "   ").unwrap();
        assert!(cfg.sandbox.python_path.is_none());
        assert!(cfg.sandbox.pip_path.is_none());
        assert_eq!(cfg.get_field("sandbox.python_path").unwrap(), "");
        assert_eq!(cfg.get_field("sandbox.pip_path").unwrap(), "");
    }

    #[test]
    fn list_and_bool_parsing() {
        let mut cfg = AppConfig::default();
        cfg.set_field("scope.authorized", "127.0.0.1\n10.0.0.0/24\n\n").unwrap();
        assert_eq!(cfg.scope.authorized, vec!["127.0.0.1", "10.0.0.0/24"]);
        cfg.set_field("scope.denylist", "a, b，c").unwrap();
        assert_eq!(cfg.scope.denylist, vec!["a", "b", "c"]);
        cfg.set_field("agent.reflect", "关").unwrap();
        assert!(!cfg.agent.reflect);
        assert!(cfg.set_field("agent.reflect", "maybe").is_err());
        assert!(cfg.set_field("no.such.key", "1").is_err());
    }

    #[test]
    fn setting_every_field_keeps_config_valid() {
        // 用「合法样例」写满全部字段后，配置必须仍是合法的 ——
        // 否则 UI 上随便调一项就存不下去。
        //
        // 唯一例外是 `[context]` 的五段比例：它们是一组**耦合约束**（和≈1、dialog≥0.4），
        // 单独把某一项调到下限必然破坏整体，这不是 bug 而是校验该做的事。
        // 因此这里跳过这五项，改由 `validate` 的专项测试覆盖。
        const COUPLED: &[&str] = &[
            "context.system",
            "context.knowledge",
            "context.experience",
            "context.tools",
            "context.dialog",
        ];
        // 必填文本项不能清空，否则档案校验不过（这不是耦合约束，是必填项）
        const REQUIRED_TEXT: &[&str] = &["llm.base_url", "llm.model"];
        let mut cfg = AppConfig::default();
        // 先有一个真档案：模型类字段都落在激活档案上
        cfg.add_llm_preset("openai").unwrap();
        for f in fields() {
            if COUPLED.contains(&f.key) {
                continue;
            }
            let sample = match &f.kind {
                FieldKind::Action { .. } => continue,
                FieldKind::Pick { .. } => continue,
                FieldKind::Bool => "false".to_string(),
                FieldKind::Int { min, .. } | FieldKind::Slider { min, .. } => min.to_string(),
                FieldKind::Float { min, .. } => format!("{min}"),
                FieldKind::Enum { options } => options[0].to_string(),
                FieldKind::List => String::new(),
                FieldKind::Text | FieldKind::Secret if REQUIRED_TEXT.contains(&f.key) => {
                    "sample".to_string()
                }
                FieldKind::Text | FieldKind::Secret => String::new(),
                FieldKind::Path { .. } => String::new(),
            };
            cfg.set_field(f.key, &sample)
                .unwrap_or_else(|e| panic!("{} 写失败：{e}", f.key));
        }
        // 空 domain.active 由样例的清空触发，单独补回默认
        cfg.set_field("domain.active", "default").unwrap();
        let errs = cfg.validate();
        assert!(errs.is_empty(), "默认值 + 合法样例应通过校验：{errs:?}");
    }

    #[test]
    fn restart_sections_are_reported() {
        let cfg = AppConfig::default();
        let s = cfg.restart_sections_for(&["app.font_size".to_string()]);
        assert_eq!(s, vec![ConfigSection::App]);
        let s = cfg.restart_sections_for(&["context.compact_threshold".to_string()]);
        assert!(s.is_empty(), "压缩阈值应热更新");
        assert_eq!(field("context.compact_threshold").unwrap().section, ConfigSection::Context);
    }

    #[test]
    fn schema_covers_every_section_except_tree() {
        // 防止「新增了配置分区却忘了登记卡片」
        let mut seen: Vec<ConfigSection> = fields().iter().map(|f| f.section).collect();
        seen.sort_by_key(|s| s.as_str());
        seen.dedup();
        for need in [
            ConfigSection::App,
            ConfigSection::Theme,
            ConfigSection::Llm,
            ConfigSection::Context,
            ConfigSection::Agent,
            ConfigSection::Memory,
            ConfigSection::Net,
            ConfigSection::Sandbox,
            ConfigSection::Plugin,
            ConfigSection::Run,
            ConfigSection::Scope,
            ConfigSection::Logging,
            ConfigSection::Domain,
        ] {
            assert!(seen.contains(&need), "配置分区 {} 没有任何卡片项", need.as_str());
        }
    }

    #[test]
    fn add_preset_creates_then_only_switches() {
        let mut cfg = AppConfig::default();
        cfg.add_llm_preset("deepseek").expect("预设应存在");
        assert_eq!(cfg.llm.active.as_deref(), Some("deepseek"));
        assert_eq!(cfg.llm.profiles.len(), 1);
        let url = cfg.llm.profiles[0].base_url.clone();

        // 用户改过地址后再次选同一个预设：只切换，不许把地址重置回去
        cfg.set_field("llm.base_url", "https://my.proxy/v1").unwrap();
        assert_eq!(cfg.llm.profiles[0].base_url, "https://my.proxy/v1");
        cfg.add_llm_preset("deepseek").unwrap();
        assert_eq!(cfg.llm.profiles[0].base_url, "https://my.proxy/v1", "不许覆盖用户改动");
        assert_ne!(url, cfg.llm.profiles[0].base_url);

        assert!(cfg.add_llm_preset("不存在的厂家").is_err());
        // 动作型字段通过 set_field 走同一个入口
        cfg.set_field("llm.add_preset", "openai").unwrap();
        assert_eq!(cfg.llm.active.as_deref(), Some("openai"));
        assert_eq!(cfg.get_field("llm.add_preset").unwrap(), "", "动作型字段不存值");
    }

    #[test]
    fn field_edits_land_on_the_active_profile() {
        let mut cfg = AppConfig::default();
        cfg.add_llm_preset("anthropic").unwrap();
        cfg.set_field("llm.model", "claude-sonnet-4").unwrap();
        cfg.set_field("llm.api_key", "sk-ant-xxx").unwrap();
        cfg.set_field("llm.context_window", "200000").unwrap();
        let p = cfg.active_llm_profile().expect("应有激活档案");
        assert_eq!(p.model, "claude-sonnet-4");
        assert_eq!(p.api_key.as_deref(), Some("sk-ant-xxx"));
        assert_eq!(p.context_window, Some(200_000));
        // 读回来是同一份
        assert_eq!(cfg.get_field("llm.model").unwrap(), "claude-sonnet-4");

        // 切到另一个档案：读写跟着切（不串档）
        cfg.add_llm_preset("openai").unwrap();
        assert_ne!(cfg.get_field("llm.model").unwrap(), "claude-sonnet-4");
        assert_eq!(cfg.get_field("llm.api_key").unwrap(), "", "新档案没有密钥");
    }

    #[test]
    fn driver_is_canonicalized_on_write() {
        let mut cfg = AppConfig::default();
        cfg.add_llm_preset("openai").unwrap();
        cfg.set_field("llm.driver", "claude").unwrap();
        assert_eq!(cfg.get_field("llm.driver").unwrap(), "anthropic");
        assert!(cfg.set_field("llm.driver", "nonsense").is_err());
    }

    #[test]
    fn pick_sources_resolve_without_panicking() {
        let cfg = AppConfig::default();
        // 默认配置下没有 domains 目录也没有档案：返回空列表而不是报错
        let _ = PickSource::Domains.options(&cfg);
        assert!(PickSource::LlmProfiles.options(&cfg).is_empty());
        let presets = PickSource::LlmPresets.options(&cfg);
        assert!(presets.len() >= 10, "内置预设应覆盖主流厂商：{}", presets.len());
        for need in ["openai", "anthropic", "gemini", "deepseek"] {
            assert!(presets.contains(&need.to_string()), "缺预设 {need}");
        }
    }

    /// 配置拆 4 个二级页后，每页必须有卡：空页等于导航里的死入口。
    #[test]
    fn every_config_page_has_at_least_one_card() {
        let cards = cards();
        for page in ConfigPage::nav() {
            let n = cards.iter().filter(|c| c.page == page).count();
            assert!(n > 0, "配置页 {} 一张卡都没有", page.id());
        }
        // 领域是一级页，卡片内嵌在领域页里 —— 必须有且只有一张
        let domain = cards.iter().filter(|c| c.page == ConfigPage::Domain).count();
        assert_eq!(domain, 1, "领域卡应当只有一张（切换/保留会话）");
    }

    /// 导航 id 必须唯一且能反查到页（UI 靠 id 取卡片列表）。
    #[test]
    fn config_page_ids_are_unique_and_nonempty() {
        let mut ids: Vec<&str> = ConfigPage::nav().iter().map(|p| p.id()).collect();
        ids.push(ConfigPage::Domain.id());
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "配置页 id 重复：{ids:?}");
        for p in ConfigPage::nav() {
            assert!(!p.label().is_empty() && !p.hint().is_empty(), "{} 缺显示名/说明", p.id());
        }
    }
}
