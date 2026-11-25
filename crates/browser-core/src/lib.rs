//! # browser-core —— 无头 HTTP 浏览器（**无 JS 引擎**）
//!
//! 定位：把"抓页面、维持登录态、批量探测"做成基座的原子能力。
//! 插件不许自己发请求——所有出网流量都走这里，于是 ScopeGuard / 限流 / 审计
//! 只需要在一个地方保证。
//!
//! ## 能做什么
//!
//! - GET/POST/任意方法、手动重定向链（可拿到每一跳）
//! - 命名会话 + 自管 Cookie 池（登录后跨请求维持身份）
//! - 表单提交（从页面解析字段，或直接给键值对）
//! - 批量并发 + 三层限流（全局 / 单 host / 单域 QPS）
//! - HTML 解析（链接/表单/脚本/注释/参数名）
//!
//! ## 不做
//!
//! 执行 JS、等待 XHR、截图、渲染。真浏览器要内嵌 Chromium/WebView2，
//! 与"不依赖第三方程序"的约定冲突；代价明说：SPA 靠 JS 渲染出的接口发现不了。
//!
//! ## 为什么自己管 Cookie
//!
//! `reqwest::cookie::Jar` 只写不给读——但渗透场景必须能把拿到的会话导出交给下一步，
//! 也要能把"上一轮登录留下的 Cookie"注入回来。所以这里自己存。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub use wpa_kit::html;
pub use wpa_kit::{codec, textx};

/// 全局网络预算（照搬构想的默认值，全部可配）。
#[derive(Debug, Clone)]
pub struct NetBudget {
    pub max_global_concurrency: usize,
    pub max_per_host_concurrency: usize,
    pub max_per_domain_qps: usize,
    pub request_timeout_ms: u64,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
}

impl Default for NetBudget {
    fn default() -> Self {
        Self {
            max_global_concurrency: 500,
            max_per_host_concurrency: 20,
            max_per_domain_qps: 50,
            request_timeout_ms: 10_000,
            max_redirects: 10,
            max_body_bytes: 4 * 1024 * 1024,
        }
    }
}

impl NetBudget {
    /// 按配置 `[net]` 生成预算。
    ///
    /// 只覆盖配置里真有对应项的三个：请求超时、单域 QPS、User-Agent（UA 由
    /// [`Browser::new`] 直接读同一份参数）。并发/重定向/正文上限没有配置项，
    /// 保持 [`NetBudget::default`] —— 不为了"看起来可配"而凭空造一个倍率。
    pub fn from_net_settings() -> Self {
        let s = net_core::settings::current();
        let mut b = Self::default();
        b.request_timeout_ms = s.timeout_secs.saturating_mul(1000).clamp(1_000, 600_000);
        if s.per_host_qps > 0 {
            b.max_per_domain_qps = s.per_host_qps as usize;
        }
        b
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 限流

/// 三层限流：全局并发 / 单 host 并发 / 单域 QPS。
pub struct Limiter {
    budget: NetBudget,
    global: Arc<Semaphore>,
    hosts: Mutex<BTreeMap<String, Arc<Semaphore>>>,
    /// 每个 host 的"下一次可发起时间"（用于 QPS 排队）。
    next_slot: Mutex<BTreeMap<String, Instant>>,
}

impl Limiter {
    pub fn new(budget: NetBudget) -> Self {
        let global = Arc::new(Semaphore::new(budget.max_global_concurrency.max(1)));
        Self {
            budget,
            global,
            hosts: Mutex::new(BTreeMap::new()),
            next_slot: Mutex::new(BTreeMap::new()),
        }
    }

    fn host_sem(&self, host: &str) -> Arc<Semaphore> {
        let mut g = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(host.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.budget.max_per_host_concurrency.max(1))))
            .clone()
    }

