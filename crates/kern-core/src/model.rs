//! 通用数据模型：目标 / 任务 / 事实 / 执行上下文与结果。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 会话标识：一次人机对话 / 一次作业的顶层归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
    pub fn from_raw(u: Uuid) -> Self {
        Self(u)
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

/// 任务类型（渗透任务语义分类）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    /// 侦察 / 信息收集（无侵入）
    Recon,
    /// 指纹识别与资产测绘
    Fingerprint,
    /// 漏洞探测（低风险）
    Probe,
    /// 验证 / 利用（高风险，默认需审批）
    Exploit,
    /// 结果加工与报告
    Collect,
    /// 自定义
    Custom(String),
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

/// 任务执行状态（精简七态状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Created,
    Queued,
    Running,
    Paused,
    Blocked,
    Done,
    Failed,
    Abandoned,
}

/// 审批决策（HITL）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalDecision {
    Approved,
    Denied,
    Timeout,
}

/// 用户 / 系统的目标输入（由界面层解析后传入内核层）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Objective {
    pub id: SessionId,
    pub description: String,
    pub authorized_scope: Vec<String>,
    pub constraints: Vec<String>,
    pub ref_plan: Option<String>,
}

/// 单次工具调用的审批请求上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub task_id: TaskId,
    pub tool_name: String,
    pub target: String,
    pub risk: RiskLevel,
    pub args_summary: String,
    pub reason: String,
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

/// 一次执行动作的产出，回写黑板并供反思器消费。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub task_id: TaskId,
    pub ok: bool,
    pub facts: Vec<FactEntry>,
    pub summary: String,
    pub duration_ms: u64,
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

impl FactEntry {
    pub fn simple(entity: impl Into<String>, attribute: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        Self {
            entity: entity.into(),
            attribute: attribute.into(),
            value: value.into(),
            confidence: 1.0,
            source_task: None,
        }
    }
}
