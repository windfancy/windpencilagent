//! 契约宿主：把 `wpa-api` 的群组契约接到基座执行链上。
//!
//! 这里是**唯一**知道"契约工具"与"基座工具"两种形态的地方：
//! - 树侧：`Arc<dyn ApiTool>`（只认 wpa-api）
//! - 基座侧：`Arc<dyn toolkit_core::tools::Tool>`（进 Registry / 执行器 / LLM 工具面）
//!
//! 适配器同时把基座已建好的共享运行时（HTTP / 事件 / 存储）注入给树，
//! 避免每个群组各建一套连接与句柄。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use base_llm::ToolSchema;
use kern_core::model::{ExecutionContext, RiskLevel};
use kern_core::Registry;
use toolkit_core::tools::{ResultKind, Tool, ToolResult};
use toolkit_core::ToolRegistryExt;

use wpa_api::{
    ApiTool, GroupPlugin, GroupRuntime, HttpClient, HttpRequest, HttpResponse, RiskTier, SemVer,
    TreeManifest, GroupRegistry,
};

// ---------------------------------------------------------------------------
// 风险档位映射
// ---------------------------------------------------------------------------

pub fn to_kern_risk(t: RiskTier) -> RiskLevel {
    match t {
        RiskTier::None => RiskLevel::None,
        RiskTier::Low => RiskLevel::Low,
        RiskTier::Medium => RiskLevel::Medium,
        RiskTier::High => RiskLevel::High,
        RiskTier::Critical => RiskLevel::Critical,
    }
}

/// 反向映射（工具面向 UI/审计展示时使用；主链路只用正向）。
#[allow(dead_code)]
pub fn from_kern_risk(r: RiskLevel) -> RiskTier {
    match r {
        RiskLevel::None => RiskTier::None,
        RiskLevel::Low => RiskTier::Low,
        RiskLevel::Medium => RiskTier::Medium,
        RiskLevel::High => RiskTier::High,
        RiskLevel::Critical => RiskTier::Critical,
    }
}

// ---------------------------------------------------------------------------
// 共享 HTTP 客户端（全局一个连接池）
// ---------------------------------------------------------------------------

/// 基座共享 HTTP 客户端：所有群组共用同一个 reqwest 连接池。
pub struct SharedHttp {
    client: reqwest::Client,
}

impl SharedHttp {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("windpencilagent/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for SharedHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for SharedHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.to_uppercase().as_bytes())
            .map_err(|e| format!("非法 HTTP 方法 {}: {e}", req.method))?;
        let mut rb = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if let Some(body) = req.body {
            rb = rb.body(body);
        }
        if let Some(ms) = req.timeout_ms {
            rb = rb.timeout(Duration::from_millis(ms));
        }
        let resp = rb.send().await.map_err(|e| format!("请求失败：{e}"))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = resp.text().await.map_err(|e| format!("读取响应失败：{e}"))?;
        Ok(HttpResponse { status, headers, body })
    }
}

// ---------------------------------------------------------------------------
// 基座侧运行时实现
// ---------------------------------------------------------------------------

/// 会话级运行时：共享 HTTP + 事件记录 + 命名空间 KV。
pub struct BaseRuntime {
    session_id: String,
    http: Arc<dyn HttpClient>,
    data_dir: Option<PathBuf>,
    events: Mutex<Vec<(String, Value)>>,
    kv: Mutex<HashMap<(String, String), Value>>,
}

impl BaseRuntime {
    pub fn new(session_id: impl Into<String>, data_dir: Option<PathBuf>) -> Self {
        Self {
            session_id: session_id.into(),
            http: Arc::new(SharedHttp::new()),
            data_dir,
            events: Mutex::new(Vec::new()),
            kv: Mutex::new(HashMap::new()),
        }
    }

