//! # knowledge-core —— 基座知识原语
//!
//! 一个**通用**的文件式知识提供者：树包把知识写成 jsonl/json/md 就是可检索知识库，
//! 不需要每棵树各造一套存储。
//!
//! 它属于基座而不是树：知识"存储/调取/学习"的机制是基础设施，
//! 具体知识内容才是业务（在 `trees/<id>/knowledge/*` 里）。

mod loader;
mod provider;

pub use loader::{
    experience_to_entry, load_files, ConstraintDecl, ConstraintFile, KnowledgeEntry, Loaded,
};
pub use provider::FileKnowledgeProvider;