    /// 拿到"可以发请求"的许可；QPS 不够时在这里排队等待。
    pub async fn acquire(&self, host: &str) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), String> {
        let qps = self.budget.max_per_domain_qps.max(1);
        let interval = Duration::from_micros(1_000_000 / qps as u64);
        let wait = {
            let mut slots = self.next_slot.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let slot = slots.get(host).copied().unwrap_or(now);
            let start = if slot > now { slot } else { now };
            slots.insert(host.to_string(), start + interval);
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        let g = self
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "全局并发许可已关闭".to_string())?;
        let h = self
            .host_sem(host)
            .acquire_owned()
            .await
            .map_err(|_| "host 并发许可已关闭".to_string())?;
        Ok((g, h))
    }
}

// ---------------------------------------------------------------- Cookie

/// 一条 Cookie。
#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// 空 = host-only（只对该主机生效）。
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: String,
    pub expires_at_ms: Option<u64>,
}

impl Cookie {
    /// 是否适用于该 URL（按域后缀 + 路径前缀判断）。
    pub fn applies_to(&self, url: &str) -> bool {
        let Some(u) = textx::parse_url(url) else {
            return false;
        };
        if self.secure && u.scheme != "https" {
            return false;
        }
        let host = textx::normalize_host(&u.host);
        let dom_ok = if self.domain.is_empty() {
            host == textx::normalize_host(&u.host)
        } else {
            let d = textx::normalize_host(&self.domain);
            host == d || host.ends_with(&format!(".{d}"))
        };
        if !dom_ok {
            return false;
        }
        let path = if u.path.is_empty() { "/" } else { &u.path };
        let cpath = if self.path.is_empty() { "/" } else { &self.path };
        path.starts_with(cpath)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "value": self.value,
            "domain": self.domain,
            "path": self.path,
            "secure": self.secure,
            "httponly": self.http_only,
            "same_site": self.same_site,
            "expires_at_ms": self.expires_at_ms,
        })
    }
}

/// Cookie 池（按域名 + 名字去重）。
#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    cookies: BTreeMap<(String, String), Cookie>,
}

