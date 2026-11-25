//! # net-core —— 基座网络原语
//!
//! 自研实现，不调用 `nmap` / `curl` / `dig` / `nslookup`（用户铁律：不依赖第三方程序）。
//!
//! 提供的能力是**通用**的：连通性判断、端口探测、banner、DNS 解析、HTTP 请求。
//! 它们不含任何业务判断（不知道"渗透"是什么），业务语义由树包包装。
//!
//! 全局参数（超时 / 代理 / UA / 并发）走 [`settings`]：由装配层从配置 `[net]` 安装，
//! 工具执行期读快照 —— 工具实例由内核统一构造，拿不到配置引用。

pub mod dns;
pub mod scan;
pub mod settings;
pub mod tools;

pub use scan::{parse_ports, probe, scan, service_name, ScanResult, COMMON_PORTS};
pub use settings::NetSettings;
pub use tools::{NetBanner, NetDns, NetHttp, NetReach, NetScan};
