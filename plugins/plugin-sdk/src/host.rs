//! 宿主能力封装：插件只看见这几个函数，看不见任何 IO 实现。
//!
//! **铁律**：插件不许自己建线程池、不许自己发网络请求、不许自己写盘。
//! 所有 IO 都经过这里 → 宿主 → ScopeGuard / 限流 / 审计，一条路。

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::RwLock;

use serde_json::{json, Value};

use crate::api::{HostApi, JsonCallFn};
use crate::codec;

static HOST: RwLock<Option<HostApi>> = RwLock::new(None);

/// 注入宿主能力表。返回 0 = 就绪，负数为错误码（-1 空指针 / -2 版本不符 / -3 结构过短）。
///
/// **可重复调用**：同一个 dll 在进程内只有一份静态区，宿主可能再次加载它
/// （第二个会话、测试里第二个宿主实例），此时后一次注入覆盖前一次——能力表由宿主决定，
/// 覆盖是正确语义，不该报"重复初始化"让第二次加载失败。
// 指针有效性由 ABI 契约保证：只有 declare_tool! 生成的 `wbp_init` 会调它，
// 而 `wbp_init` 是 extern "C" 导出、参数由宿主传入且在整个初始化期内有效。
// 标成 unsafe fn 会污染每个插件的导出符号，得不偿失。
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn init(host: *const HostApi) -> i32 {
    if host.is_null() {
        return -1;
    }
    // SAFETY: 宿主保证指针在初始化期间有效；按 repr(C) 布局整体拷一份，
    // 之后不再依赖宿主的栈/堆位置。
    let copy = unsafe { std::ptr::read(host) };
    if copy.abi_version != crate::api::ABI_VERSION {
        return -2;
    }
    if copy.size < 8 {
        return -3;
    }
    let mut w = HOST.write().unwrap_or_else(|e| e.into_inner());
    *w = Some(copy);
    0
}

/// 是否已注入（用于插件内部判断"我在宿主里跑还是单测里跑"）。
pub fn ready() -> bool {
    HOST.read().map(|g| g.is_some()).unwrap_or(false)
}

fn host() -> Option<HostApi> {
    *HOST.read().unwrap_or_else(|e| e.into_inner())
}

fn free_host_string(api: HostApi, ptr: *mut c_char) {
    if let Some(f) = api.host_free {
        f(ptr);
    }
}

/// 调一个宿主能力，返回解析后的 JSON（`ok:false` 转成 Err）。
fn invoke(pick: impl Fn(&HostApi) -> Option<JsonCallFn>, req: &Value) -> Result<Value, String> {
    let api = host().ok_or_else(|| "插件未初始化（宿主未注入能力表）".to_string())?;
    let f = pick(&api).ok_or_else(|| "宿主未开放该能力（插件权限未声明？）".to_string())?;
    let body = serde_json::to_string(req).map_err(|e| format!("序列化请求失败：{e}"))?;
    let c = CString::new(body).map_err(|_| "请求含 NUL 字节".to_string())?;
    let out = f(c.as_ptr());
    if out.is_null() {
        return Err("宿主返回空指针".into());
    }
    // SAFETY: 宿主承诺返回以 NUL 结尾的 UTF-8 串。
    let text = unsafe { CStr::from_ptr(out) }.to_string_lossy().to_string();
    free_host_string(api, out);
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("宿主返回非法 JSON：{e}"))?;
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        let msg = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("宿主调用失败")
            .to_string();
        return Err(msg);
    }
    Ok(v)
}

// ---------------------------------------------------------------- HTTP

/// HTTP 请求描述。
#[derive(Debug, Clone)]
pub struct HttpReq {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
    pub follow: bool,
}

impl HttpReq {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
            follow: true,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            method: "POST".into(),
            url: url.into(),
            headers: Vec::new(),
            body: Some(body.into()),
            timeout_ms: None,
            follow: true,
        }
    }

    pub fn method(mut self, m: impl Into<String>) -> Self {
        self.method = m.into().to_uppercase();
        self
    }

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn body(mut self, b: impl Into<String>) -> Self {
        self.body = Some(b.into());
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    pub fn no_follow(mut self) -> Self {
        self.follow = false;
        self
    }

    pub fn ua(self, ua: &str) -> Self {
        self.header("User-Agent", ua)
    }

    pub fn to_json(&self) -> Value {
        let mut h = serde_json::Map::new();
        for (k, v) in &self.headers {
            h.insert(k.clone(), Value::String(v.clone()));
        }
        let mut v = json!({
            "method": self.method,
            "url": self.url,
            "headers": Value::Object(h),
            "follow": self.follow,
        });
        if let Some(b) = &self.body {
            v["body"] = Value::String(b.clone());
        }
        if let Some(t) = self.timeout_ms {
            v["timeout_ms"] = json!(t);
        }
        v
    }
}

