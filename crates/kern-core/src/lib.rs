//! PenRustBot 内核层核心 crate。
//!
//! 承载「一切皆插接」体系的统一接口、Registry 注册表、事件总线、
//! Scope Guard 授权范围红线，以及全局通用数据模型。
//!
//! 本 crate 不依赖任何具体实现组件，只定义契约。

mod approval;
mod component;
mod event;

/// 执行契约（ToolExecutor / ToolOutcome）与审批中枢。
pub mod exec;

/// 全局通用数据模型（会话 / 任务 / 目标 / 执行上下文等）。
pub mod model;
mod registry;
mod scope;

pub use approval::{ApprovalHub, PendingApproval};
pub use component::{Component, ComponentKind, ComponentSpec, ComponentStatus};
pub use event::{EventBus, EventKind, KernelEvent};
pub use exec::{ToolExecutor, ToolOutcome};
pub use model::{
    AgentId, ApprovalDecision, ExecutionContext, ExecutionOutcome, FactEntry, Objective, RiskLevel,
    SessionId, TaskId, TaskKind, TaskState,
};
pub use registry::{AssemblyError, Registry, RegistryError};
pub use scope::{ScopeGuard, ScopeRule, ScopeViolation};
