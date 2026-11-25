//! MCP stdio 客户端（R3-2）：自研 JSON-RPC over stdio，零第三方协议依赖。
//!
//! - [`client::McpClient`]：spawn server 子进程 + newline-delimited JSON-RPC；
//! - [`manager::McpManager`]：多 server 管理、惰性启动、崩溃自动重连；
//! - [`tool::McpToolAdapter`]：远程工具适配为本框架 Tool（`mcp_<server>_<tool>`）。

pub mod client;
pub mod manager;
pub mod proto;
pub mod tool;

pub use client::{McpClient, McpError};
pub use manager::{McpConfigFile, McpManager, McpManagerError, McpServerConfig};
pub use proto::McpToolDef;
pub use tool::McpToolAdapter;

/// 按风险字符串解析档位（mcp.json 的 `risk` 字段）。
pub fn parse_risk(s: &str) -> kern_core::model::RiskLevel {
    use kern_core::model::RiskLevel;
    match s.to_ascii_lowercase().as_str() {
        "none" => RiskLevel::None,
        "low" => RiskLevel::Low,
        "high" => RiskLevel::High,
        "critical" => RiskLevel::Critical,
        // 默认 medium：未声明风险的外部工具按需审批红线之下、可见性之上
        _ => RiskLevel::Medium,
    }
}

#[cfg(test)]
mod tests {
    use kern_core::model::RiskLevel;

    #[test]
    fn risk_parsing_defaults_to_medium() {
        assert!(matches!(super::parse_risk("high"), RiskLevel::High));
        assert!(matches!(super::parse_risk("LOW"), RiskLevel::Low));
        assert!(matches!(super::parse_risk(""), RiskLevel::Medium));
        assert!(matches!(super::parse_risk("unknown"), RiskLevel::Medium));
    }

    #[test]
    fn registered_name_folds_illegal_chars() {
        assert_eq!(
            crate::tool::McpToolAdapter::registered_name("fs", "read_file"),
            "mcp_fs_read_file"
        );
        assert_eq!(
            crate::tool::McpToolAdapter::registered_name("web", "search.web"),
            "mcp_web_search_web"
        );
    }
}
