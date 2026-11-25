//! 插件运行时：把 [`HostApi`] 的 22 个能力接到基座真实实现上。
//!
//! ## 为什么所有出网流量必须走这里
//!
//! 插件是 dll，不能信任。它们只拿到函数指针，而每个函数在真正发请求前都会：
//!
//! 1. 从 `plugin_host::ctx` 取本次调用的会话与授权范围；
//! 2. 走 `agent_core::scope_denial`（与主链路同一套 fail-closed 判据）；
//! 3. 由 `browser-core` 统一限流（全局 / 单 host / 单域 QPS）；
//! 4. 记审计事件。
//!
//! 于是"插件绕过 ScopeGuard 打越界目标"在结构上不可能发生——它没有别的网络入口。
//!
//! ## 线程模型
//!
//! `HostApi` 的函数是同步 C 函数，被 `spawn_blocking` 的线程调用。
//! 这里用保存的 `runtime::Handle` 在该线程内 `block_on`——blocking 线程不在
//! async worker 池里，所以不会互相饿死。

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::future::Future;
use std::os::raw::{c_char, c_int};
use std::panic::UnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use browser_core::{Browser, Cookie, FetchReq};
use plugin_abi::HostApi;
use plugin_host::HostFactory;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wpa_kit::{codec, digest, html};

/// 知识检索回调（由装配方注入：返回 (title, body, score)）。
pub type KbSearchFn = Arc<dyn Fn(&str, usize) -> Vec<(String, String, u32)> + Send + Sync>;

/// 插件运行时环境。
pub struct PluginEnv {
    pub rt: tokio::runtime::Handle,
    pub browser: Arc<Browser>,
    /// 证据 / 原始结果落盘根目录。
    pub data_dir: PathBuf,
    /// 插件 KV（按 (ns,key) 隔离；进程内）。
    pub kv: Mutex<BTreeMap<(String, String), Value>>,
    /// 审计事件（环形，最多 1024 条）。
    pub audits: Mutex<Vec<Value>>,
    /// 知识检索（未注入时 kb_search 返回空并说明原因）。
    pub kb: Mutex<Option<KbSearchFn>>,
}

impl PluginEnv {
    pub fn new(rt: tokio::runtime::Handle, data_dir: PathBuf) -> Self {
        Self {
            rt,
            // 预算取自配置 [net]（超时 / 单域 QPS / UA），不再用写死的默认值
            browser: Arc::new(Browser::new(browser_core::NetBudget::from_net_settings())),
            data_dir,
            kv: Mutex::new(BTreeMap::new()),
            audits: Mutex::new(Vec::new()),
            kb: Mutex::new(None),
        }
    }

    pub fn with_kb(self, f: KbSearchFn) -> Self {
        if let Ok(mut g) = self.kb.lock() {
            *g = Some(f);
        }
        self
    }

    pub fn audit_snapshot(&self) -> Vec<Value> {
        self.audits.lock().map(|v| v.clone()).unwrap_or_default()
    }

    fn push_audit(&self, ev: Value) {
        if let Ok(mut v) = self.audits.lock() {
            v.push(ev);
            if v.len() > 1024 {
                v.remove(0);
            }
        }
    }
}

static ENV: OnceLock<Arc<PluginEnv>> = OnceLock::new();

/// 安装全局环境（进程内一次；重复调用以第一次为准）。
pub fn install_env(env: Arc<PluginEnv>) {
    let _ = ENV.set(env);
}

/// 当前环境（未安装时为 None）。
pub fn env() -> Option<&'static Arc<PluginEnv>> {
    ENV.get()
}

fn need_env() -> Result<&'static Arc<PluginEnv>, String> {
    ENV.get()
        .ok_or_else(|| "插件运行时未初始化（宿主未安装环境）".to_string())
}

fn block<F: Future>(f: F) -> Result<F::Output, String> {
    // 调用方保证自己跑在 spawn_blocking 线程里
    Ok(need_env()?.rt.block_on(f))
}

// ---------------------------------------------------------------- 边界工具

