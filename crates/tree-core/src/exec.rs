//! 声明式执行器：树包里的工具**不必编译**，声明 `exec` 即可。
//!
//! 四路执行，全部在基座执行链**之内**（ScopeGuard → 拒绝清单 → 知识约束 →
//! 风险审批 → 执行 → 审计留痕），树包永远拿不到未经审批的直接执行权：
//!
//! | kind | 执行 | 约束 |
//! |---|---|---|
//! | `http` | 基座共享连接池 | 拒绝 `file://` 等非 http(s) 协议 |
//! | `command` | argv 直传，**不经 shell** | 必须有 program；cwd 限定群组目录/工作区 |
//! | `python` | 基座沙箱解释器 | 脚本必须落在群组目录内，拒绝 `..` |
//! | `primitive` | 基座通用原语（`net.*` / `text.*`） | 原语自身风险与红线照旧 |
//!
//! 模板占位 `{{name}}` 从（插槽填充后的）参数取值；缺值即拒绝执行，不静默留空。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use wpa_api::{ApiTool, HttpRequest, RiskTier, ToolReply, ToolRequest};

#[cfg(test)]
use wpa_api::GroupRuntime;

use crate::pnc::{parse_risk, toml_to_json, ExecSpec, ToolDecl};
use crate::slots::{apply_slots, strip_from_schema, SlotContext};

/// 基座原语调用器（由 engine 注入，转发到内核 Registry）。
///
/// 放在 trait 后面是为了让 `tree-core` 不认识 `kern-core`——树包层只认契约。
#[async_trait]
pub trait PrimitiveInvoker: Send + Sync + 'static {
    async fn invoke(&self, primitive: &str, args: Value) -> Result<ToolReply, String>;
    /// 原语是否已注册（装配期校验用；默认认为存在）。
    fn has(&self, _primitive: &str) -> bool {
        true
    }
}

/// 输出截断上限（给 LLM 的回显；完整明细走 detail）。
const SUMMARY_CAP: usize = 4000;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// 一个声明式工具。
pub struct DeclarativeTool {
    decl: ToolDecl,
    /// 群组目录（相对路径与 python 脚本的基准）。
    base_dir: PathBuf,
    /// 声明归属组（审计与工具面收敛）。
    group: String,
    /// 原语调用器（仅 `exec.kind = primitive` 需要）。
    invoker: Option<Arc<dyn PrimitiveInvoker>>,
    /// 预渲染的 schema（已移除插槽字段）。
    schema: Value,
}

impl DeclarativeTool {
    pub fn new(
        decl: ToolDecl,
        base_dir: PathBuf,
        group: String,
        invoker: Option<Arc<dyn PrimitiveInvoker>>,
    ) -> Self {
        let mut schema = decl
            .parameters
            .as_ref()
            .map(toml_to_json)
            .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
        if schema.get("type").is_none() {
            if let Some(obj) = schema.as_object_mut() {
                obj.insert("type".into(), Value::String("object".into()));
            }
        }
        strip_from_schema(&mut schema, &decl.slots);
        Self { decl, base_dir, group, invoker, schema }
    }

    pub fn declared_risk(&self) -> RiskTier {
        parse_risk(self.decl.risk.as_deref())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.decl.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, 600))
    }

    /// 插槽上下文：授权范围来自基座（红线不下放给模型）。
    fn slot_ctx(&self, req: &ToolRequest) -> SlotContext {
        let mut profile = BTreeMap::new();
        profile.insert("group".into(), self.group.clone());
        profile.insert("timeout".into(), self.timeout().as_secs().to_string());
        SlotContext {
            scope: req.authorized_scope.clone(),
            workspace_name: String::new(),
            workspace_cwd: req
                .runtime
                .data_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
            profile,
            session_id: req.session_id.clone(),
        }
    }

    fn expand(&self, tpl: &str, args: &Value) -> Result<String, String> {
        let mut out = String::with_capacity(tpl.len());
        let mut rest = tpl;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return Err(format!("模板占位未闭合：{tpl}"));
            };
            let key = after[..end].trim();
            let val = args.get(key).ok_or_else(|| format!("模板参数缺失：{key}"))?;
            out.push_str(&scalar_to_string(val));
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let head: String = s.chars().take(cap).collect();
    format!("{head}\n…（已截断，共 {} 字符）", s.chars().count())
}

