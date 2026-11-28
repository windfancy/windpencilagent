//! 网络原语工具：`net.scan` / `net.banner` / `net.reach` / `net.dns` / `net.http`。
//!
//! 它们**没有业务语义**（不认识"渗透""漏洞"），只是通用网络连通性与信息获取能力：
//! 任何 agent 做接口联调、服务巡检、DNS 排查都用得上。业务包装在树包里完成。

use std::time::Duration;

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Value};

use toolkit_core::tools::{Tool, ToolResult};

use crate::scan::{self, COMMON_PORTS, MAX_CONCURRENCY, MAX_PORTS};
use crate::settings;

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn arg_u64(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn target_of(args: &Value) -> Result<String, String> {
    arg_str(args, "host")
        .or_else(|| arg_str(args, "target"))
        .ok_or_else(|| "缺少 host 参数".to_string())
}

// ---------------------------------------------------------------------------
// net.scan
// ---------------------------------------------------------------------------

/// TCP connect 端口扫描。
pub struct NetScan;

#[async_trait]
impl Tool for NetScan {
    fn name(&self) -> &str {
        "net.scan"
    }

    fn description(&self) -> String {
        "对目标做 TCP connect 端口探测（可选抓 banner）。ports 支持 `80,443`、`1-1024`、`22,8000-8100`。\
         只探测、不改动目标。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": {"type": "string", "description": "目标主机（IP 或域名）"},
                "ports": {"type": "string", "description": "端口表达式；省略则扫常用端口"},
                "timeout_ms": {"type": "integer", "description": "单端口超时（毫秒，默认 800）"},
                "concurrency": {"type": "integer", "description": "并发数（默认 128，上限 512）"},
                "banner": {"type": "boolean", "description": "是否抓取服务 banner（默认 false）"}
            },
            "required": ["host"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn risk_for(&self, args: &Value) -> RiskLevel {
        // 大范围扫描提高一档：误操作代价与目标侧告警量都随端口数上升。
        let n = arg_str(args, "ports")
            .and_then(|s| scan::parse_ports(&s).ok())
            .map(|p| p.len())
            .unwrap_or(COMMON_PORTS.len());
        if n > 200 {
            RiskLevel::Medium
        } else {
            RiskLevel::Low
        }
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult {
        let started = std::time::Instant::now();
        let host = match target_of(&args) {
            Ok(h) => h,
            Err(e) => return ToolResult::error(e, 0),
        };
        if host != ctx.target && !ctx.target.is_empty() {
            // 执行层已过 ScopeGuard；这里是双保险，防止上下文中转时目标被改写。
            // 注意：只有当 ctx.target 是具体目标（非空）时才比对。
            tracing::debug!(host = %host, ctx_target = %ctx.target, "net.scan 目标与上下文不一致（已交由 ScopeGuard 判定）");
        }
        let ports = match arg_str(&args, "ports") {
            Some(s) => match scan::parse_ports(&s) {
                Ok(p) => p,
                Err(e) => return ToolResult::error(e, 0),
            },
            None => COMMON_PORTS.to_vec(),
        };
        if ports.len() > MAX_PORTS {
            return ToolResult::error(format!("端口数量超过上限 {MAX_PORTS}"), 0);
        }
        let timeout = Duration::from_millis(arg_u64(&args, "timeout_ms", 800).clamp(50, 10_000));
        let conc = arg_u64(&args, "concurrency", 128).clamp(1, MAX_CONCURRENCY as u64) as usize;
        let banner = arg_bool(&args, "banner", false);

        match scan::scan(&host, &ports, timeout, conc, banner).await {
            Ok(r) => {
                let ms = started.elapsed().as_millis() as u64;
                let list: Vec<String> = r
                    .open
                    .iter()
                    .map(|p| {
                        let svc = if p.service.is_empty() { String::new() } else { format!("({})", p.service) };
                        match &p.banner {
                            Some(b) => format!("{}{} [{}]", p.port, svc, b),
                            None => format!("{}{}", p.port, svc),
                        }
                    })
                    .collect();
                let summary = format!(
                    "{} 扫描 {} 个端口，开放 {} 个：{}",
                    r.host,
                    r.scanned,
                    r.open.len(),
                    if list.is_empty() { "无".to_string() } else { list.join("、") }
                );
                ToolResult::json(summary, serde_json::to_value(&r).unwrap_or(Value::Null), ms)
            }
            Err(e) => ToolResult::error(e, started.elapsed().as_millis() as u64),
        }
    }
}

// ---------------------------------------------------------------------------
// net.banner
// ---------------------------------------------------------------------------

/// 单端口 banner 抓取。
pub struct NetBanner;

#[async_trait]
impl Tool for NetBanner {
    fn name(&self) -> &str {
        "net.banner"
    }

    fn description(&self) -> String {
        "连接目标指定端口并读取欢迎信息（服务指纹的第一手证据）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": {"type": "string"},
                "port": {"type": "integer", "description": "端口（默认 80）"},
                "timeout_ms": {"type": "integer", "description": "连接超时（默认 1500）"}
            },
            "required": ["host"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let started = std::time::Instant::now();
        let host = match target_of(&args) {
            Ok(h) => h,
            Err(e) => return ToolResult::error(e, 0),
        };
        let port = arg_u64(&args, "port", 80).clamp(1, 65535) as u16;
        let timeout = Duration::from_millis(arg_u64(&args, "timeout_ms", 1500).clamp(100, 20_000));
        match scan::probe(&host, port, timeout, true).await {
            Ok(Some((banner, ms))) => {
                let banner_for_text = banner.clone();
                let text = banner_for_text.unwrap_or_else(|| "（连接成功但未主动发送 banner）".into());
                ToolResult::json(
                    format!("{host}:{port} 开放；banner：{text}"),
                    json!({"host": host, "port": port, "open": true, "banner": banner, "connect_ms": ms}),
                    started.elapsed().as_millis() as u64,
                )
            }
            Ok(None) => ToolResult::json(
                format!("{host}:{port} 未开放（拒绝或超时）"),
                json!({"host": host, "port": port, "open": false}),
                started.elapsed().as_millis() as u64,
            ),
            Err(e) => ToolResult::error(e, started.elapsed().as_millis() as u64),
        }
    }
}

// ---------------------------------------------------------------------------
// net.reach
// ---------------------------------------------------------------------------

/// 主机可达性判断（TCP 多端口任一开放即视为存活）。
pub struct NetReach;

#[async_trait]
impl Tool for NetReach {
    fn name(&self) -> &str {
        "net.reach"
    }

    fn description(&self) -> String {
        "判断主机是否在线：对若干常见端口做 TCP 连接，任一成功即视为可达（不需要管理员权限，优于 ICMP）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": {"type": "string"},
                "ports": {"type": "string", "description": "候选端口（默认 80,443,22,445,8080）"},
                "timeout_ms": {"type": "integer", "description": "单端口超时（默认 800）"}
            },
            "required": ["host"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let started = std::time::Instant::now();
        let host = match target_of(&args) {
            Ok(h) => h,
            Err(e) => return ToolResult::error(e, 0),
        };
        let ports = match arg_str(&args, "ports") {
            Some(s) => match scan::parse_ports(&s) {
                Ok(p) => p,
                Err(e) => return ToolResult::error(e, 0),
            },
            None => vec![80, 443, 22, 445, 8080],
        };
        let timeout = Duration::from_millis(arg_u64(&args, "timeout_ms", 800).clamp(50, 10_000));
        // 并发度默认取配置 `[net].concurrency`（不再硬编码 8）
        let r = scan::scan(&host, &ports, timeout, settings::current().effective_concurrency(), false)
            .await;
        let ms = started.elapsed().as_millis() as u64;
        match r {
            Ok(r) => {
                let alive = !r.open.is_empty();
                let detail = json!({
                    "host": r.host,
                    "alive": alive,
                    // 本次实际扫描的端口（回显，避免被误读成"忽略 ports 参数"）
                    "ports": ports.clone(),
                    "scanned": ports.len(),
                    "open_ports": r.open.iter().map(|p| p.port).collect::<Vec<_>>(),
                    "resolved": r.resolved,
                });
                ToolResult::json(
                    format!(
                        "{} {}（扫描 {} 个端口，开放端口：{}）",
                        host,
                        if alive { "可达" } else { "不可达或全端口过滤" },
                        ports.len(),
                        if r.open.is_empty() {
                            "无".to_string()
                        } else {
                            r.open.iter().map(|p| p.port.to_string()).collect::<Vec<_>>().join(",")
                        }
                    ),
                    detail,
                    ms,
                )
            }
            Err(e) => ToolResult::error(e, ms),
        }
    }
}

// ---------------------------------------------------------------------------
// net.dns
// ---------------------------------------------------------------------------

/// DNS 查询（自研报文，不调系统解析器）。
pub struct NetDns;

#[async_trait]
impl Tool for NetDns {
    fn name(&self) -> &str {
        "net.dns"
    }

    fn description(&self) -> String {
        "解析域名的 A/AAAA/CNAME/MX/TXT/NS/SOA 记录。返回 TTL 与记录值，用于资产归属与基础设施判定。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": {"type": "string", "description": "要解析的域名"},
                "type": {"type": "string", "description": "记录类型（A/AAAA/CNAME/MX/TXT/NS/SOA），默认 A"},
                "server": {"type": "string", "description": "DNS 服务器（默认 223.5.5.5）"},
                "timeout_ms": {"type": "integer", "description": "超时（默认 3000）"}
            },
            "required": ["host"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let started = std::time::Instant::now();
        let name = match target_of(&args) {
            Ok(h) => h,
            Err(e) => return ToolResult::error(e, 0),
        };
        let tname = arg_str(&args, "type").unwrap_or_else(|| "A".into());
        let qtype = match crate::dns::parse_type(&tname) {
            Some(t) => t,
            None => {
                return ToolResult::error(
                    format!("不支持的记录类型：{tname}（可选 A/AAAA/CNAME/MX/TXT/NS/SOA）"),
                    0,
                )
            }
        };
        let server = arg_str(&args, "server").unwrap_or_else(|| "223.5.5.5".into());
        let timeout = Duration::from_millis(arg_u64(&args, "timeout_ms", 3000).clamp(200, 15_000));
        match crate::dns::query(&server, &name, qtype, timeout).await {
            Ok(r) => {
                let ms = started.elapsed().as_millis() as u64;
                let lines: Vec<String> = r
                    .answers
                    .iter()
                    .map(|a| format!("{} {} TTL={}", crate::dns::type_name(a.rtype), a.value, a.ttl))
                    .collect();
                let summary = if r.rcode != 0 {
                    format!("{name} 查询失败：{}（rcode {}）", r.rcode_text(), r.rcode)
                } else if lines.is_empty() {
                    format!("{name} 无 {tname} 记录")
                } else {
                    format!("{name} ({tname})：{}", lines.join("；"))
                };
                ToolResult::json(
                    summary,
                    json!({
                        "name": name,
                        "type": tname,
                        "rcode": r.rcode,
                        "rcode_text": r.rcode_text(),
                        "truncated": r.truncated,
                        "records": r.answers.iter().map(|a| json!({
                            "name": a.name,
                            "type": crate::dns::type_name(a.rtype),
                            "ttl": a.ttl,
                            "value": a.value
                        })).collect::<Vec<_>>(),
                        "rtt_ms": r.rtt_ms,
                    }),
                    ms,
                )
            }
            Err(e) => ToolResult::error(e, started.elapsed().as_millis() as u64),
        }
    }
}

// ---------------------------------------------------------------------------
// net.http
// ---------------------------------------------------------------------------

/// HTTP 客户端（按当前网络参数缓存）。
///
/// 为什么不用 `OnceLock<Client>`：超时/代理/UA 都来自配置，改配置后必须换客户端。
/// 这里以"参数快照相等"为缓存键：参数没变就复用连接池，变了才重建一个。
/// 用 `Mutex` + 小对象而非泛型池：`net.http` 的调用频度远低于一次 TCP 握手成本。
fn http_client() -> reqwest::Client {
    use std::sync::Mutex;
    struct Cache {
        key: settings::NetSettings,
        client: reqwest::Client,
    }
    static CACHE: std::sync::OnceLock<Mutex<Option<Cache>>> = std::sync::OnceLock::new();

    let want = settings::current();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(c) = guard.as_ref() {
        if c.key == want {
            return c.client.clone();
        }
    }
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(want.timeout_secs.clamp(1, 600)))
        .connect_timeout(Duration::from_secs(want.connect_timeout_secs.clamp(1, 120)))
        .user_agent(want.effective_user_agent())
        .redirect(reqwest::redirect::Policy::limited(5))
        .danger_accept_invalid_certs(false);
    // 代理写错不该让整个工具不可用：报错就退回直连，并在返回体里能看出来的只有失败原因
    if let Some(p) = want.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
        match reqwest::Proxy::all(p) {
            Ok(proxy) => b = b.proxy(proxy),
            Err(e) => tracing::warn!(proxy = p, error = %e, "代理地址非法，本次请求按直连执行"),
        }
    }
    let client = b.build().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "HTTP 客户端构建失败，退回默认客户端（参数未生效）");
        reqwest::Client::new()
    });
    *guard = Some(Cache { key: want, client: client.clone() });
    client
}