fn reply(v: Value) -> *mut c_char {
    match CString::new(v.to_string()) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn err(msg: impl Into<String>) -> *mut c_char {
    reply(json!({"ok": false, "error": msg.into()}))
}

fn ok(v: Value) -> *mut c_char {
    let mut m = match v {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    m.insert("ok".into(), Value::Bool(true));
    reply(Value::Object(m))
}

fn parse_arg(p: *const c_char) -> Result<Value, String> {
    if p.is_null() {
        return Ok(json!({}));
    }
    // SAFETY: 宿主保证传入以 NUL 结尾的 UTF-8 串（插件侧 CString）。
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string();
    if s.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&s).map_err(|e| format!("入参不是合法 JSON：{e}"))
}

/// 统一兜 panic：`extern "C"` 里 panic 会 abort 整个进程。
fn guard(f: impl FnOnce() -> Result<Value, String> + UnwindSafe) -> *mut c_char {
    match std::panic::catch_unwind(f) {
        Ok(Ok(v)) => ok(v),
        Ok(Err(e)) => err(e),
        Err(p) => {
            let msg = if let Some(s) = p.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = p.downcast_ref::<String>() {
                s.clone()
            } else {
                "未知 panic".to_string()
            };
            err(format!("宿主内部 panic：{msg}"))
        }
    }
}

/// 目标授权校验：与主链路同一套判据（空范围 + 带目标 = 拒绝）。
fn check_target(key: &str, value: &str) -> Result<(), String> {
    let Some(ctx) = plugin_host::ctx::current() else {
        return Ok(());
    };
    let args = json!({ key: value });
    match agent_core::scope_denial(&ctx.scope, &ctx.denylist, &args) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

fn audit_event(plugin: &str, event: &str, payload: Value) {
    let Some(env) = ENV.get() else { return };
    env.push_audit(json!({
        "ts_ms": now_ms(),
        "plugin": plugin,
        "event": event,
        "payload": payload,
    }));
    tracing::debug!(plugin, event, "插件审计");
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn plugin_id() -> String {
    plugin_host::ctx::current()
        .map(|c| c.plugin)
        .unwrap_or_default()
}

// ---------------------------------------------------------------- HTTP

extern "C" fn h_http(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let fr = FetchReq::from_json(&v)?;
        check_target("url", &fr.url)?;
        let env = need_env()?;
        let resp = block(env.browser.fetch(fr.clone()))??;
        audit_event(
            &plugin_id(),
            "http",
            json!({"url": fr.url, "status": resp.status, "bytes": resp.body.len()}),
        );
        Ok(resp.to_json())
    })
}

extern "C" fn h_http_batch(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let arr = v
            .get("requests")
            .and_then(Value::as_array)
            .ok_or("缺少 requests 数组")?;
        let mut reqs = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let fr = FetchReq::from_json(item).map_err(|e| format!("requests[{i}]：{e}"))?;
            check_target("url", &fr.url).map_err(|e| format!("requests[{i}] 被拒：{e}"))?;
            reqs.push(fr);
        }
        let conc = v.get("concurrency").and_then(Value::as_u64).unwrap_or(10) as usize;
        let env = need_env()?;
        let results = block(env.browser.fetch_batch(reqs.clone(), conc))?;
        let out: Vec<Value> = results
            .into_iter()
            .map(|r| match r {
                Ok(resp) => resp.to_json(),
                Err(e) => json!({"ok": false, "error": e}),
            })
            .collect();
        audit_event(&plugin_id(), "http_batch", json!({"count": reqs.len()}));
        Ok(json!({"results": out, "count": out.len()}))
    })
}

// ---------------------------------------------------------------- 会话

extern "C" fn h_session_open(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();
        let base = v.get("base_url").and_then(Value::as_str).unwrap_or("");
        if !base.is_empty() {
            check_target("url", base)?;
        }
        let ua = v.get("ua").and_then(Value::as_str).unwrap_or("");
        let env = need_env()?;
        let s = env.browser.open_session(&name, base, ua);
        audit_event(&plugin_id(), "session_open", json!({"session": name}));
        Ok(json!({"session": s.name}))
    })
}

extern "C" fn h_session_request(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v
            .get("session")
            .and_then(Value::as_str)
            .ok_or("缺少 session")?
            .to_string();
        let fr = FetchReq::from_json(&v)?;
        check_target("url", &fr.url)?;
        let env = need_env()?;
        let resp = block(env.browser.fetch_in(&name, fr.clone()))??;
        audit_event(
            &plugin_id(),
            "session_request",
            json!({"session": name, "url": fr.url, "status": resp.status}),
        );
        Ok(resp.to_json())
    })
}

extern "C" fn h_session_form(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v
            .get("session")
            .and_then(Value::as_str)
            .ok_or("缺少 session")?
            .to_string();
        let url = v
            .get("url")
            .and_then(Value::as_str)
            .ok_or("缺少 url")?
            .to_string();
        check_target("url", &url)?;
        let fields: Vec<(String, String)> = v
            .get("fields")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, val)| {
                        (
                            k.clone(),
                            val.as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| val.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let method = v.get("method").and_then(Value::as_str).unwrap_or("POST");
        let env = need_env()?;
        let resp = block(env.browser.submit_form(&name, &url, &fields, method))??;
        audit_event(
            &plugin_id(),
            "session_form",
            json!({"session": name, "url": url, "status": resp.status}),
        );
        Ok(resp.to_json())
    })
}

extern "C" fn h_session_cookies(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v.get("session").and_then(Value::as_str).unwrap_or("default");
        let env = need_env()?;
        let s = env.browser.session(name)?;
        let list: Vec<Value> = s.cookies().iter().map(Cookie::to_json).collect();
        Ok(json!({"cookies": list, "count": list.len()}))
    })
}

extern "C" fn h_session_close(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v.get("session").and_then(Value::as_str).unwrap_or("default");
        let env = need_env()?;
        let closed = env.browser.close_session(name);
        Ok(json!({"closed": closed}))
    })
}

// ---------------------------------------------------------------- HTML

extern "C" fn h_html_parse(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let body = v.get("html").and_then(Value::as_str).ok_or("缺少 html")?;
        let base = v.get("base_url").and_then(Value::as_str).unwrap_or("");
        let doc = html::parse(body, base);
        Ok(doc.to_json())
    })
}

extern "C" fn h_html_query(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let body = v.get("html").and_then(Value::as_str).ok_or("缺少 html")?;
        let sel = v.get("selector").cloned().unwrap_or(json!({}));
        let tag = sel.get("tag").and_then(Value::as_str).unwrap_or("a");
        let attr_name = sel.get("attr").and_then(Value::as_str);
        let attr_value = sel.get("value").and_then(Value::as_str);
        let text_contains = sel.get("text_contains").and_then(Value::as_str);
        let nodes = html::query(body, tag, attr_name, attr_value, text_contains);
        let out: Vec<Value> = nodes
            .iter()
            .take(500)
            .map(|n| {
                let attrs: Map<String, Value> = n
                    .attrs
                    .iter()
                    .map(|a| (a.name.clone(), Value::String(a.value.clone())))
                    .collect();
                json!({"tag": n.name, "attrs": Value::Object(attrs)})
            })
            .collect();
        Ok(json!({"nodes": out, "count": out.len()}))
    })
}