/// HTTP 响应。
#[derive(Debug, Clone, Default)]
pub struct HttpResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub final_url: String,
    pub elapsed_ms: u64,
    pub redirects: u32,
}

impl HttpResp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn from_json(v: &Value) -> Self {
        let headers = v
            .get("headers")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .map(|(k, val)| (k.clone(), val.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            status: v.get("status").and_then(Value::as_u64).unwrap_or(0) as u16,
            headers,
            body: v.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
            final_url: v
                .get("final_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            elapsed_ms: v.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0),
            redirects: v.get("redirects").and_then(Value::as_u64).unwrap_or(0) as u32,
        }
    }
}

/// 单次 HTTP。
pub fn http(req: HttpReq) -> Result<HttpResp, String> {
    let v = invoke(|a| a.http, &req.to_json())?;
    Ok(HttpResp::from_json(&v))
}

/// 批量 HTTP（并发与限速在宿主；插件只给"我要发这些"）。
pub fn http_batch(reqs: &[HttpReq], concurrency: usize) -> Result<Vec<Result<HttpResp, String>>, String> {
    let arr: Vec<Value> = reqs.iter().map(|r| r.to_json()).collect();
    let v = invoke(
        |a| a.http_batch,
        &json!({"requests": arr, "concurrency": concurrency}),
    )?;
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(results
        .iter()
        .map(|r| {
            if r.get("ok").and_then(Value::as_bool) == Some(false) {
                Err(r
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("请求失败")
                    .to_string())
            } else {
                Ok(HttpResp::from_json(r))
            }
        })
        .collect())
}

// ------------------------------------------------------------- 会话

/// 会话内请求（Cookie 自动带）。
pub fn session_request(session: &str, req: HttpReq) -> Result<HttpResp, String> {
    let mut v = req.to_json();
    v["session"] = json!(session);
    let out = invoke(|a| a.session_request, &v)?;
    Ok(HttpResp::from_json(&out))
}

/// 会话内表单提交（`fields` 为键值对，宿主负责 URL 编码）。
pub fn session_form(
    session: &str,
    url: &str,
    fields: &[(String, String)],
    method: &str,
) -> Result<HttpResp, String> {
    let mut f = serde_json::Map::new();
    for (k, v) in fields {
        f.insert(k.clone(), Value::String(v.clone()));
    }
    let out = invoke(
        |a| a.session_form,
        &json!({"session": session, "url": url, "fields": Value::Object(f), "method": method}),
    )?;
    Ok(HttpResp::from_json(&out))
}

/// 打开命名会话。
pub fn session_open(name: &str, base_url: &str, ua: &str) -> Result<String, String> {
    let out = invoke(
        |a| a.session_open,
        &json!({"name": name, "base_url": base_url, "ua": ua}),
    )?;
    Ok(out
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_string())
}

/// 读会话 Cookie。
pub fn session_cookies(session: &str) -> Result<Vec<Value>, String> {
    let out = invoke(|a| a.session_cookies, &json!({"session": session}))?;
    Ok(out
        .get("cookies")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

pub fn session_close(session: &str) -> Result<(), String> {
    invoke(|a| a.session_close, &json!({"session": session}))?;
    Ok(())
}

// ---------------------------------------------------------------- TCP

/// 裸 TCP 探测请求。
#[derive(Debug, Clone)]
pub struct TcpReq {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub payload: Vec<u8>,
    pub read_ms: u64,
    pub expect_bytes: usize,
}

impl TcpReq {
    pub fn probe(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: false,
            payload: Vec::new(),
            read_ms: 2500,
            expect_bytes: 4096,
        }
    }

    pub fn payload(mut self, p: impl Into<Vec<u8>>) -> Self {
        self.payload = p.into();
        self
    }

    pub fn text(mut self, p: &str) -> Self {
        self.payload = p.as_bytes().to_vec();
        self
    }

    pub fn tls(mut self) -> Self {
        self.tls = true;
        self
    }

    pub fn read_ms(mut self, ms: u64) -> Self {
        self.read_ms = ms;
        self
    }

    /// 单次读取的字节上限（协议回复较长时调大，如 Redis `INFO`）。
    pub fn expect_bytes(mut self, n: usize) -> Self {
        self.expect_bytes = n;
        self
    }

    pub fn to_json(&self) -> Value {
        json!({
            "host": self.host,
            "port": self.port,
            "tls": self.tls,
            "payload_b64": codec::b64_encode(&self.payload),
            "read_ms": self.read_ms,
            "expect_bytes": self.expect_bytes,
        })
    }
}

/// TCP 探测结果。
#[derive(Debug, Clone, Default)]
pub struct TcpResp {
    pub open: bool,
    pub banner: Vec<u8>,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl TcpResp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.banner).to_string()
    }