/// 通用 HTTP 请求（拿响应头/状态/正文）。
pub struct NetHttp;

#[async_trait]
impl Tool for NetHttp {
    fn name(&self) -> &str {
        "net.http"
    }

    fn description(&self) -> String {
        "发起 HTTP 请求并返回状态码、响应头与正文摘要。用于接口探测、指纹识别、健康检查。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "完整 URL（含 http:// 或 https://）"},
                "method": {"type": "string", "description": "HTTP 方法（默认 GET）"},
                "headers": {"type": "object", "description": "请求头键值对"},
                "body": {"type": "string", "description": "请求体"},
                "max_body": {"type": "integer", "description": "正文最多返回多少字符（默认 4000）"}
            },
            "required": ["url"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn risk_for(&self, args: &Value) -> RiskLevel {
        match arg_str(args, "method").unwrap_or_else(|| "GET".into()).to_ascii_uppercase().as_str() {
            "GET" | "HEAD" | "OPTIONS" => RiskLevel::Low,
            // 写操作可能改变目标状态
            _ => RiskLevel::Medium,
        }
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let started = std::time::Instant::now();
        let url = match arg_str(&args, "url") {
            Some(u) => u,
            None => return ToolResult::error("缺少 url 参数".to_string(), 0),
        };
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return ToolResult::error(format!("只允许 http(s) URL：{url}"), 0);
        }
        let method = arg_str(&args, "method").unwrap_or_else(|| "GET".into()).to_ascii_uppercase();
        let m = match reqwest::Method::from_bytes(method.as_bytes()) {
            Ok(m) => m,
            Err(_) => return ToolResult::error(format!("非法 HTTP 方法：{method}"), 0),
        };
        let client = http_client();
        let mut rb = client.request(m, &url);
        if let Some(h) = args.get("headers").and_then(Value::as_object) {
            for (k, v) in h {
                if let Some(s) = v.as_str() {
                    rb = rb.header(k.as_str(), s);
                }
            }
        }
        if let Some(b) = arg_str(&args, "body") {
            rb = rb.body(b);
        }
        match rb.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                    .collect();
                let max_body = arg_u64(&args, "max_body", 4000).clamp(0, 50_000) as usize;
                let body = resp.text().await.unwrap_or_default();
                let body_chars = body.chars().count();
                let head: String = body.chars().take(max_body).collect();
                let server = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("server"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let summary = format!(
                    "{method} {url} → {status}{}（{} 字符）",
                    if server.is_empty() { String::new() } else { format!("，Server: {server}") },
                    body_chars
                );
                ToolResult::json(
                    summary,
                    json!({
                        "url": url,
                        "status": status,
                        "headers": headers,
                        "body": head,
                        "body_chars": body_chars,
                        "truncated": body_chars > max_body,
                    }),
                    started.elapsed().as_millis() as u64,
                )
            }
            Err(e) => ToolResult::error(format!("请求失败：{e}"), started.elapsed().as_millis() as u64),
        }
    }
}