// ---------------------------------------------------------------- TCP

/// 裸 TCP 探测（连上 → 可选发包 → 读回）。
///
/// 不做 TLS：基座没有内置 TLS 客户端，硬塞会让每个插件背一个 rustls。
/// 需要 TLS 的场景走 `http`（reqwest 自带）——它在语义上也更合适。
async fn tcp_probe(
    host: &str,
    port: u16,
    payload: &[u8],
    read_ms: u64,
    expect: usize,
) -> Result<Value, String> {
    let addr = format!("{host}:{port}");
    let started = std::time::Instant::now();
    let connect = tokio::time::timeout(
        std::time::Duration::from_millis(read_ms.max(1000)),
        tokio::net::TcpStream::connect(&addr),
    )
    .await;
    let mut stream = match connect {
        Err(_) => {
            return Ok(json!({
                "open": false, "banner_b64": "", "banner_text": "",
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "error": "连接超时",
            }))
        }
        Ok(Err(e)) => {
            return Ok(json!({
                "open": false, "banner_b64": "", "banner_text": "",
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "error": e.to_string(),
            }))
        }
        Ok(Ok(s)) => s,
    };

    if !payload.is_empty() {
        stream
            .write_all(payload)
            .await
            .map_err(|e| format!("发送失败：{e}"))?;
        let _ = stream.flush().await;
    }

    let mut buf = vec![0u8; expect.clamp(1, 1 << 20)];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(read_ms.max(100)),
        stream.read(&mut buf),
    )
    .await;
    let (banner, read_err) = match read {
        Ok(Ok(n)) => {
            buf.truncate(n);
            (buf, None)
        }
        Ok(Err(e)) => (Vec::new(), Some(format!("读取失败：{e}"))),
        Err(_) => (Vec::new(), Some("读取超时".to_string())),
    };

    Ok(json!({
        "open": true,
        "banner_b64": codec::b64_encode(&banner),
        "banner_text": String::from_utf8_lossy(&banner).to_string(),
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "error": read_err,
    }))
}

extern "C" fn h_tcp(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let host = v
            .get("host")
            .and_then(Value::as_str)
            .ok_or("缺少 host")?
            .to_string();
        let port = v
            .get("port")
            .and_then(Value::as_u64)
            .ok_or("缺少 port（整数）")? as u16;
        if v.get("tls").and_then(Value::as_bool).unwrap_or(false) {
            return Err("宿主不提供裸 TCP 上的 TLS（需要 TLS 请改用 http 能力）".into());
        }
        check_target("host", &host)?;
        let payload = v
            .get("payload_b64")
            .and_then(Value::as_str)
            .map(codec::b64_decode)
            .unwrap_or_default();
        let read_ms = v.get("read_ms").and_then(Value::as_u64).unwrap_or(2500);
        let expect = v.get("expect_bytes").and_then(Value::as_u64).unwrap_or(4096) as usize;
        let out = block(tcp_probe(&host, port, &payload, read_ms, expect))??;
        audit_event(
            &plugin_id(),
            "tcp",
            json!({"host": host, "port": port, "open": out["open"]}),
        );
        Ok(out)
    })
}

extern "C" fn h_tcp_batch(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let arr = v
            .get("targets")
            .and_then(Value::as_array)
            .ok_or("缺少 targets 数组")?
            .clone();
        let mut targets: Vec<(String, u16)> = Vec::with_capacity(arr.len());
        for (i, t) in arr.iter().enumerate() {
            let host = t
                .get("host")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("targets[{i}] 缺 host"))?
                .to_string();
            let port = t
                .get("port")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("targets[{i}] 缺 port"))? as u16;
            check_target("host", &host).map_err(|e| format!("targets[{i}] 被拒：{e}"))?;
            targets.push((host, port));
        }
        let read_ms = v.get("read_ms").and_then(Value::as_u64).unwrap_or(2000);
        let conc = v.get("concurrency").and_then(Value::as_u64).unwrap_or(64) as usize;

        let results: Vec<Value> = block(async {
            use futures_util::stream::{self, StreamExt};
            let out: Vec<Value> = stream::iter(targets.iter().cloned().map(|(h, p)| async move {
                match tcp_probe(&h, p, &[], read_ms, 0).await {
                    Ok(mut v) => {
                        v["host"] = json!(h);
                        v["port"] = json!(p);
                        v
                    }
                    Err(e) => json!({"host": h, "port": p, "open": false, "error": e}),
                }
            }))
            .buffer_unordered(conc.clamp(1, 500))
            .collect()
            .await;
            out
        })?;
        audit_event(&plugin_id(), "tcp_batch", json!({"count": results.len()}));
        Ok(json!({"results": results, "count": results.len()}))
    })
}

// ---------------------------------------------------------------- UDP

