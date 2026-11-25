//! MCP stdio 客户端：spawn 子进程 + newline-delimited JSON-RPC 2.0。
//!
//! 崩溃隔离设计：server 进程死亡只令当前请求失败（ServerDown），
//! 不影响主进程；下次调用由上层 manager 惰性重启。
//!
//! IO 抽象为 boxed AsyncRead/AsyncWrite，测试用 tokio duplex 无需真实进程。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::proto::{self, McpToolDef};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// 客户端错误。
#[derive(Debug, Error)]
pub enum McpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("MCP server process is down")]
    ServerDown,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("rpc error: {0}")]
    Rpc(String),
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// 请求关联核心（读取任务与请求方共享）。
struct ClientCore {
    pending: PendingMap,
    next_id: AtomicI64,
    alive: AtomicBool,
    request_timeout: Duration,
}

/// 一个已连接的 MCP server 客户端。
pub struct McpClient {
    /// 写端用 tokio Mutex 包一层：`&self` 即可写入（请求天然串行），同时补足 Sync
    stdin: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    core: Arc<ClientCore>,
    child: Option<Child>,
    reader: JoinHandle<()>,
}

impl McpClient {
    /// 从任意 IO 对构造（测试用 duplex；生产用子进程管道）。
    pub fn from_io(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        child: Option<Child>,
    ) -> Self {
        Self::from_io_with_timeout(reader, writer, child, Duration::from_secs(30))
    }

