//! MCP 工具适配器：把远程 MCP 工具适配为本框架 `Tool` trait，注册进 registry
//! 后对模型呈现为 `mcp_<server>_<tool>`（复用 R1-1 参数级风险契约）。

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_core::tools::{ResultKind, Tool, ToolResult};
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::Value;

use crate::manager::McpManager;
use crate::proto::McpToolDef;

/// 回显给 LLM 的文本上限（超长部分经 spill/截断链路处理）。
const SUMMARY_LIMIT: usize = 12_000;

/// 单个 MCP 工具的适配器。
pub struct McpToolAdapter {
    mgr: Arc<McpManager>,
    server: String,
    tool: McpToolDef,
    risk: RiskLevel,
    /// 注册名（构造时固定，`name()` 直接返回）
    registered: String,
}

impl McpToolAdapter {
    /// 构造适配器（server 侧工具目录中取一条）。
    pub fn new(mgr: Arc<McpManager>, server: &str, tool: McpToolDef, risk: RiskLevel) -> Self {
        Self {
            registered: Self::registered_name(server, &tool.name),
            mgr,
            server: server.to_string(),
            tool,
            risk,
        }
    }

    /// 注册名：`mcp_<server>_<tool>`（工具名不符合 [a-z0-9_-] 的字符折叠为 `_`）。
    pub fn registered_name(server: &str, tool: &str) -> String {
        let fold = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        format!("mcp_{}_{}", fold(server), fold(tool))
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.registered
    }

    fn description(&self) -> String {
        format!(
            "[MCP:{}（外部 server，进程崩溃自动重连）] {}",
            self.server,
            if self.tool.description.is_empty() { "（无描述）" } else { &self.tool.description }
        )
    }

    fn input_schema(&self) -> Value {
        if self.tool.input_schema.is_object() {
            self.tool.input_schema.clone()
        } else {
            serde_json::json!({"type": "object", "properties": {}})
        }
    }

    fn risk(&self) -> RiskLevel {
        self.risk
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        // 崩溃隔离：ensure 惰性重连；两次尝试（首次可能踩中刚死亡进程）
        let mut last_err = String::new();
        for attempt in 0..2 {
            let client = match self.mgr.ensure(&self.server).await {
                Ok((c, _)) => c,
                Err(e) => {
                    last_err = format!("MCP server `{}` 不可用: {e}", self.server);
                    break;
                }
            };
            match client.call_tool(&self.tool.name, args.clone()).await {
                Ok((ok, text, detail)) => {
                    let mut summary = text;
                    if summary.len() > SUMMARY_LIMIT {
                        summary.truncate(SUMMARY_LIMIT);
                        summary.push_str("\n[输出过长已截断]");
                    }
                    return ToolResult {
                        ok,
                        kind: if ok { ResultKind::Text } else { ResultKind::Error },
                        summary,
                        detail: Some(detail),
                        duration_ms: ms(),
                    };
                }
                Err(e) => {
                    last_err = format!("{e}");
                    if !matches!(e, crate::client::McpError::ServerDown) {
                        break; // 协议/超时错误不重试
                    }
                    tracing::warn!(attempt, server = %self.server, "MCP 进程中断，重试");
                }
            }
        }
        ToolResult::error(last_err, ms())
    }
}