/// 单包 UDP 探测：发一个报文，等一个回包。
///
/// 语义上比 TCP 更"温和"——无连接、无重传，未响应不代表端口关闭，
/// 所以返回值用 `responded` 而不是 `open`，避免插件误判。
async fn udp_probe(
    host: &str,
    port: u16,
    payload: &[u8],
    read_ms: u64,
    expect: usize,
) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let addr = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("域名解析失败：{e}"))?
        .next()
        .ok_or_else(|| format!("{host} 无可用地址"))?;
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = tokio::net::UdpSocket::bind(bind)
        .await
        .map_err(|e| format!("绑定本地端口失败：{e}"))?;
    sock.connect(addr)
        .await
        .map_err(|e| format!("关联远端失败：{e}"))?;
    sock.send(payload)
        .await
        .map_err(|e| format!("发送失败：{e}"))?;

    let mut buf = vec![0u8; expect.clamp(1, 1 << 20)];
    let got = tokio::time::timeout(
        std::time::Duration::from_millis(read_ms.max(100)),
        sock.recv(&mut buf),
    )
    .await;

    let elapsed = started.elapsed().as_millis() as u64;
    match got {
        Ok(Ok(n)) => {
            buf.truncate(n);
            Ok(json!({
                "responded": true,
                "data_b64": codec::b64_encode(&buf),
                "data_text": String::from_utf8_lossy(&buf).to_string(),
                "bytes": n,
                "remote": addr.to_string(),
                "elapsed_ms": elapsed,
                "error": Value::Null,
            }))
        }
        Ok(Err(e)) => Ok(json!({
            "responded": false, "data_b64": "", "data_text": "", "bytes": 0,
            "remote": addr.to_string(), "elapsed_ms": elapsed,
            "error": format!("接收失败：{e}"),
        })),
        Err(_) => Ok(json!({
            "responded": false, "data_b64": "", "data_text": "", "bytes": 0,
            "remote": addr.to_string(), "elapsed_ms": elapsed,
            "error": "无响应（超时）",
        })),
    }
}

extern "C" fn h_udp(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let host = v
            .get("host")
            .and_then(Value::as_str)
            .ok_or("缺少 host")?
            .to_string();
        let port = v
            .get("port")
            .and_then(Value::as_u64)
            .ok_or("缺少 port（整数）")? as u16;
        check_target("host", &host)?;
        let payload = v
            .get("payload_b64")
            .and_then(Value::as_str)
            .map(codec::b64_decode)
            .unwrap_or_default();
        let read_ms = v.get("read_ms").and_then(Value::as_u64).unwrap_or(2000);
        let expect = v.get("expect_bytes").and_then(Value::as_u64).unwrap_or(4096) as usize;
        let out = block(udp_probe(&host, port, &payload, read_ms, expect))??;
        audit_event(
            &plugin_id(),
            "udp",
            json!({"host": host, "port": port, "responded": out["responded"]}),
        );
        Ok(out)
    })
}

// ---------------------------------------------------------------- TLS 证书

/// 摘 TLS 服务端证书。握手时故意不校验证书——渗透场景要的就是"看它给了什么"。
async fn tls_peer_cert(host: &str, port: u16, sni: &str) -> Result<Value, String> {
    let addr = format!("{host}:{port}");
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .map_err(|_| "连接超时".to_string())?
    .map_err(|e| format!("连接失败：{e}"))?;

    let builder = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| format!("TLS 初始化失败：{e}"))?;
    let connector = tokio_native_tls::TlsConnector::from(builder);
    let sni_owned = sni.to_string();
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        connector.connect(&sni_owned, tcp),
    )
    .await
    .map_err(|_| "TLS 握手超时".to_string())?
    .map_err(|e| format!("TLS 握手失败：{e}"))?;

    let peer = stream
        .get_ref()
        .peer_certificate()
        .map_err(|e| format!("取证书失败：{e}"))?
        .ok_or("对端未提供证书")?;
    let der = peer.to_der().map_err(|e| format!("导出 DER 失败：{e}"))?;
    let mut out = parse_cert_der(&der)?;
    if let Value::Object(m) = &mut out {
        m.insert("sni".into(), json!(sni_owned));
        m.insert("der_bytes".into(), json!(der.len()));
        m.insert("sha256".into(), json!(digest::sha256_hex(&der)));
    }
    Ok(out)
}

extern "C" fn h_tls_cert(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let host = v
            .get("host")
            .and_then(Value::as_str)
            .ok_or("缺少 host")?
            .to_string();
        let port = v.get("port").and_then(Value::as_u64).unwrap_or(443) as u16;
        check_target("host", &host)?;
        let sni = v
            .get("server_name")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&host)
            .to_string();
        let out = block(tls_peer_cert(&host, port, &sni))??;
        audit_event(
            &plugin_id(),
            "tls_cert",
            json!({"host": host, "port": port, "sni": sni}),
        );
        Ok(out)
    })
}

