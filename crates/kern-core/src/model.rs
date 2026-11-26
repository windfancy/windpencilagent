//! 通用数据模型：会话 / 身份 / 事实 / 执行上下文。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 会话标识：一次人机对话 / 一次作业的顶层归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 智能体标识（Orchestrator / scanner / analyst / verifier / reporter 等）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 任务标识：plan 内唯一（如 t01）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl AsRef<str> for TaskId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 风险级别：决定是否需要人工审批节点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskLevel {
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    /// 是否需要走 HITL 人工审批。
    pub fn needs_approval(&self) -> bool {
        matches!(self, Self::High | Self::Critical)
    }
}

/// 执行上下文：传递给工具与沙箱的运行时信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionContext {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub target: String,
    pub risk: RiskLevel,
    pub approved: bool,
    /// 本次会话的完整授权范围（`--target` / 空间 / 配置三级合并后的结果）。
    ///
    /// `target` 只是"这一次要打哪个目标"，不足以代表"允许打哪些目标"。
    /// 工具需要做二次校验时必须看这个：插件、白名单型工具、需要判断
    /// "这个内网地址是否在授权清单里"的场景都靠它。空 = 未声明（fail-closed）。
    #[serde(default)]
    pub scope: Vec<String>,
}

/// 黑板事实条目：键控读写的最小单元。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactEntry {
    pub entity: String,
    pub attribute: String,
    pub value: serde_json::Value,
    pub confidence: f32,
    pub source_task: Option<TaskId>,
}