    /// 已发出的事件快照（审计/调试用）。
    pub fn events(&self) -> Vec<(String, Value)> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl GroupRuntime for BaseRuntime {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn http(&self) -> Arc<dyn HttpClient> {
        self.http.clone()
    }

    fn emit(&self, event: &str, payload: Value) {
        tracing::info!(target: "windpencilagent::plugin", event, payload = %payload, "群组事件");
        if let Ok(mut ev) = self.events.lock() {
            ev.push((event.to_string(), payload));
            // 防膨胀：只留最近 512 条
            if ev.len() > 512 {
                ev.remove(0);
            }
        }
    }

    fn data_dir(&self) -> Option<PathBuf> {
        self.data_dir.clone()
    }

    async fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>, String> {
        Ok(self
            .kv
            .lock()
            .map_err(|_| "kv 锁中毒".to_string())?
            .get(&(ns.to_string(), key.to_string()))
            .cloned())
    }

    async fn kv_put(&self, ns: &str, key: &str, value: Value) -> Result<(), String> {
        let mut kv = self.kv.lock().map_err(|_| "kv 锁中毒".to_string())?;
        kv.insert((ns.to_string(), key.to_string()), value);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 契约工具 → 基座工具 适配器
// ---------------------------------------------------------------------------

/// 把树侧 `ApiTool` 包装成基座 `Tool`。
pub struct ApiToolAdapter {
    inner: Arc<dyn ApiTool>,
    runtime: Arc<dyn GroupRuntime>,
    /// 拒绝清单（来自配置/空间）：透传给树，树的工具必须尊重它。
    denylist: Arc<Vec<String>>,
}

impl ApiToolAdapter {
    pub fn new(
        inner: Arc<dyn ApiTool>,
        runtime: Arc<dyn GroupRuntime>,
        denylist: Arc<Vec<String>>,
    ) -> Self {
        Self { inner, runtime, denylist }
    }

    /// 被包装的契约工具（热卸载定位 / 外部审计用；当前主链路不读）。
    #[allow(dead_code)]
    pub fn inner(&self) -> &Arc<dyn ApiTool> {
        &self.inner
    }
}

#[async_trait]
impl Tool for ApiToolAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> String {
        self.inner.description()
    }

    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }

    fn risk(&self) -> RiskLevel {
        to_kern_risk(self.inner.risk())
    }

    fn risk_for(&self, args: &Value) -> RiskLevel {
        to_kern_risk(self.inner.risk_for(args))
    }

    fn is_concurrency_safe(&self, args: &Value) -> bool {
        self.inner.concurrency_safe(args)
    }

    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult {
        // 授权范围必须取**会话级**清单：`ctx.target` 只是"这一次打哪个"，
        // 拿它当范围会让多目标会话里的插件只能看到第一个目标（甚至空串），
        // 直接退化成 fail-closed。target 只在会话没声明范围时兜底。
        let scope = if ctx.scope.is_empty() {
            vec![ctx.target.clone()]
        } else {
            ctx.scope.clone()
        };
        let req = wpa_api::ToolRequest {
            session_id: ctx.session_id.to_string(),
            authorized_scope: scope,
            denylist: self.denylist.as_ref().clone(),
            args,
            runtime: self.runtime.clone(),
        };
        let reply = self.inner.call(req).await;
        if reply.ok {
            match reply.detail {
                Some(detail) => ToolResult::json(reply.summary, detail, reply.duration_ms),
                None => ToolResult::text(reply.summary, reply.duration_ms),
            }
        } else {
            let mut r = ToolResult::error(reply.summary, reply.duration_ms);
            r.kind = ResultKind::Error;
            r
        }
    }
}

// ---------------------------------------------------------------------------
// 注册表同步与 schema 收集
// ---------------------------------------------------------------------------

/// 把契约注册表里已挂载的工具同步进内核 Registry。
///
/// 只同步已挂载工具（隔离区工具默认不进 LLM 工具面——fail-closed）。
pub fn sync_registry(
    kern: &Registry,
    api: &GroupRegistry,
    runtime: Arc<dyn GroupRuntime>,
    denylist: Arc<Vec<String>>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for name in api.tool_names() {
        let Some(tool) = api.tool(&name) else { continue };
        let adapter = Arc::new(ApiToolAdapter::new(tool, runtime.clone(), denylist.clone()));
        if let Err(e) = kern.register_tool(adapter) {
            warnings.push(format!("工具 {name} 注册失败：{e}"));
        }
    }
    warnings
}

/// 从内核 Registry 收集 LLM 工具面与风险表。
pub fn collect_tool_schemas(kern: &Registry) -> (Vec<ToolSchema>, HashMap<String, RiskLevel>) {
    let mut schemas = Vec::new();
    let mut risks = HashMap::new();
    for name in kern.tools() {
        if let Some(t) = kern.tool(&name) {
            schemas.push(ToolSchema {
                name: t.name().to_string(),
                description: t.description(),
                parameters: t.input_schema(),
            });
            risks.insert(name, t.risk());
        }
    }
    schemas.sort_by(|a, b| a.name.cmp(&b.name));
    (schemas, risks)
}

// ---------------------------------------------------------------------------
// 树装配
// ---------------------------------------------------------------------------

/// 装配结果（供 CLI 输出与审计）。
#[derive(Debug, Default)]
pub struct TreeReport {
    pub tree_name: String,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub order: Vec<String>,
    pub registered: Vec<String>,
    pub missing_impls: Vec<String>,
    /// 由逻辑群组声明的阶段（首个非空者；逻辑插口单活）。
    pub phases: Vec<wpa_api::Phase>,
    /// 从目录按清单装配的声明式群组（无需编译）。
    pub declarative: Vec<String>,
}

impl TreeReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// 本产物链接的群组实现。
///
/// **基座产物恒为空**：基座不认识任何具体群组。专业装配壳（`assembly/*-app`）
/// 会把自己的群组实现传进 [`assemble_tree`]。
pub fn linked_groups() -> Vec<Arc<dyn GroupPlugin>> {
    Vec::new()
}

/// 按清单装配群组（清单驱动，无文件监测）。
///
/// 流程：版本 preflight → 依赖拓扑 → 逐组装配（只装启用组）。
///
/// 两条装配来源：
/// 1. **链接实现**（`linked`）：原生群组，编译进当前可执行文件；
/// 2. **声明式目录**（`base_dir` + `GroupRef.path`）：从 `PnC.toml` 现场构建，装树不编译。
pub async fn assemble_tree(
    api: &mut GroupRegistry,
    tree: &TreeManifest,
    linked: Vec<Arc<dyn GroupPlugin>>,
    base_version: &SemVer,
) -> TreeReport {
    assemble_tree_with(api, tree, None, linked, None, base_version).await
}

/// 带声明式目录与原语注入的装配（M4/M5 主路径）。
pub async fn assemble_tree_with(
    api: &mut GroupRegistry,
    tree: &TreeManifest,
    base_dir: Option<&std::path::Path>,
    linked: Vec<Arc<dyn GroupPlugin>>,
    invoker: Option<Arc<dyn tree_core::exec::PrimitiveInvoker>>,
    base_version: &SemVer,
) -> TreeReport {
    let report_validation = tree.validate(base_version);
    let mut report = TreeReport {
        tree_name: tree.name.clone(),
        warnings: report_validation.warnings.clone(),
        order: report_validation.order.clone(),
        ..Default::default()
    };
    if !report_validation.is_ok() {
        report.errors = report_validation.errors;
        return report;
    }

    for group_ref in tree.enabled_groups() {
        // 1) 链接实现优先（原生群组，能力上限更高）
        if let Some(plugin) = linked.iter().find(|g| g.manifest().id == group_ref.id) {
            let manifest = plugin.manifest();
            if !manifest.version_ok(base_version) {
                report.errors.push(format!(
                    "群组 {} 要求 windpencilagent >= {:?}（当前 {base_version}）",
                    group_ref.id, manifest.min_base_version
                ));
                continue;
            }
            if let Err(e) = api.note_group(manifest) {
                report.errors.push(format!("群组 {} 登记失败：{e}", group_ref.id));
                continue;
            }
            match plugin.register(api).await {
                Ok(()) => report.registered.push(group_ref.id.clone()),
                Err(e) => report.errors.push(format!("群组 {} 装配失败：{e}", group_ref.id)),
            }
            continue;
        }

        // 2) 声明式目录（`path`）
        let src = group_ref.source();
        if !src.starts_with("builtin:") && !src.trim().is_empty() {
            let Some(base) = base_dir else {
                report.missing_impls.push(group_ref.id.clone());
                report.warnings.push(format!(
                    "群组 {} 声明为目录 `{src}`，但未提供树包根目录（已跳过）",
                    group_ref.id
                ));
                continue;
            };
            let dir = base.join(src);
            match tree_core::DeclarativeGroup::from_dir(&dir, invoker.clone()) {
                Ok(g) => {
                    if g.manifest().kind != group_ref.kind {
                        report.warnings.push(format!(
                            "群组 {} 在 tree.toml 声明为 {}，PnC.toml 写的是 {}（以 PnC 为准）",
                            group_ref.id,
                            group_ref.kind.as_str(),
                            g.manifest().kind.as_str()
                        ));
                    }
                    let manifest = g.manifest();
                    if let Err(e) = api.note_group(manifest) {
                        report.errors.push(format!("群组 {} 登记失败：{e}", group_ref.id));
                        continue;
                    }
                    match g.register(api).await {
                        Ok(()) => {
                            report.registered.push(group_ref.id.clone());
                            report.declarative.push(group_ref.id.clone());
                            for w in g.warnings() {
                                report.warnings.push(format!("[{}] {w}", group_ref.id));
                            }
                            if report.phases.is_empty() {
                                let phases = g.phases();
                                if !phases.is_empty() {
                                    report.phases = phases;
                                }
                            }
                        }
                        Err(e) => {
                            report.errors.push(format!("群组 {} 装配失败：{e}", group_ref.id));
                        }
                    }
                }
                Err(e) => report.errors.push(format!("群组 {} 加载失败：{e}", group_ref.id)),
            }
            continue;
        }

        // 3) 既没有链接实现也没有可用目录
        report.missing_impls.push(group_ref.id.clone());
        report.warnings.push(format!(
            "群组 {} 在清单中启用，但本可执行文件未链接其实现、也没有声明式目录（已跳过）",
            group_ref.id
        ));
    }
    report
}

/// 反拓扑序卸载（先卸依赖方，再卸被依赖方）。
pub async fn shutdown_groups(linked: &[Arc<dyn GroupPlugin>], order: &[String]) {
    for id in order.iter().rev() {
        if let Some(g) = linked.iter().find(|g| g.manifest().id == *id) {
            if let Err(e) = g.shutdown().await {
                tracing::warn!(group = %id, error = %e, "群组卸载失败");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kern_core::model::{AgentId, SessionId, TaskId};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wpa_api::{GroupManifest, GroupKind, ToolReply, ToolRequest};

    struct FakeTool {
        name: String,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ApiTool for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> String {
            "fake".into()
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object","properties":{"x":{"type":"string"}}})
        }
        fn risk(&self) -> RiskTier {
            RiskTier::Medium
        }
        async fn call(&self, req: ToolRequest) -> ToolReply {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // 验证运行时注入可用
            let _ = req.runtime.http();
            req.runtime.emit("fake.called", json!({"x": req.args["x"]}));
            ToolReply::json("ok", json!({"echo": req.args["x"]}), 3)
        }
    }

    struct FakeGroup {
        id: String,
        tools: Vec<Arc<dyn ApiTool>>,
    }

    #[async_trait]
    impl GroupPlugin for FakeGroup {
        fn manifest(&self) -> GroupManifest {
            GroupManifest::new(&self.id, "fake", "1.0.0", GroupKind::Tools)
        }
        async fn register(&self, reg: &mut GroupRegistry) -> Result<(), String> {
            for t in &self.tools {
                reg.register_tool_in(t.clone(), Some(&self.id))?;
            }
            Ok(())
        }
    }

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("a"),
            task_id: TaskId::new("t"),
            target: "127.0.0.1".into(),
            risk: RiskLevel::Low,
            approved: true,
        }
    }

    #[tokio::test]
    async fn api_tool_runs_through_kern_registry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut api = GroupRegistry::new();
        api.register_tool(Arc::new(FakeTool { name: "demo.echo".into(), calls: calls.clone() }))
            .unwrap();

        let rt: Arc<dyn GroupRuntime> = Arc::new(BaseRuntime::new("s1", None));
        let kern = Registry::new();
        let warns = sync_registry(&kern, &api, rt, Arc::new(Vec::new()));
        assert!(warns.is_empty(), "{warns:?}");
        assert_eq!(kern.tools(), vec!["demo.echo".to_string()]);

        let tool = kern.tool("demo.echo").expect("已注册");
        let out = tool.call(&ctx(), json!({"x":"hi"})).await;
        assert!(out.ok);
        assert_eq!(out.duration_ms, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tree_assembly_registers_linked_group_and_reports_missing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let group: Arc<dyn GroupPlugin> = Arc::new(FakeGroup {
            id: "tools.demo".into(),
            tools: vec![Arc::new(FakeTool { name: "demo.echo".into(), calls })],
        });

        let tree_json = r#"{
          "name": "demo",
          "version": "1.0.0",
          "min_base_version": "0.1.0",
          "groups": [
            {"id": "tools.demo", "kind": "tools", "plugin": "builtin:demo"},
            {"id": "logic.plan", "kind": "logic", "plugin": "builtin:plan"}
          ]
        }"#;
        let tree = TreeManifest::from_json(tree_json).unwrap();
        let mut api = GroupRegistry::new();
        let report = assemble_tree(&mut api, &tree, vec![group], &SemVer::parse("0.1.0").unwrap()).await;

        assert!(report.is_ok(), "{:?}", report.errors);
        assert_eq!(report.registered, vec!["tools.demo"]);
        assert_eq!(report.missing_impls, vec!["logic.plan"]);
        assert_eq!(api.tool_names(), vec!["demo.echo"]);
        assert_eq!(api.groups().len(), 1);
    }

    #[tokio::test]
    async fn tree_assembly_stops_on_version_mismatch() {
        let tree = TreeManifest::from_json(
            r#"{"name":"t","version":"1.0.0","min_base_version":"9.0.0","groups":[]}"#,
        )
        .unwrap();
        let mut api = GroupRegistry::new();
        let report = assemble_tree(&mut api, &tree, vec![], &SemVer::parse("0.1.0").unwrap()).await;
        assert!(!report.is_ok());
        assert!(api.is_empty(), "校验失败不得装配任何群组");
    }

    #[test]
    fn risk_mapping_roundtrip() {
        for t in [RiskTier::None, RiskTier::Low, RiskTier::Medium, RiskTier::High, RiskTier::Critical] {
            assert_eq!(from_kern_risk(to_kern_risk(t)), t);
        }
    }
}
