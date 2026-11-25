//! # wpa-kit —— 插件与宿主共用的纯工具库
//!
//! 只依赖 `std`。两边都要用的东西放这里，避免"插件一份、宿主一份"漂移：
//!
//! | 模块 | 内容 | 谁用 |
//! |---|---|---|
//! | [`codec`] | base64 / hex / URL 编解码 | 插件（payload、JWT） |
//! | [`digest`] | SHA-256 / MD5 / CRC32 / Murmur3 | 插件（证据哈希、favicon） |
//! | [`textx`] | URL 拆解、HTML 实体、UUID/时间归一化 | 两边 |
//! | [`html`] | 标签/表单/链接/脚本解析 | 两边（宿主 `html_parse`、插件本地判定） |
//! | [`shape`] | 响应差分与相似度 | 两边（插件判定、宿主 `response_diff` 基座版） |
//!
//! **刻意不含正则引擎**：`regex` 约 900KB，只有少数插件真需要，
//! 让它们自己引，别让每个插件都背。

pub mod codec;
pub mod digest;
pub mod html;
pub mod shape;
pub mod textx;