#[async_trait]
impl ApiTool for DeclarativeTool {
    fn name(&self) -> &str {
        &self.decl.name
    }

    fn description(&self) -> String {
        self.decl.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn risk(&self) -> RiskTier {
        self.declared_risk()
    }

    fn risk_for(&self, _args: &Value) -> RiskTier {
        // 声明档即参数级档：声明式工具不做参数细分（细分由原语自己负责）。
        self.declared_risk()
    }

    fn concurrency_safe(&self, _args: &Value) -> bool {
        self.decl.concurrency_safe.unwrap_or(false)
    }

    async fn call(&self, req: ToolRequest) -> ToolReply {
        let started = Instant::now();
        let mut args = req.args.clone();
        let ctx = self.slot_ctx(&req);
        let missing = apply_slots(&mut args, &self.decl.slots, &ctx);
        if !missing.is_empty() {
            return ToolReply::err(
                format!(
                    "缺少上下文插槽：{}（通常是授权范围为空或空间未绑定工作目录）",
                    missing.join("、")
                ),
                started.elapsed().as_millis() as u64,
            );
        }

        let fut = self.run(&req, &args, started);
        match tokio::time::timeout(self.timeout(), fut).await {
            Ok(reply) => reply,
            Err(_) => ToolReply::err(
                format!("工具 {} 执行超时（{}s）", self.decl.name, self.timeout().as_secs()),
                started.elapsed().as_millis() as u64,
            ),
        }
    }
}

impl DeclarativeTool {
    async fn run(&self, req: &ToolRequest, args: &Value, started: Instant) -> ToolReply {
        let ms = || started.elapsed().as_millis() as u64;
        match &self.decl.exec {
            ExecSpec::Http { method, url, headers, body, expect } => {
                let url = match self.expand(url, args) {
                    Ok(u) => u,
                    Err(e) => return ToolReply::err(e, ms()),
                };
                let lower = url.to_ascii_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                    return ToolReply::err(format!("拒绝非 http(s) 目标：{url}"), ms());
                }
                let mut hr = match method.to_ascii_uppercase().as_str() {
                    "GET" => HttpRequest::get(&url),
                    "POST" => {
                        let b = match body.as_deref().map(|t| self.expand(t, args)).transpose() {
                            Ok(b) => b.unwrap_or_default(),
                            Err(e) => return ToolReply::err(e, ms()),
                        };
                        HttpRequest::post(&url, b)
                    }
                    other => HttpRequest {
                        method: other.to_string(),
                        url: url.clone(),
                        headers: vec![],
                        body: None,
                        timeout_ms: None,
                    },
                };
                for (k, v) in headers {
                    match self.expand(v, args) {
                        Ok(val) => hr = hr.header(k.clone(), val),
                        Err(e) => return ToolReply::err(e, ms()),
                    }
                }
                hr = hr.timeout_ms(self.timeout().as_millis() as u64);
                match req.runtime.http().send(hr).await {
                    Ok(resp) => {
                        let ok = if expect.is_empty() {
                            (200..400).contains(&resp.status)
                        } else {
                            expect.contains(&resp.status)
                        };
                        let summary = format!(
                            "{} {} → {}（{} 字节）",
                            self.decl.name,
                            url,
                            resp.status,
                            resp.body.len()
                        );
                        let detail = serde_json::json!({
                            "url": url,
                            "status": resp.status,
                            "headers": resp.headers,
                            "body": truncate(&resp.body, 20000),
                        });
                        if ok {
                            ToolReply::json(summary, detail, ms())
                        } else {
                            let mut r = ToolReply::json(summary, detail, ms());
                            r.ok = false;
                            r
                        }
                    }
                    Err(e) => ToolReply::err(format!("请求失败：{e}"), ms()),
                }
            }

            ExecSpec::Command { program, args: argv, stdin } => {
                let mut cmd = tokio::process::Command::new(program);
                for a in argv {
                    match self.expand(a, args) {
                        Ok(v) => {
                            cmd.arg(v);
                        }
                        Err(e) => return ToolReply::err(e, ms()),
                    }
                }
                if let Some(dir) = self.exec_cwd() {
                    cmd.current_dir(dir);
                }
                cmd.kill_on_drop(true);
                cmd.stdin(std::process::Stdio::piped());
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => return ToolReply::err(format!("启动失败 {program}：{e}"), ms()),
                };
                if let Some(input) = stdin {
                    // stdin 与 args 一样支持 {{name}} 模板：缺参数要报错而不是原样下发
                    let rendered = match self.expand(&input, args) {
                        Ok(v) => v,
                        Err(e) => return ToolReply::err(e, ms()),
                    };
                    use tokio::io::AsyncWriteExt;
                    if let Some(mut pipe) = child.stdin.take() {
                        let _ = pipe.write_all(rendered.as_bytes()).await;
                        let _ = pipe.shutdown().await;
                    }
                }
                match child.wait_with_output().await {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        let code = out.status.code().unwrap_or(-1);
                        let summary = truncate(
                            &format!(
                                "{} 退出码 {code}\n{}",
                                self.decl.name,
                                if stdout.trim().is_empty() { stderr.as_str() } else { stdout.as_str() }
                            ),
                            SUMMARY_CAP,
                        );
                        let detail = serde_json::json!({
                            "exit_code": code,
                            "stdout": truncate(&stdout, 40000),
                            "stderr": truncate(&stderr, 10000),
                        });
                        if out.status.success() {
                            ToolReply::json(summary, detail, ms())
                        } else {
                            let mut r = ToolReply::json(summary, detail, ms());
                            r.ok = false;
                            r
                        }
                    }
                    Err(e) => ToolReply::err(format!("等待进程失败：{e}"), ms()),
                }
            }

