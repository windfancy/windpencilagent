//! 基础能力层存储：本地键值持久化（sled）、经验库、任务持久化与审计日志落盘。

mod audit;
mod experience;
pub mod goal;
pub mod sessions;
pub mod spill;
mod store;
pub mod tasks;

pub use audit::AuditLog;
pub use experience::{
    ExperienceEntry, ExperienceStore, KIND_DEAD_END, KIND_SUMMARY, KIND_TARGET, KIND_TECHNIQUE,
};
pub use goal::{
    GoalState, GoalStore, GOAL_AWAITING_ENDORSEMENT, GOAL_BLOCKED, GOAL_COMPLETED,
    GOAL_IN_PROGRESS,
};
pub use sessions::{SessionRecord, SessionState, SessionStore, StoredMessage};
pub use spill::SpillStore;
pub use store::{Store, StoreError};
pub use tasks::{BoardCounts, TaskRecord, TaskStore};
