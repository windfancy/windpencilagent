//! MCP server 管理器：多 server 注册、惰性启动、崩溃后自动重连、工具目录缓存。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::client::{McpClient, McpError};
use crate::proto::McpToolDef;

/// manager 错误。
#[derive(Debug, Error)]
pub enum McpManagerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("config error: {0}")]
    Config(String),
    #[error("client error: {0}")]
    Client(#[from] McpError),
}

/// 单个 server 的连接配置（mcp.json 或插件声明）。
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct McpServerConfig {
    /// 逻辑名：工具名映射 `mcp_<name>_<tool>`；须为 [a-z0-9_-]
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: serde_json::Map<String, Value>,
    /// 工具风险档位：low|medium|high（默认 medium，未声明的外部工具按需审批）
    #[serde(default)]
    pub risk: Option<String>,
}

/// mcp.json 顶层结构。
#[derive(Debug, Clone, Deserialize)]
pub struct McpConfigFile {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// 运行中的 server 条目。
struct ServerEntry {
    cfg: McpServerConfig,
    state: AsyncMutex<EntryState>,
}

struct EntryState {
    client: Option<Arc<McpClient>>,
    tools: Vec<McpToolDef>,
}

/// MCP 多 server 管理器（进程崩溃隔离：调用失败即惰性重启）。
pub struct McpManager {
    servers: HashMap<String, Arc<ServerEntry>>,
}

impl McpManager {
    /// 从配置文件构造（exe 同级 `mcp.json`：`{"servers":[{name,command,args,env,risk}]}`）。
    pub fn from_config_file(path: impl AsRef<Path>) -> Result<Self, McpManagerError> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        Self::from_config(&raw)
    }

    /// 从配置文本构造。
    pub fn from_config(raw: &str) -> Result<Self, McpManagerError> {
        let cfg: McpConfigFile =
            serde_json::from_str(raw).map_err(|e| McpManagerError::Config(format!("{e}")))?;
        let mut servers = HashMap::new();
        for s in cfg.servers {
            validate_name(&s.name)?;
            servers.insert(s.name.clone(), Arc::new(ServerEntry { cfg: s, state: AsyncMutex::new(EntryState { client: None, tools: Vec::new() }) }));
        }
        Ok(Self { servers })
    }

    /// 空管理器（测试）。
    pub fn empty() -> Self {
        Self { servers: HashMap::new() }
    }

    /// 已配置的 server 名列表。
    pub fn server_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.servers.keys().cloned().collect();
        v.sort();
        v
    }

    /// 取 server 配置。
    pub fn server_config(&self, name: &str) -> Option<McpServerConfig> {
        self.servers.get(name).map(|e| e.cfg.clone())
    }

    /// 确保 server 已连接且工具目录已缓存（死亡自动重连）。
    pub async fn ensure(&self, name: &str) -> Result<(Arc<McpClient>, Vec<McpToolDef>), McpManagerError> {
        let entry = self
            .servers
            .get(name)
            .ok_or_else(|| McpManagerError::Config(format!("unknown server `{name}`")))?;
        let mut state = entry.state.lock().await;
        if let Some(client) = &state.client {
            if client.is_alive() {
                return Ok((client.clone(), state.tools.clone()));
            }
            tracing::warn!(server = %name, "MCP server 进程已死亡，重连中");
        }
        let client = McpClient::spawn(&entry.cfg.command, &entry.cfg.args, &entry.cfg.env).await?;
        let tools = client.list_tools().await?;
        tracing::info!(server = %name, tools = tools.len(), "MCP server 已连接");
        let client = Arc::new(client);
        state.tools = tools.clone();
        state.client = Some(client.clone());
        Ok((client, tools))
    }

    /// 列出某 server 的工具（未连接则返回空）。
    pub async fn tools_of(&self, name: &str) -> Vec<McpToolDef> {
        match self.ensure(name).await {
            Ok((_, tools)) => tools,
            Err(e) => {
                tracing::warn!(server = %name, error = %e, "MCP server 不可用");
                Vec::new()
            }
        }
    }
}

/// 工具名合法性：[a-z0-9_-]，非空，长度 ≤ 32。
fn validate_name(name: &str) -> Result<(), McpManagerError> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(McpManagerError::Config(format!(
            "server name `{name}` 不合法（须为小写字母/数字/-/_，≤32 字符）"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parse() {
        let raw = r#"{"servers":[{"name":"fs","command":"mcp-server-fs","args":["D:/data"],"risk":"high"}]}"#;
        let mgr = McpManager::from_config(raw).unwrap();
        assert_eq!(mgr.server_names(), vec!["fs"]);
        let cfg = mgr.server_config("fs").unwrap();
        assert_eq!(cfg.risk.as_deref(), Some("high"));
        assert_eq!(cfg.args, vec!["D:/data"]);
    }

    #[test]
    fn invalid_server_name_rejected() {
        let raw = r#"{"servers":[{"name":"Bad Name!","command":"x"}]}"#;
        assert!(matches!(McpManager::from_config(raw), Err(McpManagerError::Config(_))));
    }
}