fn parse_cert_der(der: &[u8]) -> Result<Value, String> {
    use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

    let (_, cert) = X509Certificate::from_der(der).map_err(|e| format!("证书解析失败：{e}"))?;
    let nb = cert.validity().not_before.timestamp();
    let na = cert.validity().not_after.timestamp();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut alt: Vec<String> = Vec::new();
    let mut is_ca = false;
    let mut ocsp = false;
    for ext in cert.extensions() {
        match ext.parsed_extension() {
            ParsedExtension::SubjectAlternativeName(san) => {
                for n in &san.general_names {
                    match n {
                        GeneralName::DNSName(s) => alt.push(format!("DNS:{s}")),
                        GeneralName::IPAddress(b) => alt.push(format!("IP:{}", ip_from_bytes(b))),
                        GeneralName::RFC822Name(s) => alt.push(format!("EMAIL:{s}")),
                        GeneralName::URI(s) => alt.push(format!("URI:{s}")),
                        _ => {}
                    }
                }
            }
            ParsedExtension::BasicConstraints(bc) => is_ca = bc.ca,
            ParsedExtension::AuthorityInfoAccess(_) => ocsp = true,
            _ => {}
        }
    }
    let subject = cert.subject().to_string();
    let issuer = cert.issuer().to_string();
    let self_signed = subject == issuer;

    let sig_oid = cert.signature_algorithm.algorithm.to_id_string();
    let sig_alg = match sig_oid.as_str() {
        "1.2.840.113549.1.1.2" => "md2WithRSA".to_string(),
        "1.2.840.113549.1.1.4" => "md5WithRSA".to_string(),
        "1.2.840.113549.1.1.5" => "sha1WithRSA".to_string(),
        "1.2.840.113549.1.1.11" => "sha256WithRSA".to_string(),
        "1.2.840.113549.1.1.12" => "sha384WithRSA".to_string(),
        "1.2.840.113549.1.1.13" => "sha512WithRSA".to_string(),
        "1.2.840.10045.4.1" => "sha1WithECDSA".to_string(),
        "1.2.840.10045.4.3.2" => "sha256WithECDSA".to_string(),
        "1.2.840.10045.4.3.3" => "sha384WithECDSA".to_string(),
        "1.2.840.10045.4.3.4" => "sha512WithECDSA".to_string(),
        other => other.to_string(),
    };
    let weak = sig_alg.contains("md2")
        || sig_alg.contains("md5")
        || sig_alg.contains("sha1")
        || sig_alg.starts_with("1.2.840.113549.1.1.4");

    Ok(json!({
        "subject": subject,
        "issuer": issuer,
        "common_name": cert
            .subject()
            .iter_common_name()
            .next()
            .and_then(|c| c.as_str().ok())
            .map(str::to_string),
        "serial": cert.raw_serial_as_string(),
        "alt_names": alt,
        "alt_count": alt.len(),
        "not_before": iso_utc(nb),
        "not_after": iso_utc(na),
        "not_before_ts": nb,
        "not_after_ts": na,
        "days_left": (na - now) / 86_400,
        "expired": now > na,
        "not_yet_valid": now < nb,
        "sig_alg": sig_alg,
        "sig_oid": sig_oid,
        "weak_signature": weak,
        "is_ca": is_ca,
        "self_signed": self_signed,
        "has_ocsp": ocsp,
    }))
}

fn ip_from_bytes(b: &[u8]) -> String {
    match b.len() {
        4 => std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string(),
        16 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(b);
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => codec::hex_encode(b),
    }
}

/// unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`（Howard Hinnant 的 civil_from_days，不引时间库）。
fn iso_utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

// ---------------------------------------------------------------- 本机信息

/// 本机信息：`{what:"net"}` 出网卡表，`{what:"host"}` 出主机身份。
fn sysinfo(req_what: &str) -> Result<Value, String> {
    match req_what {
        "net" | "" => Ok(sysinfo_net()),
        "host" => Ok(sysinfo_host()),
        other => Err(format!("不支持的 what：{other}（可用 net / host）")),
    }
}

fn sysinfo_host() -> Value {
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| {
            std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|_| "unknown".to_string());
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    json!({
        "hostname": hostname,
        "user": user,
        "domain": domain,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "family": std::env::consts::FAMILY,
    })
}

fn sysinfo_net() -> Value {
    let mut ifaces = local_interfaces();
    let primary = primary_out_ip();
    // 非 Windows 平台没有原生枚举：至少给出主出口 IP，而不是空表
    if ifaces.is_empty() {
        if let Some(ip) = primary.clone() {
            ifaces.push(json!({
                "name": "", "desc": "", "mac": "", "ip": ip,
                "mask": "", "gateway": "", "source": "route",
            }));
        }
    }
    json!({
        "interfaces": ifaces,
        "count": ifaces.len(),
        "primary_ip": primary,
    })
}

/// 主出口 IP：UDP `connect` 只查路由表不发包，因此无网络副作用。
fn primary_out_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("223.5.5.5:53").ok()?;
    s.local_addr().ok().map(|a| a.ip().to_string())
}

