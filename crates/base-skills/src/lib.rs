//! 基础能力层技能体系：技能包结构、检索、上下文注入、zip 安装与按需加载。

mod builtin;
mod inject;
mod manager;
mod skill;
pub mod tool;

pub use builtin::BUILTIN_SKILLS;
pub use inject::{InjectLevel, SkillInjector};
pub use manager::{SkillEntry, SkillManager, SkillManagerError};
pub use skill::{Skill, SkillError, SkillIndex, SkillManifest};
pub use tool::SkillLoadTool;
