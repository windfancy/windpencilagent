//! 共享运行时：树通过它访问基座已建好的资源（网络、存储、事件），
//! 从而**不自己造第二套**（避免重复建 HTTP client / 存储句柄）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;

/// HTTP 请求（简化模型：够用即可，不暴露基座内部类型）。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Some(body.into()),
            timeout_ms: None,
        }
    }

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }
}

/// HTTP 响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 基座提供的 HTTP 客户端（连接池/超时/代理统一由基座管理）。
#[async_trait]
pub trait HttpClient: Send + Sync + 'static {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String>;
}

/// 群组运行期句柄：由基座在调用工具时注入。
#[async_trait]
pub trait GroupRuntime: Send + Sync + 'static {
    fn session_id(&self) -> &str;

    /// 共享 HTTP 客户端（**不要**在群组里新建 reqwest Client）。
    fn http(&self) -> Arc<dyn HttpClient>;

    /// 发出审计/观察事件（统一进事件总线，纳入证据链）。
    fn emit(&self, event: &str, payload: Value);

    /// 本次会话的数据目录（插件落盘/结果缓存用）。
    fn data_dir(&self) -> Option<PathBuf> {
        None
    }

    /// 当前工作区根（空间 cwd）：插件/树包做文件落点时的基准目录。
    /// None 表示未绑定工作区，调用方应回落 `data_dir()` 或程序目录。
    fn workspace_cwd(&self) -> Option<PathBuf> {
        None
    }

    /// 共享键值存储读取（命名空间隔离；默认不支持）。
    async fn kv_get(&self, _ns: &str, _key: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }

    /// 共享键值存储写入（默认不支持）。
    async fn kv_put(&self, _ns: &str, _key: &str, _value: Value) -> Result<(), String> {
        Ok(())
    }

    /// 当前时间（毫秒，由基座统一时钟）。
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders() {
        let r = HttpRequest::get("http://x").header("A", "1").timeout_ms(500);
        assert_eq!(r.method, "GET");
        assert_eq!(r.headers.len(), 1);
        assert_eq!(r.timeout_ms, Some(500));
        let p = HttpRequest::post("http://x", "body");
        assert_eq!(p.method, "POST");
        assert_eq!(p.body.as_deref(), Some("body"));
    }

    #[test]
    fn response_success_range() {
        let ok = HttpResponse { status: 204, headers: vec![], body: String::new() };
        assert!(ok.is_success());
        let bad = HttpResponse { status: 500, headers: vec![], body: String::new() };
        assert!(!bad.is_success());
    }
}
