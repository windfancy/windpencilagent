//! windpencilagent 基座引擎：配置、契约宿主、工具装配、会话构建、UI 快照。
//!
//! 本 crate 是 `windpencilagent`（CLI）与 `wpa-desktop`（Slint 桌面）的**共同底座**：
//! 两个前端都只是"驱动引擎"，不含业务判断。
//!
//! 边界：本 crate 不认识任何专业领域词汇。业务能力全部来自
//! `wpa-api` 契约下的群组插件（专业智能树以数据包形式装入 `trees/`）。

pub mod base;
pub mod catalog;
pub mod registry;
pub mod config;
pub mod config_schema;
pub mod context;
pub mod host;
pub mod plugin_rt;
pub mod llm;
pub mod snapshot;
pub mod tools;
pub mod domain;
pub mod embedder;

pub use base::{
    apply_net_settings, assemble, build_session, Base, SessionInfo, SessionOpts,
};
pub use config::{AppConfig, ChangeSet, ConfigSection};
pub use config_schema::{cards, field, fields, Card, ConfigPage, Field, FieldKind};
pub use context::{fit_text, Budget, ContextPlan};
pub use host::{BaseRuntime, TreeReport};
pub use registry::{EntryRecord, GroupRecord, ToolRegistry};
pub use plugin_rt::{factory as plugin_host_factory, PluginEnv};
pub use snapshot::{CapGroupRow, DomainCardRow, ModelRow, SkillRow, Snapshot, SpaceRow, ToolRow};

/// 授权范围/拒绝清单条目的自检提示（UI 保存前、CLI `check` 共用同一套判据）。
///
/// 返回空 = 全部条目可用。非空时：授权条目写错只丢自己，拒绝条目写错会拒绝一切带目标操作。
pub fn scope_notes(authorized: &[String], denylist: &[String]) -> Vec<String> {
    kern_core::ScopeGuard::from_lists_lenient(authorized, denylist).1
}
