//! # tree-core —— 声明式能力 + 基座约束层
//!
//! 两块分工：
//!
//! - [`pnc`]：`PnC.toml` 清单模型与校验（fail-closed），插件的元数据唯一来源；
//! - [`exec`] / [`group`] / [`slots`]：声明式工具（`[[tool]]` + `exec` 四路执行）、
//!   群组装配、插槽（目标从授权范围取，不让模型猜）；
//! - [`hooks`]：知识约束 → 可执行 guard 谓词（红线），`Must` 级约束必须编译成
//!   能拦人的规则，而不是一句给模型看的话；
//! - [`phases`]：阶段机与工具面收敛。
//!
//! 树包**安装器**（`install.rs`：版本闸 / 指纹 / `trees/registry.toml`）已随
//! 「不再有独立的 trees/ 体系」移除——能力的装卸统一走 `wpa-engine::registry`
//! （`data/registry.toml`）。

pub mod exec;
pub mod group;
pub mod hooks;
pub mod pnc;
pub mod phases;
pub mod slots;

pub use exec::{build_tools, find_python, DeclarativeTool, PrimitiveInvoker};
pub use group::{DeclarativeGroup, DeclarativePlanner};
pub use hooks::{compile_default, ConstraintSet, GuardRule};
pub use pnc::{parse_risk, toml_to_json, ExecSpec, PnCManifest, ToolDecl};
pub use phases::{PhaseSet, PhaseTransition, SharedPhases};
pub use slots::{apply_slots, strip_from_schema, SlotContext};