    pub fn from_io_with_timeout(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        child: Option<Child>,
        request_timeout: Duration,
    ) -> Self {
        let core = Arc::new(ClientCore {
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicI64::new(1),
            alive: AtomicBool::new(true),
            request_timeout,
        });
        let core_for_reader = core.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let Some((id, result)) = proto::parse_response(&line) else {
                    // server 主动通知/请求：当前实现忽略（日志留痕）
                    tracing::debug!(raw = %line, "MCP server notification");
                    continue;
                };
                let sender = core_for_reader.pending.lock().unwrap().remove(&id);
                if let Some(tx) = sender {
                    let _ = tx.send(result);
                }
            }
            // EOF / 管道断裂退出循环：标记死亡并清空所有在途请求
            core_for_reader.alive.store(false, Ordering::SeqCst);
            let mut pending = core_for_reader.pending.lock().unwrap();
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("server process exited".into()));
            }
        });
        Self { stdin: tokio::sync::Mutex::new(writer), core, child, reader: reader_task }
    }

    /// spawn MCP server 子进程并握手。
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &serde_json::Map<String, Value>,
    ) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in env {
            if let Some(vs) = v.as_str() {
                cmd.env(k, vs);
            }
        }
        let mut child = cmd.spawn()?;
        let stdout: ChildStdout = child.stdout.take().expect("stdout piped");
        let stdin: ChildStdin = child.stdin.take().expect("stdin piped");
        // stderr 排水（server 日志进 tracing，不阻塞管道）
        if let Some(err) = child.stderr.take() {
            tokio::spawn(async move {
                let mut err = BufReader::new(err);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match err.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => tracing::debug!(target: "mcp_stderr", "{}", String::from_utf8_lossy(&buf).trim()),
                    }
                }
            });
        }
        let client = Self::from_io(Box::new(stdout), Box::new(stdin), Some(child));
        client.initialize().await?;
        Ok(client)
    }

    /// initialize 握手 + initialized 通知。
    pub async fn initialize(&self) -> Result<Value, McpError> {
        let result = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "windpencilbot", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        self.notify("notifications/initialized", Value::Object(Default::default())).await;
        Ok(result)
    }

    /// 发送请求并等待响应（超时保护）。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        if !self.core.alive.load(Ordering::SeqCst) {
            return Err(McpError::ServerDown);
        }
        let id = self.core.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.core.pending.lock().unwrap().insert(id, tx);

        let line = format!("{}\n", proto::request(id, method, params));
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                self.core.alive.store(false, Ordering::SeqCst);
                return Err(McpError::Io(e));
            }
            if let Err(e) = stdin.flush().await {
                self.core.alive.store(false, Ordering::SeqCst);
                return Err(McpError::Io(e));
            }
        }

        match timeout(self.core.request_timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(msg))) => Err(McpError::Rpc(msg)),
            Ok(Err(_recv)) => Err(McpError::ServerDown), // 读取任务退出=进程死亡
            Err(_elapsed) => {
                self.core.pending.lock().unwrap().remove(&id);
                Err(McpError::Timeout(self.core.request_timeout))
            }
        }
    }

    /// 发送通知（不等待响应，失败静默——通知语义）。
    pub async fn notify(&self, method: &str, params: Value) {
        let line = format!("{}\n", proto::notification(method, params));
        let mut stdin = self.stdin.lock().await;
        let _ = stdin.write_all(line.as_bytes()).await;
        let _ = stdin.flush().await;
    }

    /// 拉取工具清单。
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        Ok(proto::extract_tools(&result))
    }

    /// 调用工具，返回 `(ok, 文本回显, 全量 JSON)`。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<(bool, String, Value), McpError> {
        let result = self
            .request("tools/call", serde_json::json!({"name": name, "arguments": arguments}))
            .await?;
        let (ok, text) = proto::extract_call_result(&result);
        Ok((ok, text, result))
    }

    /// 进程是否仍存活。
    pub fn is_alive(&self) -> bool {
        self.core.alive.load(Ordering::SeqCst)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // 终止子进程（崩溃隔离：不让 server 生命周期绑住主进程）
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        self.reader.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    fn split_duplex() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(64 * 1024)
    }

    #[tokio::test]
    async fn handshake_list_call_over_duplex() {
        // A：client 读端 ← server 写端；C：client 写端 → server 读端
        let (client_reader, server_write) = split_duplex();
        let (client_write, server_read) = split_duplex();

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            let mut r = BufReader::new(server_read);
            let mut w = server_write;
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                seen2.lock().unwrap().push(line.trim().to_string());
                let v: Value = serde_json::from_str(line.trim()).unwrap();
                let resp = if v.get("method").and_then(Value::as_str) == Some("initialize") {
                    serde_json::json!({
                        "jsonrpc":"2.0","id":v["id"].as_i64().unwrap(),
                        "result":{"protocolVersion":PROTOCOL_VERSION,"capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}
                    })
                    .to_string()
                } else if v.get("method").and_then(Value::as_str) == Some("tools/list") {
                    serde_json::json!({
                        "jsonrpc":"2.0","id":v["id"].as_i64().unwrap(),
                        "result":{"tools":[{"name":"scan","description":"d","inputSchema":{"type":"object","properties":{}}}]}
                    })
                    .to_string()
                } else if v.get("method").and_then(Value::as_str) == Some("tools/call") {
                    serde_json::json!({
                        "jsonrpc":"2.0","id":v["id"].as_i64().unwrap(),
                        "result":{"content":[{"type":"text","text":"done"}],"isError":false}
                    })
                    .to_string()
                } else {
                    continue; // initialized 通知：无响应
                };
                use tokio::io::AsyncWriteExt as _;
                w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
                w.flush().await.unwrap();
            }
        });

        let client = McpClient::from_io(Box::new(client_reader), Box::new(client_write), None);
        client.initialize().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "scan");
        let (ok, text, _detail) = client.call_tool("scan", serde_json::json!({"x":1})).await.unwrap();
        assert!(ok);
        assert_eq!(text, "done");
        // 请求顺序：initialize → initialized 通知 → tools/list → tools/call
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(seen[1].contains("notifications/initialized"));
    }

    #[tokio::test]
    async fn server_down_fails_pending_requests() {
        // A：client 读端 ← server 写端（立即关闭=EOF）；C：client 写端 → server 读端（立即关闭=写失败）
        let (client_reader, server_write) = split_duplex();
        let (client_write, _server_read) = split_duplex();
        let client = McpClient::from_io(Box::new(client_reader), Box::new(client_write), None);
        drop(server_write);
        drop(_server_read);
        // 等读取任务发现 EOF
        tokio::time::sleep(Duration::from_millis(50)).await;
        let r = client.request("tools/list", serde_json::json!({})).await;
        assert!(matches!(r, Err(McpError::ServerDown)));
    }
}
