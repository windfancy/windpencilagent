//! 统一组件接口：「一切皆插接」的基础。

use std::any::Any;
use std::fmt;

/// 组件的大类，决定其在体系中的语义归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentKind {
    /// 工具：LLM 通过 function-calling 调用的原子能力单元
    Tool,
    /// 技能：SOP / prompt 资产 / 配套参考，供规划期检索注入
    Skill,
    /// 插件：可热加载的扩展包（Wasm / 进程 / dylib）
    Plugin,
    /// 规划策略：目标 → 结构化任务序列
    Planner,
    /// 反思策略：执行结果 → 计划修正
    Reflecter,
    /// LLM 供应商网关
    Llm,
    /// UI 通道：web / tui / headless / webhook
    UiChannel,
    /// 报告渲染器
    Reporter,
    /// 业务功能域智能体（scanner/analyst/verifier/reporter 等）
    DomainAgent,
    /// 存储后端
    Store,
}

impl fmt::Display for ComponentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Tool => "tool",
                Self::Skill => "skill",
                Self::Plugin => "plugin",
                Self::Planner => "planner",
                Self::Reflecter => "reflecter",
                Self::Llm => "llm",
                Self::UiChannel => "ui-channel",
                Self::Reporter => "reporter",
                Self::DomainAgent => "domain-agent",
                Self::Store => "store",
            }
        )
    }
}

/// 组件的运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentStatus {
    Loaded,
    Initialized,
    Running,
    Paused,
    Obsolete,
    Unloaded,
}

/// 组件的规格描述：供 Registry 列表展示、配置驱动装配与日志审计。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ComponentSpec {
    pub id: String,
    pub kind: ComponentKind,
    pub version: String,
    pub description: String,
    pub status: ComponentStatus,
    pub config_ref: Option<String>,
}

/// 一切能力组件的统一接口。
///
/// 组件之间只依赖该 trait 与事件总线，不互相硬调用。
pub trait Component: Send + Sync + 'static {
    /// 组件唯一标识（在同一 kind 内唯一）。
    fn id(&self) -> &str;

    /// 组件大类。
    fn kind(&self) -> ComponentKind;

    /// 组件版本。
    fn version(&self) -> &str;

    /// 人类可读的描述，供 UI 展示与日志。
    fn description(&self) -> String;

    /// 组件的当前运行状态。
    fn status(&self) -> ComponentStatus;

    /// 生成规格描述（供 Registry 呈现）。
    fn spec(&self) -> ComponentSpec {
        ComponentSpec {
            id: self.id().to_string(),
            kind: self.kind(),
            version: self.version().to_string(),
            description: self.description(),
            status: self.status(),
            config_ref: None,
        }
    }

    /// 类型擦除：供客户端把具体组件向下转型（需实现者返回自身）。
    fn as_any(&self) -> &dyn Any;
}
