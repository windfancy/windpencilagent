//! 跨 dll 边界的能力表 —— 定义在 [`plugin_abi`]，这里只做转出。
//!
//! **单一真相源**：宿主侧（`plugin-host`）与插件侧（本 SDK）引用的是同一个 crate，
//! 不存在"两份结构体定义漂移"的风险。

pub use plugin_abi::*;

/// 插件清单必须携带的键（宿主解析时校验，缺项即拒绝加载）。
pub const MANIFEST_KEYS: &[&str] = &[
    "abi",
    "id",
    "version",
    "kind",
    "risk",
    "permissions",
    "schema",
    "summary",
];