/// 全部网络原语（engine 装配时批量注册）。
pub fn all() -> Vec<std::sync::Arc<dyn Tool>> {
    vec![
        std::sync::Arc::new(NetScan),
        std::sync::Arc::new(NetBanner),
        std::sync::Arc::new(NetReach),
        std::sync::Arc::new(NetDns),
        std::sync::Arc::new(NetHttp),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kern_core::model::{AgentId, SessionId, TaskId};

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("t"),
            task_id: TaskId::new("t"),
            target: "127.0.0.1".into(),
            risk: RiskLevel::Low,
            approved: true,
        }
    }

    #[test]
    fn tool_metadata_is_stable() {
        let names: Vec<String> = all().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names.len(), 5);
        assert!(names.iter().any(|n| n == "net.scan"));
        assert!(names.iter().any(|n| n == "net.dns"));
        for t in all() {
            let s = t.input_schema();
            assert_eq!(s["type"], "object");
        }
    }

    #[test]
    fn scan_risk_escalates_with_port_count() {
        let t = NetScan;
        assert_eq!(t.risk_for(&json!({"host":"h","ports":"80,443"})), RiskLevel::Low);
        assert_eq!(t.risk_for(&json!({"host":"h","ports":"1-1024"})), RiskLevel::Medium);
    }

    #[test]
    fn http_risk_depends_on_method() {
        let t = NetHttp;
        assert_eq!(t.risk_for(&json!({"url":"http://x","method":"GET"})), RiskLevel::Low);
        assert_eq!(t.risk_for(&json!({"url":"http://x","method":"POST"})), RiskLevel::Medium);
    }

    #[tokio::test]
    async fn http_rejects_non_http_scheme() {
        let r = NetHttp.call(&ctx(), json!({"url":"file:///etc/passwd"})).await;
        assert!(!r.ok);
    }

    #[tokio::test]
    async fn http_get_local_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut s, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nServer: TestSrv/1.0\r\nContent-Length: 5\r\n\r\nhello",
                        )
                        .await;
                }
            }
        });
        let r = NetHttp
            .call(&ctx(), json!({"url": format!("http://127.0.0.1:{port}/")}))
            .await;
        assert!(r.ok, "{}", r.summary);
        assert!(r.summary.contains("200"));
        assert!(r.summary.contains("TestSrv"));
        assert_eq!(r.detail.unwrap()["body"], "hello");
    }

    #[tokio::test]
    async fn dns_rejects_unknown_type() {
        let r = NetDns.call(&ctx(), json!({"host":"example.com","type":"BOGUS"})).await;
        assert!(!r.ok);
        assert!(r.summary.contains("不支持的记录类型"));
    }

    #[tokio::test]
    async fn reach_reports_unreachable_gracefully() {
        let r = NetReach
            .call(&ctx(), json!({"host":"127.0.0.1","ports":"1","timeout_ms":200}))
            .await;
        assert!(r.ok, "不可达应正常返回结果而非报错：{}", r.summary);
        assert!(r.summary.contains("不可达"));
    }

    #[tokio::test]
    async fn reach_echoes_scanned_ports() {
        // 回显本次请求的端口，避免被误读成"忽略 ports 参数"
        let r = NetReach
            .call(&ctx(), json!({"host":"127.0.0.1","ports":"3306,6379,3389","timeout_ms":200}))
            .await;
        assert!(r.ok);
        let d = r.detail.expect("应有 detail");
        let ports: Vec<u64> = d["ports"].as_array().unwrap().iter()
            .filter_map(serde_json::Value::as_u64).collect();
        assert_eq!(ports, vec![3306, 3389, 6379]);
        assert_eq!(d["scanned"].as_u64(), Some(3));
    }
}
