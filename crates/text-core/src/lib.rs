//! # text-core —— 基座文本原语
//!
//! 编码转换、哈希、正则、线索提取、文本变换。这些是**通用**能力：
//! 报告整理、参数构造、证据比对都靠它们；不含任何业务判断。

pub mod codec;
pub mod hash;
pub mod tools;

pub use codec::{decode, encode, SCHEMES};
pub use hash::{hash_all, hash_bytes, ALGOS};
pub use tools::{TextCodec, TextExtract, TextHash, TextRegex, TextTransform};