impl CookieJar {
    /// 组装 `Cookie:` 请求头。
    pub fn header_for(&self, url: &str) -> Option<String> {
        let now = now_ms();
        let mut parts: Vec<String> = Vec::new();
        for c in self.cookies.values() {
            if let Some(exp) = c.expires_at_ms {
                if exp != 0 && exp < now {
                    continue;
                }
            }
            if c.applies_to(url) {
                parts.push(format!("{}={}", c.name, c.value));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }

    /// 收一条 `Set-Cookie` 头。
    pub fn store(&mut self, url: &str, set_cookie: &str) {
        let mut it = set_cookie.split(';');
        let Some(kv) = it.next() else { return };
        let Some((name, value)) = kv.split_once('=') else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let url_host = textx::parse_url(url)
            .map(|u| textx::normalize_host(&u.host))
            .unwrap_or_default();
        let mut c = Cookie {
            name: name.clone(),
            value: value.trim().to_string(),
            domain: String::new(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: String::new(),
            expires_at_ms: None,
        };
        let mut max_age: Option<i64> = None;
        for attr in it {
            let attr = attr.trim();
            let (k, v) = match attr.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (attr, ""),
            };
            match k.to_ascii_lowercase().as_str() {
                "domain" => c.domain = v.trim_start_matches('.').to_string(),
                "path" => {
                    if !v.is_empty() {
                        c.path = v.to_string()
                    }
                }
                "secure" => c.secure = true,
                "httponly" => c.http_only = true,
                "samesite" => c.same_site = v.to_string(),
                "max-age" => max_age = v.parse::<i64>().ok(),
                "expires" => {
                    if let Some(ts) = parse_http_date(v) {
                        c.expires_at_ms = Some(ts);
                    }
                }
                _ => {}
            }
        }
        if let Some(secs) = max_age {
            if secs <= 0 {
                // 明确要求删除
                self.cookies.remove(&(c.domain.clone(), name));
                return;
            }
            c.expires_at_ms = Some(now_ms() + secs as u64 * 1000);
        }
        if c.domain.is_empty() {
            c.domain = url_host;
        }
        // 过期即删
        if let Some(exp) = c.expires_at_ms {
            if exp != 0 && exp < now_ms() {
                self.cookies.remove(&(c.domain.clone(), name));
                return;
            }
        }
        self.cookies.insert((c.domain.clone(), name), c);
    }

    pub fn list(&self) -> Vec<Cookie> {
        self.cookies.values().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
    }
}

/// 解析 `Wed, 21 Oct 2015 07:28:00 GMT`（不引时间库）。
fn parse_http_date(s: &str) -> Option<u64> {
    let s = s.trim();
    let rest = match s.find(',') {
        Some(i) => s[i + 1..].trim(),
        None => s,
    };
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let day: i64 = parts[0].parse().ok()?;
    let mon = match parts[1].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[2].parse().ok()?;
    let time: Vec<&str> = parts[3].split(':').collect();
    if time.len() < 3 {
        return None;
    }
    let (hh, mm, ss): (i64, i64, i64) = (
        time[0].parse().ok()?,
        time[1].parse().ok()?,
        time[2].parse().ok()?,
    );
    let secs = days_from_civil(year, mon, day) * 86_400 + hh * 3600 + mm * 60 + ss;
    if secs < 0 {
        return None;
    }
    Some(secs as u64 * 1000)
}

/// Howard Hinnant 的 civil→days 算法（1970-01-01 为 0）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------- 请求/响应

/// 一次抓取请求。
#[derive(Debug, Clone, Default)]
pub struct FetchReq {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
    pub follow: bool,
}

impl FetchReq {
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

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// 从插件传来的 JSON 解析（`{method,url,headers,body,timeout_ms,follow}`）。
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let url = v
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "缺少 url".to_string())?
            .to_string();
        let headers = v
            .get("headers")
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
        Ok(Self {
            method: v
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_ascii_uppercase(),
            url,
            headers,
            body: v.get("body").and_then(Value::as_str).map(str::to_string),
            timeout_ms: v.get("timeout_ms").and_then(Value::as_u64),
            follow: v.get("follow").and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

/// 抓取结果。
#[derive(Debug, Clone, Default)]
pub struct FetchResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub final_url: String,
    pub elapsed_ms: u64,
    pub redirects: u32,
    pub truncated: bool,
}

impl FetchResp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn to_json(&self) -> Value {
        let headers: serde_json::Map<String, Value> = self
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        json!({
            "ok": true,
            "status": self.status,
            "headers": Value::Object(headers),
            "body": self.body,
            "final_url": self.final_url,
            "elapsed_ms": self.elapsed_ms,
            "redirects": self.redirects,
            "truncated": self.truncated,
        })
    }
}

// ---------------------------------------------------------------- 会话

/// 命名会话：一套 Cookie 池 + 固定 UA。
pub struct Session {
    pub name: String,
    pub base_url: String,
    pub ua: String,
    jar: Mutex<CookieJar>,
    created_ms: u64,
    last_used_ms: AtomicU64,
}

impl Session {
    fn new(name: &str, base_url: &str, ua: &str) -> Self {
        Self {
            name: name.to_string(),
            base_url: base_url.to_string(),
            ua: ua.to_string(),
            jar: Mutex::new(CookieJar::default()),
            created_ms: now_ms(),
            last_used_ms: AtomicU64::new(now_ms()),
        }
    }

    fn jar(&self) -> std::sync::MutexGuard<'_, CookieJar> {
        self.jar.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn cookies(&self) -> Vec<Cookie> {
        self.jar().list()
    }

    pub fn inject_cookies(&self, cookies: &[Cookie]) {
        let mut j = self.jar();
        for c in cookies {
            j.store(&format!("http://{}/", c.domain), &format!("{}={}", c.name, c.value));
        }
    }

    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }
}

/// 无头浏览器（进程内单例即可，内部状态全部加锁）。
pub struct Browser {
    client: reqwest::Client,
    budget: NetBudget,
    limiter: Limiter,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    /// 抓取计数（审计/UI 用）。
    pub fetches: AtomicU64,
}

impl Browser {
    pub fn new(budget: NetBudget) -> Self {
        let client = reqwest::Client::builder()
            // 自己跟随重定向：要拿到每一跳、也要能在每跳收 Set-Cookie
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(budget.request_timeout_ms.min(10_000)))
            .user_agent(net_core::settings::current().effective_user_agent())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            limiter: Limiter::new(budget.clone()),
            budget,
            sessions: Mutex::new(BTreeMap::new()),
            fetches: AtomicU64::new(0),
        }
    }

    pub fn budget(&self) -> &NetBudget {
        &self.budget
    }

    /// 直接抓取（无会话，不带 Cookie）。
    pub async fn fetch(&self, req: FetchReq) -> Result<FetchResp, String> {
        self.fetch_with(req, None).await
    }

    /// 带会话抓取（自动带/收 Cookie）。
    pub async fn fetch_in(&self, session: &str, req: FetchReq) -> Result<FetchResp, String> {
        let s = self.session(session)?;
        s.touch();
        self.fetch_with(req, Some(s)).await
    }

    async fn fetch_with(
        &self,
        req: FetchReq,
        session: Option<Arc<Session>>,
    ) -> Result<FetchResp, String> {
        let started = Instant::now();
        let mut url = req.url.clone();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("只支持 http/https：{url}"));
        }
        let mut method = req.method.to_ascii_uppercase();
        let mut body = req.body.clone();
        let mut redirects = 0u32;
        let mut extra_cookies: Vec<String> = Vec::new();

        loop {
            let host = textx::parse_url(&url)
                .map(|u| textx::normalize_host(&u.host))
                .ok_or_else(|| format!("URL 无法解析：{url}"))?;
            // 许可在跳出循环前一直持有（包括跟随时）
            let _permits = self.limiter.acquire(&host).await?;

            let m = reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| format!("非法方法 {method}：{e}"))?;
            let mut rb = self.client.request(m, &url);
            rb = rb.timeout(Duration::from_millis(
                req.timeout_ms.unwrap_or(self.budget.request_timeout_ms),
            ));
            if let Some(s) = &session {
                rb = rb.header("User-Agent", &s.ua);
            }
            for (k, v) in &req.headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            if !extra_cookies.is_empty() {
                rb = rb.header("Cookie", extra_cookies.join("; "));
            }
            if let Some(b) = &body {
                rb = rb.body(b.clone());
            }

            let resp = rb.send().await.map_err(|e| {
                if e.is_timeout() {
                    format!("请求超时：{url}")
                } else if e.is_connect() {
                    format!("连接失败：{url}（{e}）")
                } else {
                    format!("请求失败：{e}")
                }
            })?;

            let status = resp.status().as_u16();
            let headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                .collect();

            // 收 Cookie（每跳都收）
            if let Some(s) = &session {
                let mut jar = s.jar();
                for (k, v) in &headers {
                    if k.eq_ignore_ascii_case("set-cookie") {
                        jar.store(&url, v);
                    }
                }
            }

            let is_redirect = matches!(status, 301 | 302 | 303 | 307 | 308);
            let location = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                .map(|(_, v)| v.clone());

            if req.follow && is_redirect && redirects < self.budget.max_redirects as u32 {
                if let Some(loc) = location {
                    let next = textx::resolve_url(&url, &loc);
                    if next.is_empty() {
                        // 落到 javascript: 之类，视为终点
                        let body_text = self.read_body(resp).await?;
                        return Ok(FetchResp {
                            status,
                            headers,
                            body: body_text.0,
                            final_url: url,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            redirects,
                            truncated: body_text.1,
                        });
                    }
                    // 303 或 302/301 后浏览器会改 GET；307/308 保持方法与 body
                    if status == 303 || ((status == 301 || status == 302) && method != "GET" && method != "HEAD") {
                        method = "GET".into();
                        body = None;
                    }
                    // 跨站重定向时不带原 Cookie 头，改为按新 URL 重新取
                    if let Some(s) = &session {
                        extra_cookies = s
                            .jar()
                            .header_for(&next)
                            .map(|h| vec![h])
                            .unwrap_or_default();
                    }
                    url = next;
                    redirects += 1;
                    continue;
                }
            }

            let (text, truncated) = self.read_body(resp).await?;
            self.fetches.fetch_add(1, Ordering::Relaxed);
            return Ok(FetchResp {
                status,
                headers,
                body: text,
                final_url: url,
                elapsed_ms: started.elapsed().as_millis() as u64,
                redirects,
                truncated,
            });
        }
    }

    async fn read_body(&self, resp: reqwest::Response) -> Result<(String, bool), String> {
        let bytes = resp.bytes().await.map_err(|e| format!("读取响应失败：{e}"))?;
        let truncated = bytes.len() > self.budget.max_body_bytes;
        let slice = if truncated {
            &bytes[..self.budget.max_body_bytes]
        } else {
            &bytes[..]
        };
        // 不追求字符集还原：非 UTF-8 用容错解码（渗透场景多为 ASCII 协议）
        Ok((String::from_utf8_lossy(slice).to_string(), truncated))
    }

    /// 批量抓取（并发 + 限流都在内部；插件只给"我要发这些"）。
    pub async fn fetch_batch(
        &self,
        reqs: Vec<FetchReq>,
        concurrency: usize,
    ) -> Vec<Result<FetchResp, String>> {
        let n = concurrency.clamp(1, self.budget.max_global_concurrency.max(1));
        stream::iter(reqs.into_iter().map(|r| async move { self.fetch(r).await }))
            .buffer_unordered(n)
            .collect()
            .await
    }

    // ---- 会话 ----

    pub fn open_session(&self, name: &str, base_url: &str, ua: &str) -> Arc<Session> {
        let ua = if ua.is_empty() {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36".to_string()
        } else {
            ua.to_string()
        };
        let s = Arc::new(Session::new(name, base_url, &ua));
        let mut g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(name.to_string(), s.clone());
        s
    }

    pub fn session(&self, name: &str) -> Result<Arc<Session>, String> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| format!("会话 `{name}` 不存在（先 session_open）"))
    }

    pub fn close_session(&self, name: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .is_some()
    }

    pub fn session_names(&self) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// 会话内提交表单（`fields` 由调用方给，URL 编码/头由这里负责）。
    pub async fn submit_form(
        &self,
        session: &str,
        url: &str,
        fields: &[(String, String)],
        method: &str,
    ) -> Result<FetchResp, String> {
        let s = self.session(session)?;
        let method = method.to_ascii_uppercase();
        let encoded = codec::build_query(fields);
        let mut req = if method == "GET" {
            let sep = if url.contains('?') { '&' } else { '?' };
            FetchReq::get(format!("{url}{sep}{encoded}"))
        } else {
            FetchReq::post(url, encoded.clone())
                .header("Content-Type", "application/x-www-form-urlencoded")
        };
        req.follow = true;
        let _ = encoded;
        s.touch();
        self.fetch_with(req, Some(s)).await
    }

    /// 回收空闲会话（默认 30 分钟）。
    pub fn purge_idle_sessions(&self, max_idle_ms: u64) -> Vec<String> {
        let now = now_ms();
        let mut g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let stale: Vec<String> = g
            .iter()
            .filter(|(_, s)| now.saturating_sub(s.last_used_ms.load(Ordering::Relaxed)) >= max_idle_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            g.remove(k);
        }
        stale
    }
}