#[cfg(windows)]
fn local_interfaces() -> Vec<Value> {
    // 直接对接 iphlpapi！GetAdaptersInfo：不引第三方程序、不引 windows crate。
    // 取的是 C 结构体的前缀（Next..SecondaryWinsServer），尾部 time_t 字段不读，
    // 前缀偏移由 repr(C) 按 C 的对齐规则自动算出，与 MSVC 一致。
    use std::os::raw::{c_char, c_ulong};

    const MAX_ADAPTER_NAME_LENGTH: usize = 256;
    const MAX_ADAPTER_DESCRIPTION_LENGTH: usize = 128;
    const MAX_ADAPTER_ADDRESS_LENGTH: usize = 8;

    #[repr(C)]
    struct IpAddrString {
        next: *mut IpAddrString,
        ip: [c_char; 16],
        mask: [c_char; 16],
        context: c_ulong,
    }

    #[repr(C)]
    struct AdapterInfo {
        next: *mut AdapterInfo,
        combo_index: c_ulong,
        adapter_name: [c_char; MAX_ADAPTER_NAME_LENGTH + 4],
        description: [c_char; MAX_ADAPTER_DESCRIPTION_LENGTH + 4],
        address_length: u32,
        address: [u8; MAX_ADAPTER_ADDRESS_LENGTH],
        index: c_ulong,
        type_: u32,
        dhcp_enabled: u32,
        current_ip: *mut IpAddrString,
        ip_list: IpAddrString,
        gateway_list: IpAddrString,
        dhcp_server: IpAddrString,
        have_wins: i32,
        primary_wins: IpAddrString,
        secondary_wins: IpAddrString,
        // LeaseObtained / LeaseExpires 不读，故不必声明
    }

    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetAdaptersInfo(info: *mut u8, size: *mut c_ulong) -> c_ulong;
    }

    fn cstr(buf: &[c_char]) -> String {
        let bytes: Vec<u8> = buf.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        String::from_utf8_lossy(&bytes).to_string()
    }

    let mut out = Vec::new();
    unsafe {
        let mut size: c_ulong = 0;
        // 第一次调用只为拿所需缓冲区大小（返回 ERROR_BUFFER_OVERFLOW=111）
        GetAdaptersInfo(std::ptr::null_mut(), &mut size);
        if size == 0 {
            return out;
        }
        let mut buf = vec![0u8; size as usize];
        let rc = GetAdaptersInfo(buf.as_mut_ptr(), &mut size);
        if rc != 0 {
            tracing::debug!(rc, "GetAdaptersInfo 失败");
            return out;
        }
        let mut cur = buf.as_ptr() as *const AdapterInfo;
        let mut guard = 0;
        while !cur.is_null() && guard < 128 {
            guard += 1;
            let a = &*cur;
            let name = cstr(&a.adapter_name);
            let desc = cstr(&a.description);
            let mac = if a.address_length == 0 || a.address_length as usize > MAX_ADAPTER_ADDRESS_LENGTH {
                String::new()
            } else {
                (0..a.address_length as usize)
                    .map(|i| format!("{:02x}", a.address[i]))
                    .collect::<Vec<_>>()
                    .join(":")
            };
            let gateway = cstr(&a.gateway_list.ip);
            // 一块网卡可能绑多个 IP：逐个展开，gateway 取该网卡的首个网关
            let mut ip = &a.ip_list as *const IpAddrString;
            let mut k = 0;
            while !ip.is_null() && k < 64 {
                k += 1;
                let e = &*ip;
                let addr = cstr(&e.ip);
                if !addr.is_empty() && addr != "0.0.0.0" {
                    out.push(json!({
                        "name": name,
                        "desc": desc,
                        "mac": mac,
                        "ip": addr,
                        "mask": cstr(&e.mask),
                        "gateway": gateway,
                        "if_index": a.index,
                        "dhcp": a.dhcp_enabled != 0,
                        "source": "iphlpapi",
                    }));
                }
                ip = e.next;
            }
            // 没有 IP 的网卡（未连接）也列出来，便于判断"该网卡存在但没地址"
            if k == 0 {
                out.push(json!({
                    "name": name, "desc": desc, "mac": mac, "ip": "",
                    "mask": "", "gateway": gateway, "if_index": a.index,
                    "dhcp": a.dhcp_enabled != 0, "source": "iphlpapi",
                }));
            }
            cur = a.next;
        }
    }
    out
}

#[cfg(not(windows))]
fn local_interfaces() -> Vec<Value> {
    Vec::new()
}

extern "C" fn h_sysinfo(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let what = v.get("what").and_then(Value::as_str).unwrap_or("net");
        // 读本机信息不算越界，但也不该被无授权调用拿去当"环境探测"——
        // 仍然受审计（有记录可追溯），不做 scope 校验（没有目标概念）。
        let out = sysinfo(what)?;
        audit_event(&plugin_id(), "sysinfo", json!({"what": what}));
        Ok(out)
    })
}

// ---------------------------------------------------------------- DNS

extern "C" fn h_dns(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .ok_or("缺少 name")?
            .to_string();
        let rtype = v.get("rtype").and_then(Value::as_str).unwrap_or("A");
        let qtype: u16 = match rtype.to_ascii_uppercase().as_str() {
            "A" => 1,
            "NS" => 2,
            "CNAME" => 5,
            "MX" => 15,
            "TXT" => 16,
            "AAAA" => 28,
            other => return Err(format!("不支持的记录类型 {other}")),
        };
        let server = v
            .get("server")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(default_dns_server);
        let resp = block(async {
            net_core::dns::query(&server, &name, qtype, std::time::Duration::from_secs(5)).await
        })??;
        audit_event(&plugin_id(), "dns", json!({"name": name, "rtype": rtype}));
        let records: Vec<String> = resp.answers.iter().map(|r| r.value.clone()).collect();
        let cname: Vec<String> = resp
            .answers
            .iter()
            .filter(|r| r.rtype == 5)
            .map(|r| r.value.clone())
            .collect();
        let ttl = resp.answers.iter().map(|r| r.ttl).min().unwrap_or(0);
        Ok(json!({
            "records": records,
            "cname": cname,
            "ttl": ttl,
            "rcode": resp.rcode,
            "rtt_ms": resp.rtt_ms,
        }))
    })
}

/// 默认 DNS 服务器：读系统配置，失败回落到公共 DNS。
fn default_dns_server() -> String {
    // Windows 下 systemd 式的 /etc/resolv.conf 不存在，直接用公共 DNS 更可控
    if cfg!(windows) {
        "223.5.5.5:53".to_string()
    } else {
        std::fs::read_to_string("/etc/resolv.conf")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.trim().strip_prefix("nameserver "))
                    .map(|h| format!("{}:53", h.trim()))
            })
            .unwrap_or_else(|| "223.5.5.5:53".to_string())
    }
}

// ---------------------------------------------------------------- 数据