            ExecSpec::Python { script, args: argv } => {
                let Some(interp) = find_python() else {
                    return ToolReply::err(
                        "未找到 Python 解释器：可设环境变量 WINDBOT_PYTHON 指定，\
                         或把便携版放在 exe 同级 python\\python.exe，或让 python 在 PATH 上"
                            .to_string(),
                        ms(),
                    );
                };
                let path = match self.resolve_inside(script) {
                    Ok(p) => p,
                    Err(e) => return ToolReply::err(e, ms()),
                };
                let mut cmd = tokio::process::Command::new(interp);
                cmd.arg(&path);
                for a in argv {
                    match self.expand(a, args) {
                        Ok(v) => {
                            cmd.arg(v);
                        }
                        Err(e) => return ToolReply::err(e, ms()),
                    }
                }
                cmd.current_dir(self.base_dir.clone());
                cmd.kill_on_drop(true);
                match cmd.output().await {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        let code = out.status.code().unwrap_or(-1);
                        let summary = truncate(
                            &format!("{} 退出码 {code}\n{}", self.decl.name, if stdout.trim().is_empty() { stderr.as_str() } else { stdout.as_str() }),
                            SUMMARY_CAP,
                        );
                        let detail = serde_json::json!({
                            "exit_code": code,
                            "stdout": truncate(&stdout, 40000),
                            "stderr": truncate(&stderr, 10000),
                        });
                        if out.status.success() {
                            ToolReply::json(summary, detail, ms())
                        } else {
                            let mut r = ToolReply::json(summary, detail, ms());
                            r.ok = false;
                            r
                        }
                    }
                    Err(e) => ToolReply::err(format!("Python 执行失败：{e}"), ms()),
                }
            }

            ExecSpec::Primitive { primitive, passthrough } => {
                let Some(inv) = &self.invoker else {
                    return ToolReply::err(
                        format!("原语 {primitive} 不可用（基座未注入原语调用器）"),
                        ms(),
                    );
                };
                let mut call_args = args.clone();
                if let Some(obj) = call_args.as_object_mut() {
                    obj.remove("__tool");
                }
                if !*passthrough {
                    call_args = serde_json::json!({});
                }
                match inv.invoke(primitive, call_args).await {
                    Ok(mut r) => {
                        r.duration_ms = ms();
                        r
                    }
                    Err(e) => ToolReply::err(format!("原语 {primitive} 失败：{e}"), ms()),
                }
            }
        }
    }

    /// 执行 cwd：只允许群组目录（相对路径不越界）。
    fn exec_cwd(&self) -> Option<PathBuf> {
        Some(self.base_dir.clone())
    }

    /// 把相对路径解析到群组目录内，拒绝 `..` 与绝对路径（防止树包读任意文件）。
    fn resolve_inside(&self, rel: &str) -> Result<PathBuf, String> {
        let r = rel.trim();
        if r.is_empty() {
            return Err("脚本路径为空".into());
        }
        let p = Path::new(r);
        if p.is_absolute() || r.contains("..") {
            return Err(format!("脚本路径非法（必须位于群组目录内）：{r}"));
        }
        Ok(self.base_dir.join(p))
    }
}