    pub fn from_json(v: &Value) -> Self {
        Self {
            open: v.get("open").and_then(Value::as_bool).unwrap_or(false),
            banner: v
                .get("banner_b64")
                .and_then(Value::as_str)
                .map(codec::b64_decode)
                .unwrap_or_default(),
            elapsed_ms: v.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0),
            error: v.get("error").and_then(Value::as_str).map(str::to_string),
        }
    }
}

pub fn tcp(req: TcpReq) -> Result<TcpResp, String> {
    let v = invoke(|a| a.tcp, &req.to_json())?;
    Ok(TcpResp::from_json(&v))
}

/// 批量 TCP 连通性（只判 open，不带 payload）。
pub fn tcp_batch(targets: &[(String, u16)], read_ms: u64, concurrency: usize) -> Result<Vec<TcpResp>, String> {
    let arr: Vec<Value> = targets
        .iter()
        .map(|(h, p)| json!({"host": h, "port": p, "read_ms": read_ms}))
        .collect();
    let v = invoke(
        |a| a.tcp_batch,
        &json!({"targets": arr, "read_ms": read_ms, "concurrency": concurrency}),
    )?;
    Ok(v.get("results")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(TcpResp::from_json).collect())
        .unwrap_or_default())
}

// ---------------------------------------------------------------- DNS

/// 解析记录（rtype：A / AAAA / CNAME / MX / TXT / NS）。
pub fn dns(name: &str, rtype: &str) -> Result<Vec<String>, String> {
    let v = invoke(|a| a.dns, &json!({"name": name, "rtype": rtype}))?;
    Ok(v.get("records")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------- UDP

/// 单包 UDP 探测。
///
/// 无连接协议"没回包"不等于端口关闭，所以字段叫 `responded` 而不是 `open`。
#[derive(Debug, Clone, Default)]
pub struct UdpResp {
    pub responded: bool,
    pub data: Vec<u8>,
    pub elapsed_ms: u64,
    pub remote: String,
    pub error: Option<String>,
}

impl UdpResp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).to_string()
    }
}

pub fn udp(host: &str, port: u16, payload: &[u8], read_ms: u64, expect_bytes: usize) -> Result<UdpResp, String> {
    let v = invoke(
        |a| a.udp,
        &json!({
            "host": host,
            "port": port,
            "payload_b64": codec::b64_encode(payload),
            "read_ms": read_ms,
            "expect_bytes": expect_bytes,
        }),
    )?;
    Ok(UdpResp {
        responded: v.get("responded").and_then(Value::as_bool).unwrap_or(false),
        data: v
            .get("data_b64")
            .and_then(Value::as_str)
            .map(codec::b64_decode)
            .unwrap_or_default(),
        elapsed_ms: v.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0),
        remote: v.get("remote").and_then(Value::as_str).unwrap_or("").into(),
        error: v.get("error").and_then(Value::as_str).map(str::to_string),
    })
}

// ---------------------------------------------------------------- TLS 证书

/// 服务端证书的解析结果（宿主不做任何信任校验，只报告"它给了什么"）。
#[derive(Debug, Clone, Default)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    pub common_name: Option<String>,
    pub serial: String,
    pub alt_names: Vec<String>,
    pub not_before: String,
    pub not_after: String,
    pub days_left: i64,
    pub expired: bool,
    pub not_yet_valid: bool,
    pub sig_alg: String,
    pub weak_signature: bool,
    pub is_ca: bool,
    pub self_signed: bool,
}