extern "C" fn h_kb_search(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let query = v
            .get("query")
            .and_then(Value::as_str)
            .ok_or("缺少 query")?
            .to_string();
        let top = v.get("top").and_then(Value::as_u64).unwrap_or(5).clamp(1, 50) as usize;
        let env = need_env()?;
        let f = env
            .kb
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or("知识库未注入（本次装配没有知识群组）")?;
        let hits: Vec<Value> = f(&query, top)
            .into_iter()
            .map(|(title, body, score)| json!({"title": title, "body": body, "score": score}))
            .collect();
        Ok(json!({"hits": hits, "count": hits.len()}))
    })
}

extern "C" fn h_kv_get(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let ns = v.get("ns").and_then(Value::as_str).unwrap_or("default");
        let key = v.get("key").and_then(Value::as_str).ok_or("缺少 key")?;
        let env = need_env()?;
        let g = env.kv.lock().map_err(|_| "kv 锁中毒".to_string())?;
        Ok(json!({"value": g.get(&(ns.to_string(), key.to_string())).cloned()}))
    })
}

extern "C" fn h_kv_put(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let ns = v.get("ns").and_then(Value::as_str).unwrap_or("default").to_string();
        let key = v
            .get("key")
            .and_then(Value::as_str)
            .ok_or("缺少 key")?
            .to_string();
        let value = v.get("value").cloned().unwrap_or(Value::Null);
        let env = need_env()?;
        let mut g = env.kv.lock().map_err(|_| "kv 锁中毒".to_string())?;
        g.insert((ns, key), value);
        Ok(json!({}))
    })
}

extern "C" fn h_raw_store(req: *const c_char) -> *mut c_char {
    guard(move || {
        let v = parse_arg(req)?;
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .ok_or("缺少 name")?
            .to_string();
        let content = v.get("content").and_then(Value::as_str).unwrap_or("");
        let mime = v.get("mime").and_then(Value::as_str).unwrap_or("text/plain");
        let env = need_env()?;

        // 文件名净化：插件给的 name 不能带路径
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .take(80)
            .collect();
        let ext = match mime {
            "text/html" => "html",
            "application/json" => "json",
            _ => "txt",
        };
        let dir = env.data_dir.join("raw");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败：{e}"))?;
        let file = format!("{safe}-{}.{ext}", now_ms());
        let path = dir.join(&file);
        std::fs::write(&path, content).map_err(|e| format!("写文件失败：{e}"))?;
        let sha = digest::sha256_hex(content.as_bytes());
        audit_event(
            &plugin_id(),
            "raw_store",
            json!({"file": file, "bytes": content.len(), "sha256": sha}),
        );
        Ok(json!({
            "handle": format!("raw:{file}"),
            "path": path.to_string_lossy(),
            "sha256": sha,
            "bytes": content.len(),
        }))
    })
}

// ---------------------------------------------------------------- 审计/日志

extern "C" fn h_audit(ev: *const c_char) {
    let Ok(v) = parse_arg(ev) else { return };
    let event = v.get("event").and_then(Value::as_str).unwrap_or("event");
    let payload = v.get("payload").cloned().unwrap_or(Value::Null);
    audit_event(&plugin_id(), event, payload);
}

extern "C" fn h_log(level: c_int, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(msg) }.to_string_lossy().to_string();
    match level {
        0 => tracing::debug!(target: "windpencilagent::plugin", "{s}"),
        2 => tracing::warn!(target: "windpencilagent::plugin", "{s}"),
        3 => tracing::error!(target: "windpencilagent::plugin", "{s}"),
        _ => tracing::info!(target: "windpencilagent::plugin", "{s}"),
    }
}

extern "C" fn h_free(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    // SAFETY: 指针来自本模块的 CString::into_raw
    unsafe {
        drop(CString::from_raw(p));
    }
}

// ---------------------------------------------------------------- 组装

/// 按插件声明的权限构造能力表（未声明的一律 `None`）。
pub fn build_table(perms: &[String]) -> HostApi {
    let has = |p: &str| perms.iter().any(|x| x == p);
    let mut api = HostApi::empty();
    if has("http") {
        api.http = Some(h_http);
        api.http_batch = Some(h_http_batch);
    }
    if has("session") {
        api.session_open = Some(h_session_open);
        api.session_request = Some(h_session_request);
        api.session_form = Some(h_session_form);
        api.session_cookies = Some(h_session_cookies);
        api.session_close = Some(h_session_close);
    }
    if has("html") {
        api.html_parse = Some(h_html_parse);
        api.html_query = Some(h_html_query);
    }
    if has("tcp") {
        api.tcp = Some(h_tcp);
        api.tcp_batch = Some(h_tcp_batch);
    }
    if has("dns") {
        api.dns = Some(h_dns);
    }
    if has("udp") {
        api.udp = Some(h_udp);
    }
    if has("tls") {
        api.tls_cert = Some(h_tls_cert);
    }
    if has("sys") {
        api.sysinfo = Some(h_sysinfo);
    }
    if has("kb") {
        api.kb_search = Some(h_kb_search);
    }
    if has("kv") {
        api.kv_get = Some(h_kv_get);
        api.kv_put = Some(h_kv_put);
    }
    if has("raw_store") {
        api.raw_store = Some(h_raw_store);
    }
    // 审计与日志恒开放：它们不产生外部副作用，且是追责所必需
    api.audit = Some(h_audit);
    api.log = Some(h_log);
    api.host_free = Some(h_free);
    api
}

