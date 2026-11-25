//! 通用智能体执行引擎：对话循环（ReAct + 工具调用）、计划评审门、子智能体。
//!
//! 本 crate 属于 windpencilagent 基座，**不含任何领域角色与领域 prompt**：
//! 专业角色由专业智能树通过 [`ChatConfig::persona`] 与
//! [`ChatConfig::prompt_sections`] 注入（契约见 `wpa-api`）。
//!
//! 注：多角色编排（orchestrator）与领域角色定义（roles）已移出基座，
//! 暂存于 `future-tree/`，由业务逻辑群组接管。

pub mod chat;
pub mod plan_state;
pub mod plan_tool;
pub mod subagent;

pub use chat::{
    estimate_text_tokens, normalize_target, scope_denial, targets_in_args, ApprovalCallback, ToolFilter,
    ChatConfig, ChatEvent, ChatEventCallback, ChatSession, ChatSnapshot, ChatTurn, SkillNote,
    DEFAULT_PERSONA,
};
pub use plan_state::{
    PlanCreateTool, PlanItem, PlanState, PlanStatus, PlanUpdateTool, PlanViewTool, SharedPlan,
};
pub use plan_tool::ExitPlanTool;
pub use subagent::{ProviderCell, SubagentTool};