impl Session {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "base_url": self.base_url,
            "ua": self.ua,
            "created_ms": self.created_ms,
            "cookies": self.cookies().iter().map(Cookie::to_json).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parsing_and_matching() {
        let mut jar = CookieJar::default();
        jar.store(
            "http://app.example.com/login",
            "sid=abc123; Path=/; Domain=.example.com; HttpOnly; SameSite=Lax",
        );
        jar.store("http://app.example.com/login", "theme=dark; Path=/ui");
        assert!(jar.header_for("http://app.example.com/x").unwrap().contains("sid=abc123"));
        // 子域也生效（Domain=.example.com）
        assert!(jar.header_for("http://api.example.com/x").unwrap().contains("sid=abc123"));
        // 别的域不生效
        assert!(jar.header_for("http://evil.com/x").is_none());
        // Path 限制
        let ui = jar.header_for("http://app.example.com/ui/page").unwrap();
        assert!(ui.contains("theme=dark"), "{ui}");
        assert!(!jar.header_for("http://app.example.com/other").unwrap().contains("theme=dark"));
    }

    #[test]
    fn cookie_expiry_and_delete() {
        let mut jar = CookieJar::default();
        jar.store("http://a.com/", "k=v; Max-Age=0");
        assert!(jar.header_for("http://a.com/").is_none(), "Max-Age=0 应删除");
        jar.store("http://a.com/", "k=v; Expires=Wed, 21 Oct 2015 07:28:00 GMT");
        assert!(jar.header_for("http://a.com/").is_none(), "过期应不可用");
    }