impl CertInfo {
    pub fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Self {
            subject: s("subject"),
            issuer: s("issuer"),
            common_name: v.get("common_name").and_then(Value::as_str).map(str::to_string),
            serial: s("serial"),
            alt_names: v
                .get("alt_names")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            not_before: s("not_before"),
            not_after: s("not_after"),
            days_left: v.get("days_left").and_then(Value::as_i64).unwrap_or(0),
            expired: v.get("expired").and_then(Value::as_bool).unwrap_or(false),
            not_yet_valid: v.get("not_yet_valid").and_then(Value::as_bool).unwrap_or(false),
            sig_alg: s("sig_alg"),
            weak_signature: v.get("weak_signature").and_then(Value::as_bool).unwrap_or(false),
            is_ca: v.get("is_ca").and_then(Value::as_bool).unwrap_or(false),
            self_signed: v.get("self_signed").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    /// 证书上的 DNS 名（去掉 `DNS:` 前缀）。
    pub fn dns_names(&self) -> Vec<String> {
        self.alt_names
            .iter()
            .filter_map(|n| n.strip_prefix("DNS:"))
            .map(str::to_string)
            .collect()
    }
}

pub fn tls_cert(host: &str, port: u16, server_name: &str) -> Result<CertInfo, String> {
    let v = invoke(
        |a| a.tls_cert,
        &json!({"host": host, "port": port, "server_name": server_name}),
    )?;
    Ok(CertInfo::from_json(&v))
}

// ---------------------------------------------------------------- 本机信息

/// 本机身份：`hostname` / `user` / `os` / `arch` / `cpus`。
pub fn sysinfo_host() -> Result<Value, String> {
    invoke(|a| a.sysinfo, &json!({"what": "host"}))
}

/// 本机网卡表：`{interfaces:[{name,desc,mac,ip,mask,gateway,dhcp}], primary_ip}`。
pub fn sysinfo_net() -> Result<Value, String> {
    invoke(|a| a.sysinfo, &json!({"what": "net"}))
}

// ------------------------------------------------------------ 知识 / KV

/// 知识命中的单条。
#[derive(Debug, Clone, Default)]
pub struct KbHit {
    pub title: String,
    pub body: String,
    pub score: u32,
}

pub fn kb_search(query: &str, top: usize) -> Result<Vec<KbHit>, String> {
    let v = invoke(|a| a.kb_search, &json!({"query": query, "top": top}))?;
    Ok(v.get("hits")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|h| KbHit {
                    title: h.get("title").and_then(Value::as_str).unwrap_or("").into(),
                    body: h.get("body").and_then(Value::as_str).unwrap_or("").into(),
                    score: h.get("score").and_then(Value::as_u64).unwrap_or(0) as u32,
                })
                .collect()
        })
        .unwrap_or_default())
}

pub fn kv_get(ns: &str, key: &str) -> Result<Option<Value>, String> {
    let v = invoke(|a| a.kv_get, &json!({"ns": ns, "key": key}))?;
    Ok(v.get("value").filter(|x| !x.is_null()).cloned())
}

pub fn kv_put(ns: &str, key: &str, value: &Value) -> Result<(), String> {
    invoke(|a| a.kv_put, &json!({"ns": ns, "key": key, "value": value}))?;
    Ok(())
}

/// 原始结果落盘，返回句柄（LLM 只拿句柄，不拿全文）。
pub fn raw_store(name: &str, content: &str, mime: &str) -> Result<Value, String> {
    invoke(
        |a| a.raw_store,
        &json!({"name": name, "content": content, "mime": mime}),
    )
}

// ------------------------------------------------------------ 审计 / 日志

/// 发审计事件（纳入证据链）。
pub fn audit(event: &str, payload: Value) {
    let Some(api) = host() else { return };
    let Some(f) = api.audit else { return };
    let Ok(body) = serde_json::to_string(&json!({"event": event, "payload": payload})) else {
        return;
    };
    let Ok(c) = CString::new(body) else { return };
    f(c.as_ptr());
}

/// 日志。
pub fn log(level: i32, msg: &str) {
    let Some(api) = host() else { return };
    let Some(f) = api.log else { return };
    let Ok(c) = CString::new(msg) else { return };
    f(level, c.as_ptr());
}