/// 找 Python 解释器。解析顺序（前者优先，便于复现）：
/// 1. 环境变量 `WINDBOT_PYTHON`（显式指定，写错就直接报错，不静默回退）；
/// 2. exe 同级自带解释器（`python/python.exe` == 便携部署的主路径）；
/// 3. PATH 上的 `python.exe` / `python3.exe`（开发机与已装 Python 的用户）。
///
/// 全都找不到 ⇒ python 路由不可用，工具报错而不是假装成功。
pub fn find_python() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WINDBOT_PYTHON") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    if let Some(root) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let bundled = [
            root.join("python").join("python.exe"),
            root.join("python-3.15-embed").join("python.exe"),
            root.join("python").join("python3.exe"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        if bundled.is_some() {
            return bundled;
        }
    }
    let names: &[&str] = if cfg!(windows) { &["python.exe", "python3.exe"] } else { &["python3", "python"] };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

/// 从包清单构建全部工具。
pub fn build_tools(
    tools: &[ToolDecl],
    base_dir: &Path,
    group: &str,
    invoker: Option<Arc<dyn PrimitiveInvoker>>,
) -> Vec<Arc<dyn ApiTool>> {
    tools
        .iter()
        .filter(|t| !t.name.trim().is_empty())
        .map(|t| {
            Arc::new(DeclarativeTool::new(
                t.clone(),
                base_dir.to_path_buf(),
                group.to_string(),
                invoker.clone(),
            )) as Arc<dyn ApiTool>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wpa_api::{HttpClient, HttpResponse};

    struct FakeHttp {
        seen: Arc<Mutex<Vec<HttpRequest>>>,
        status: u16,
        body: String,
    }

    #[async_trait]
    impl HttpClient for FakeHttp {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String> {
            self.seen.lock().unwrap().push(req);
            Ok(HttpResponse { status: self.status, headers: vec![], body: self.body.clone() })
        }
    }

    struct FakeRt {
        http: Arc<dyn HttpClient>,
    }

    #[async_trait]
    impl GroupRuntime for FakeRt {
        fn session_id(&self) -> &str {
            "s"
        }
        fn http(&self) -> Arc<dyn HttpClient> {
            self.http.clone()
        }
        fn emit(&self, _e: &str, _p: Value) {}
    }

    struct FakePrimitive;

    #[async_trait]
    impl PrimitiveInvoker for FakePrimitive {
        async fn invoke(&self, primitive: &str, args: Value) -> Result<ToolReply, String> {
            if primitive == "boom" {
                return Err("no".into());
            }
            Ok(ToolReply::json(
                format!("{primitive} ok"),
                serde_json::json!({ "primitive": primitive, "args": args }),
                0,
            ))
        }
    }

    fn http_tool(url: &str) -> DeclarativeTool {
        let decl = ToolDecl {
            name: "web.get".into(),
            description: "取页面".into(),
            risk: Some("low".into()),
            exec: ExecSpec::Http {
                method: "GET".into(),
                url: url.into(),
                headers: BTreeMap::new(),
                body: None,
                expect: vec![],
            },
            slots: BTreeMap::from([("host".to_string(), "scope.primary".to_string())]),
            parameters: Some(
                toml::from_str("type=\"object\"\nrequired=[\"host\"]\n[properties.host]\ntype=\"string\"").unwrap(),
            ),
            concurrency_safe: Some(true),
            timeout_secs: Some(5),
        };
        DeclarativeTool::new(decl, PathBuf::from("."), "tools.web".into(), None)
    }

    #[tokio::test]
    async fn http_tool_expands_slot_and_template() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tool = http_tool("http://{{host}}/index");
        // schema 已移除插槽字段
        let schema = tool.input_schema();
        assert!(schema["properties"].get("host").is_none());
        assert!(schema["required"].as_array().map(|a| a.is_empty()).unwrap_or(true));

        let rt: Arc<dyn GroupRuntime> = Arc::new(FakeRt {
            http: Arc::new(FakeHttp { seen: seen.clone(), status: 200, body: "ok".into() }),
        });
        let reply = tool
            .call(ToolRequest {
                session_id: "s".into(),
                authorized_scope: vec!["10.0.0.9".into()],
                denylist: vec![],
                args: serde_json::json!({}),
                runtime: rt,
            })
            .await;
        assert!(reply.ok, "{}", reply.summary);
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs[0].url, "http://10.0.0.9/index");
    }

    #[tokio::test]
    async fn missing_scope_refuses_to_run() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tool = http_tool("http://{{host}}/");
        let rt: Arc<dyn GroupRuntime> = Arc::new(FakeRt {
            http: Arc::new(FakeHttp { seen, status: 200, body: String::new() }),
        });
        let reply = tool
            .call(ToolRequest {
                session_id: "s".into(),
                authorized_scope: vec![],
                denylist: vec![],
                args: serde_json::json!({}),
                runtime: rt,
            })
            .await;
        assert!(!reply.ok);
        assert!(reply.summary.contains("插槽"), "{}", reply.summary);
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let tool = http_tool("file:///etc/passwd");
        let rt: Arc<dyn GroupRuntime> = Arc::new(FakeRt {
            http: Arc::new(FakeHttp { seen: Arc::new(Mutex::new(vec![])), status: 200, body: String::new() }),
        });
        let reply = tool
            .call(ToolRequest {
                session_id: "s".into(),
                authorized_scope: vec!["x".into()],
                denylist: vec![],
                args: serde_json::json!({}),
                runtime: rt,
            })
            .await;
        assert!(!reply.ok);
    }

    #[tokio::test]
    async fn primitive_tool_delegates() {
        let decl = ToolDecl {
            name: "scan.port".into(),
            description: "扫描".into(),
            risk: Some("medium".into()),
            exec: ExecSpec::Primitive { primitive: "net.scan".into(), passthrough: true },
            slots: BTreeMap::new(),
            parameters: None,
            concurrency_safe: Some(true),
            timeout_secs: None,
        };
        let tool = DeclarativeTool::new(
            decl,
            PathBuf::from("."),
            "tools.scan".into(),
            Some(Arc::new(FakePrimitive)),
        );
        let rt: Arc<dyn GroupRuntime> = Arc::new(FakeRt {
            http: Arc::new(FakeHttp { seen: Arc::new(Mutex::new(vec![])), status: 200, body: String::new() }),
        });
        let reply = tool
            .call(ToolRequest {
                session_id: "s".into(),
                authorized_scope: vec!["a".into()],
                denylist: vec![],
                args: serde_json::json!({"ports":"80"}),
                runtime: rt,
            })
            .await;
        assert!(reply.ok, "{}", reply.summary);
        assert_eq!(reply.detail.unwrap()["args"]["ports"], "80");
    }

    #[test]
    fn python_path_escape_rejected() {
        let decl = ToolDecl {
            name: "x.y".into(),
            description: String::new(),
            risk: None,
            exec: ExecSpec::Python { script: "../evil.py".into(), args: vec![] },
            slots: BTreeMap::new(),
            parameters: None,
            concurrency_safe: None,
            timeout_secs: None,
        };
        let tool = DeclarativeTool::new(decl, PathBuf::from("."), "g".into(), None);
        assert!(tool.resolve_inside("../evil.py").is_err());
        assert!(tool.resolve_inside("ok.py").is_ok());
    }

    #[test]
    fn unknown_risk_defaults_high() {
        let decl = ToolDecl {
            name: "x.y".into(),
            description: String::new(),
            risk: Some("whatever".into()),
            exec: ExecSpec::Primitive { primitive: "p".into(), passthrough: true },
            slots: BTreeMap::new(),
            parameters: None,
            concurrency_safe: None,
            timeout_secs: None,
        };
        let tool = DeclarativeTool::new(decl, PathBuf::from("."), "g".into(), None);
        assert_eq!(tool.declared_risk(), RiskTier::Critical);
        assert!(tool.declared_risk().needs_approval());
    }
}