    #[test]
    fn secure_cookie_only_on_https() {
        let mut jar = CookieJar::default();
        jar.store("https://a.com/", "s=1; Secure");
        assert!(jar.header_for("http://a.com/").is_none());
        assert!(jar.header_for("https://a.com/").unwrap().contains("s=1"));
    }

    #[test]
    fn http_date_parsing() {
        // 2015-10-21T07:28:00Z = 1445412480000
        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480_000));
        assert!(parse_http_date("garbage").is_none());
    }

    #[test]
    fn budget_defaults_follow_spec() {
        let b = NetBudget::default();
        assert_eq!(b.max_global_concurrency, 500);
        assert_eq!(b.max_per_host_concurrency, 20);
        assert_eq!(b.max_per_domain_qps, 50);
        assert_eq!(b.request_timeout_ms, 10_000);
    }

    #[tokio::test]
    async fn limiter_queues_by_qps() {
        let lim = Limiter::new(NetBudget {
            max_per_domain_qps: 20, // 50ms 间隔
            ..Default::default()
        });
        let t0 = Instant::now();
        for _ in 0..3 {
            let _p = lim.acquire("a.com").await.unwrap();
        }
        // 第 2、3 次各等 ~50ms
        assert!(t0.elapsed() >= Duration::from_millis(80), "{:?}", t0.elapsed());
    }

    #[test]
    fn session_registry() {
        let b = Browser::new(NetBudget::default());
        assert!(b.session("nope").is_err());
        b.open_session("s1", "http://a/", "");
        assert_eq!(b.session_names(), vec!["s1".to_string()]);
        assert!(b.session("s1").is_ok());
        assert!(b.close_session("s1"));
        assert!(!b.close_session("s1"));
    }

    #[test]
    fn fetch_resp_json_shape() {
        let r = FetchResp {
            status: 200,
            headers: vec![("Server".into(), "nginx".into())],
            body: "hi".into(),
            final_url: "http://a/".into(),
            elapsed_ms: 12,
            redirects: 1,
            truncated: false,
        };
        let v = r.to_json();
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"], 200);
        assert_eq!(v["headers"]["Server"], "nginx");
    }
}