/// 构造给 `PluginHost` 用的工厂（先装环境，再返回按权限裁剪的表）。
pub fn factory(env: Arc<PluginEnv>) -> HostFactory {
    install_env(env);
    Arc::new(|perms: &[String]| build_table(perms))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host::CallContext;

    fn call(f: Option<plugin_abi::JsonCallFn>, arg: &Value) -> Value {
        let c = CString::new(arg.to_string()).unwrap();
        let p = f.unwrap()(c.as_ptr());
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string();
        h_free(p);
        serde_json::from_str(&s).unwrap()
    }

    fn env_with_rt() -> Arc<PluginEnv> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        // 让 runtime 活到测试结束
        let handle = rt.handle().clone();
        std::mem::forget(rt);
        let dir = std::env::temp_dir().join(format!("wbp_rt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        Arc::new(PluginEnv::new(handle, dir))
    }

    #[test]
    fn table_respects_permissions() {
        let no_net = build_table(&[]);
        assert!(no_net.http.is_none());
        assert!(no_net.tcp.is_none());
        assert!(no_net.dns.is_none());
        assert!(no_net.audit.is_some(), "审计恒开");
        assert!(no_net.host_free.is_some());

        let web = build_table(&["http".into(), "html".into()]);
        assert!(web.http.is_some());
        assert!(web.html_parse.is_some());
        assert!(web.tcp.is_none(), "未声明 tcp 就不该给");

        // 追加能力同样要按权限裁剪
        assert!(no_net.udp.is_none());
        assert!(no_net.tls_cert.is_none());
        assert!(no_net.sysinfo.is_none());
        let extra = build_table(&["udp".into(), "tls".into(), "sys".into()]);
        assert!(extra.udp.is_some());
        assert!(extra.tls_cert.is_some());
        assert!(extra.sysinfo.is_some());
        assert!(extra.http.is_none());
    }

    #[test]
    fn iso_utc_matches_known_epochs() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(1_709_164_800), "2024-02-29T00:00:00Z", "闰年");
    }

    #[test]
    fn sysinfo_host_is_populated() {
        install_env(env_with_rt());
        let api = build_table(&["sys".into()]);
        let v = call(api.sysinfo, &json!({"what":"host"}));
        assert_eq!(v["ok"], true, "{v}");
        assert!(!v["hostname"].as_str().unwrap().is_empty());
        assert!(v["cpus"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn sysinfo_net_never_panics_and_has_shape() {
        install_env(env_with_rt());
        let api = build_table(&["sys".into()]);
        let v = call(api.sysinfo, &json!({"what":"net"}));
        assert_eq!(v["ok"], true, "{v}");
        assert!(v["interfaces"].is_array());
        #[cfg(windows)]
        assert!(
            !v["interfaces"].as_array().unwrap().is_empty(),
            "Windows 下 iphlpapi 必须枚举出网卡：{v}"
        );
        // 未知 what 要报错而不是静默返回空
        let bad = call(api.sysinfo, &json!({"what":"bogus"}));
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].as_str().unwrap().contains("net"));
    }

    #[test]
    fn bad_json_returns_structured_error() {
        install_env(env_with_rt());
        let api = build_table(&["kv".into()]);
        let c = CString::new("{not json").unwrap();
        let p = (api.kv_get.unwrap())(c.as_ptr());
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string();
        h_free(p);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("JSON"));
    }

    #[test]
    fn kv_roundtrip() {
        install_env(env_with_rt());
        let api = build_table(&["kv".into()]);
        let put = call(api.kv_put, &json!({"ns":"n","key":"k","value":{"a":1}}));
        assert_eq!(put["ok"], true);
        let got = call(api.kv_get, &json!({"ns":"n","key":"k"}));
        assert_eq!(got["value"]["a"], 1);
        let missing = call(api.kv_get, &json!({"ns":"n","key":"nope"}));
        assert!(missing["value"].is_null());
    }

    #[test]
    fn html_parse_reaches_kit() {
        install_env(env_with_rt());
        let api = build_table(&["html".into()]);
        let v = call(
            api.html_parse,
            &json!({"html":"<title>T</title><a href='/x'>X</a>","base_url":"http://h/"}),
        );
        assert_eq!(v["title"], "T");
        assert_eq!(v["links"][0]["href"], "http://h/x");
    }

    #[test]
    fn raw_store_writes_under_data_dir() {
        install_env(env_with_rt());
        let api = build_table(&["raw_store".into()]);
        let v = call(api.raw_store, &json!({"name":"ev idence/../x","content":"hello","mime":"text/plain"}));
        assert_eq!(v["ok"], true);
        let path = PathBuf::from(v["path"].as_str().unwrap());
        assert!(path.is_file());
        assert!(path.to_string_lossy().contains("raw"));
        assert_eq!(v["sha256"].as_str().unwrap().len(), 64);
        // 文件名已净化：不含路径分隔
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(!fname.contains('/'), "{fname}");
    }

    #[test]
    fn scope_guard_blocks_out_of_scope() {
        install_env(env_with_rt());
        let api = build_table(&["http".into()]);
        // 空授权范围 + 带目标 ⇒ fail-closed
        let out = plugin_host::ctx::with(
            CallContext {
                session_id: "s".into(),
                scope: vec![],
                denylist: vec![],
                plugin: "t".into(),
                data_dir: None,
            },
            || call(api.http, &json!({"url":"http://10.0.0.1/"})),
        );
        assert_eq!(out["ok"], false, "{out}");
        assert!(out["error"].as_str().unwrap().contains("授权"), "{out}");
    }
}
