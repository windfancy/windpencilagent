//! # wpa-api —— 基座与专业智能树之间的唯一契约
//!
//! ## 为什么存在
//!
//! `windpencilagent`（基座）与 `<领域>-tree`（专业智能树）是两个**独立 workspace**。
//! 树必须能在不依赖基座实现 crate 的前提下编译，因此它只能看见本 crate。
//! 基座侧通过适配器把本契约实现到自身的工具执行链上。
//!
//! ```text
//!   ┌─────────────┐   依赖    ┌──────────────┐   依赖    ┌────────────────┐
//!   │ pentest-tree│ ────────► │ wpa-api  │ ◄──────── │    windpencilagent     │
//!   │  (四类群组)  │           │  （本 crate） │           │   （基座实现）  │
//!   └─────────────┘           └──────────────┘           └────────────────┘
//! ```
//!
//! ## 四类群组（用户定义）
//!
//! | 群组 | 职责 | 本 crate 契约 |
//! |---|---|---|
//! | [`GroupKind::Knowledge`] | 专业知识存储、调取、学习，为智能体提供业务规范与约束 | [`KnowledgeProvider`] |
//! | [`GroupKind::Tools`] | 原子级工具能力 | [`ApiTool`] |
//! | [`GroupKind::Logic`] | 业务决策与流程，编排装配知识与工具 | [`Planner`] |
//! | [`GroupKind::Skills`] | 分领域工作技能（渐进披露） | [`SkillPack`] |
//!
//! ## 依赖方向铁律
//!
//! 1. 本 crate **禁止**依赖任何基座实现 crate（`kern-*` / `base-*` / `toolkit-core` /
//!    `agent-core`），也禁止依赖任何 `feat-*` / `ext-*` 业务 crate；
//! 2. 树只依赖本 crate；基座只依赖本 crate 的**契约**、不依赖任何具体群组；
//! 3. 装配壳（`assembly/*-app`）是唯一同时依赖基座与树的地方。

pub mod group;
pub mod manifest;
pub mod registry;
pub mod runtime;
pub mod version;

pub use group::{
    register_tools, ApiTool, Constraint, ConstraintQuery, ConstraintSeverity, Experience,
    GroupPlugin, KnowledgeGroup, KnowledgeHit, KnowledgeProvider, LogicGroup,
    Phase, Plan, PlanRequest, PlanStep, Planner, SkillGroup, SkillPack, ToolGroup, ToolReply,
    ToolRequest,
};
pub use manifest::{
    GroupKind, GroupManifest, GroupRef, RiskTier, TreeManifest, ValidationReport,
};
pub use registry::{RegisterError, ToolSchemaLite, GroupRegistry};
pub use runtime::{GroupRuntime, HttpClient, HttpRequest, HttpResponse};
pub use version::SemVer;

/// 契约层统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 清单非法（缺字段 / 版本不满足 / 依赖成环）。
    #[error("清单非法：{0}")]
    Manifest(String),
    /// 注册冲突（重名工具 / 重名群组）。
    #[error("注册冲突：{0}")]
    Conflict(String),
    /// 群组声明的能力与实现不符。
    #[error("契约违反：{0}")]
    Contract(String),
}
