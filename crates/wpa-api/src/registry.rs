//! 装配注册表：基座暴露给群组的**唯一装配点**。
//!
//! 群组把能力登记到这里；基座再从注册表生成 LLM 工具面与执行链。
//! 基座在编译期完全不知道树的存在，运行时也只知道"某群组登记了什么"。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::group::{ApiTool, KnowledgeProvider, Planner, SkillPack};
use crate::manifest::{GroupKind, GroupManifest, RiskTier};

// ---------------------------------------------------------------------------
// 能力标签（受控词表）
// ---------------------------------------------------------------------------

/// 能力标签：规划器与领域配置描述"要做什么"的**唯一合法词汇**。
///
/// # 为什么必须有这一层
///
/// 工具名是注册表的私有知识——插件可安装、可卸载、可被同能力的另一个实现
/// 替换。规划器或领域配置一旦写死工具名，就会出现这种静默失效：
///
/// ```text
/// 规划器:  tool_hint = ["scan.port"]      ← 写死
/// 注册表:  实际叫 "net.scan"              ← 改名后
/// 过滤:    keep() 把不存在的名字滤掉       ← 不报错
/// 结果:    计划照常产出，任务没有工具提示，失败也查不出原因
/// ```
///
/// 能力标签把"要什么"与"用哪个"解耦：规划器写 `port_scan`，装配期由
/// [`GroupRegistry::resolve_needs`] 查表翻译成真实工具名。解析不到是
/// **硬错误**（[`ResolveError::Unknown`]），不是静默过滤。
///
/// # 这不是白名单
///
/// [`KNOWN`] 是**建议词表**，只用于在扫描报告里提示拼写漂移
/// （`portscan` / `scan_port` / `port_scan` 三种写法会让解析永远查不到）。
/// 新增标签不阻塞插件开发，但请先看 [`KNOWN`] 里有没有同义的——
/// 同义标签分裂会让"一个能力多个实现可互换"这个目标落空。
pub mod caps {
    // ---- 侦察 / 资产 ----
    /// 域名解析（A/AAAA/CNAME/MX/TXT）。
    pub const DNS_RESOLVE: &str = "dns_resolve";
    /// 端口扫描（TCP connect 或 SYN）。
    pub const PORT_SCAN: &str = "port_scan";
    /// 服务与版本指纹（banner / 响应头 / 特征库）。
    pub const SERVICE_FINGERPRINT: &str = "service_fingerprint";
    /// 操作系统识别。
    pub const OS_DETECT: &str = "os_detect";
    /// 子域枚举。
    pub const SUBDOMAIN_ENUM: &str = "subdomain_enum";
    /// 综合资产发现（把域名/网段展开成主机+端口+服务）。
    pub const ASSET_DISCOVER: &str = "asset_discover";
    /// 站点爬取与目录发现。
    pub const WEB_CRAWL: &str = "web_crawl";
    /// 取页面标题（批量存活判定最廉价的手段）。
    pub const TITLE_FETCH: &str = "title_fetch";

    // ---- HTTP ----
    /// 单次 HTTP 请求（无状态）。
    pub const HTTP_FETCH: &str = "http_fetch";
    /// 带会话的 HTTP（Cookie / 登录态保持）。
    pub const HTTP_SESSION: &str = "http_session";
    /// 表单提交。
    pub const FORM_SUBMIT: &str = "form_submit";
    /// 读响应头（安全头、Server、版本）。
    pub const HEADER_READ: &str = "header_read";
    /// 读 Cookie 集合。
    pub const COOKIE_READ: &str = "cookie_read";

    // ---- 验证 ----
    /// 两响应差异比对（盲注/越权/异常的通用判据）。
    pub const RESPONSE_DIFF: &str = "response_diff";
    /// 反射探测（XSS/头注入的"可注入"事实，不声称能执行）。
    pub const REFLECTION_PROBE: &str = "reflection_probe";
    /// 版本 → 已知漏洞映射。
    pub const CVE_MATCH: &str = "cve_match";
    /// 弱口令/默认口令测试。
    pub const WEAK_PASSWORD_TEST: &str = "weak_password_test";
    /// 未授权访问检查（无凭据直接可用性）。
    pub const UNAUTH_CHECK: &str = "unauth_check";
    /// 证书检查（有效期/自签/弱签名/SAN）。
    pub const TLS_INSPECT: &str = "tls_inspect";

    // ---- 抽取 / 报告 ----
    /// 从文本中抽 URL/域名/IP/邮箱/哈希/CVE 等线索。
    pub const TEXT_EXTRACT: &str = "text_extract";
    /// 证据记录（链式哈希、原文件落盘）。
    pub const EVIDENCE_RECORD: &str = "evidence_record";
    /// 证据链校验。
    pub const EVIDENCE_VERIFY: &str = "evidence_verify";
    /// 报告渲染。
    pub const REPORT_RENDER: &str = "report_render";

    // ---- 基座通用 ----
    pub const FILE_READ: &str = "file_read";
    pub const FILE_WRITE: &str = "file_write";
    pub const FILE_LIST: &str = "file_list";
    pub const FILE_DELETE: &str = "file_delete";
    pub const SHELL_EXEC: &str = "shell_exec";
    pub const KB_SEARCH: &str = "kb_search";
    pub const HASH_COMPUTE: &str = "hash_compute";
    pub const CODEC: &str = "codec";

    /// 建议词表（**非白名单**；只用于诊断拼写漂移）。
    pub const KNOWN: &[&str] = &[
        DNS_RESOLVE, PORT_SCAN, SERVICE_FINGERPRINT, OS_DETECT, SUBDOMAIN_ENUM,
        ASSET_DISCOVER, WEB_CRAWL, TITLE_FETCH,
        HTTP_FETCH, HTTP_SESSION, FORM_SUBMIT, HEADER_READ, COOKIE_READ,
        RESPONSE_DIFF, REFLECTION_PROBE, CVE_MATCH, WEAK_PASSWORD_TEST,
        UNAUTH_CHECK, TLS_INSPECT,
        TEXT_EXTRACT, EVIDENCE_RECORD, EVIDENCE_VERIFY, REPORT_RENDER,
        FILE_READ, FILE_WRITE, FILE_LIST, FILE_DELETE, SHELL_EXEC,
        KB_SEARCH, HASH_COMPUTE, CODEC,
    ];

    /// 该标签是否在建议词表内。
    pub fn is_known(tag: &str) -> bool {
        KNOWN.contains(&tag)
    }
}

/// 能力解析失败。**fail-closed：不做前缀匹配、不做模糊匹配、不猜。**
#[derive(Debug, Clone, thiserror::Error)]
pub enum ResolveError {
    #[error("没有任何已挂载工具声明能力 `{0}`——检查该组是否 enabled、插件 capabilities 是否拼写一致")]
    Unknown(String),
    #[error("能力需求为空")]
    Empty,
}

/// 注册失败原因（fail-closed：冲突即拒绝，不做"后者覆盖"）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum RegisterError {
    #[error("工具重名（已存在同名工具）：{0}")]
    Duplicate(String),
    #[error("工具名为空")]
    EmptyName,
    #[error("群组重名：{0}")]
    DuplicateGroup(String),
}

impl From<RegisterError> for String {
    fn from(e: RegisterError) -> Self {
        e.to_string()
    }
}

/// 工具的轻量 schema（供 LLM function-calling 与 UI 展示）。
#[derive(Debug, Clone)]
pub struct ToolSchemaLite {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub risk: RiskTier,
    /// 提供该工具的群组 id（审计与上下文收敛用；隔离区为 `None`）。
    pub group: Option<String>,
    /// 能力标签（规划器据此要工具，不直接要工具名）。
    pub capabilities: Vec<String>,
}

/// 装配注册表。
#[derive(Default)]
pub struct GroupRegistry {
    tools: BTreeMap<String, Arc<dyn ApiTool>>,
    tool_group: BTreeMap<String, String>,
    /// 未声明归属组的工具：隔离区，**默认不挂载**给 LLM。
    isolated: BTreeMap<String, Arc<dyn ApiTool>>,
    providers: Vec<Arc<dyn KnowledgeProvider>>,
    planners: Vec<Arc<dyn Planner>>,
    skills: BTreeMap<String, SkillPack>,
    persona: Option<String>,
    prompt_sections: Vec<String>,
    groups: Vec<GroupManifest>,
}

impl GroupRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- 工具 ----------------------------------------------------------

    /// 注册业务工具（重名拒绝）。
    pub fn register_tool(&mut self, tool: Arc<dyn ApiTool>) -> Result<(), RegisterError> {
        self.register_tool_in(tool, None)
    }

    /// 注册"已声明归属组"的工具（由群组装配时调用）。
    pub fn register_tool_in(
        &mut self,
        tool: Arc<dyn ApiTool>,
        group: Option<&str>,
    ) -> Result<(), RegisterError> {
        let name = tool.name().trim().to_string();
        if name.is_empty() {
            return Err(RegisterError::EmptyName);
        }
        if self.tools.contains_key(&name) || self.isolated.contains_key(&name) {
            return Err(RegisterError::Duplicate(name));
        }
        if let Some(g) = group {
            self.tool_group.insert(name.clone(), g.to_string());
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// 注册到隔离区（未声明组的工具，默认不进 LLM 工具面）。
    pub fn register_tool_isolated(&mut self, tool: Arc<dyn ApiTool>) -> Result<(), RegisterError> {
        let name = tool.name().trim().to_string();
        if name.is_empty() {
            return Err(RegisterError::EmptyName);
        }
        if self.tools.contains_key(&name) || self.isolated.contains_key(&name) {
            return Err(RegisterError::Duplicate(name));
        }
        self.isolated.insert(name, tool);
        Ok(())
    }

    /// 卸载工具（热注册/热卸载用；返回被移除的句柄）。
    pub fn unregister_tool(&mut self, name: &str) -> Option<Arc<dyn ApiTool>> {
        self.tool_group.remove(name);
        self.tools.remove(name)
    }

    pub fn tool(&self, name: &str) -> Option<Arc<dyn ApiTool>> {
        self.tools.get(name).cloned()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    // ---- 能力解析（规划器要能力，不要工具名）----------------------------

    /// 能力标签 → 提供该能力的工具名（字典序，保证同一份注册表结果稳定）。
    ///
    /// 只看已挂载工具；隔离区工具不参与——它们默认连 LLM 都不给看，
    /// 自然也不该被规划器指到。
    pub fn resolve_capability(&self, cap: &str) -> Result<Vec<String>, ResolveError> {
        let cap = cap.trim();
        if cap.is_empty() {
            return Err(ResolveError::Empty);
        }
        let hits: Vec<String> = self
            .tools
            .iter()
            .filter(|(_, t)| t.capabilities().iter().any(|c| c == cap))
            .map(|(name, _)| name.clone())
            .collect();
        if hits.is_empty() {
            return Err(ResolveError::Unknown(cap.to_string()));
        }
        Ok(hits)
    }

    /// 多个能力需求 → 工具名并集（去重保序）。
    ///
    /// **任一能力解析不到即整体失败**——部分解析会让计划看起来能跑，
    /// 实际缺少关键手段，比直接失败更难查。
    pub fn resolve_needs(&self, needs: &[String]) -> Result<Vec<String>, ResolveError> {
        if needs.is_empty() {
            return Err(ResolveError::Empty);
        }
        let mut out: Vec<String> = Vec::new();
        for cap in needs {
            for name in self.resolve_capability(cap)? {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        Ok(out)
    }

    /// 全量能力索引：`能力标签 → [工具名]`（诊断页 / UI 展示用）。
    pub fn capability_index(&self) -> BTreeMap<String, Vec<String>> {
        let mut idx: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, t) in &self.tools {
            for cap in t.capabilities() {
                idx.entry(cap).or_default().push(name.clone());
            }
        }
        for v in idx.values_mut() {
            v.sort();
        }
        idx
    }

    /// 声明了但不在建议词表内的能力标签（拼写漂移诊断；**不影响装配**）。
    pub fn unknown_capabilities(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .capability_index()
            .into_keys()
            .filter(|c| !caps::is_known(c))
            .collect();
        out.sort();
        out
    }

    /// 已挂载工具的 schema 列表（字典序，保证 LLM 工具面稳定）。
    pub fn tool_schemas(&self) -> Vec<ToolSchemaLite> {
        self.tools
            .iter()
            .map(|(name, t)| ToolSchemaLite {
                name: name.clone(),
                description: t.description(),
                parameters: t.input_schema(),
                risk: t.risk(),
                group: self.tool_group.get(name).cloned(),
                capabilities: t.capabilities(),
            })
            .collect()
    }

    /// 隔离区工具名（默认不挂载，需显式放行）。
    pub fn isolated_names(&self) -> Vec<String> {
        self.isolated.keys().cloned().collect()
    }

    /// 显式放行隔离区工具（人工确认后调用）。
    pub fn promote_isolated(&mut self, name: &str, group: &str) -> Result<(), RegisterError> {
        let Some(tool) = self.isolated.remove(name) else {
            return Ok(());
        };
        self.register_tool_in(tool, Some(group))
    }

    // ---- 知识 / 逻辑 / 技能 --------------------------------------------

    pub fn register_knowledge(&mut self, provider: Arc<dyn KnowledgeProvider>) {
        self.providers.push(provider);
    }

    /// 注册规划器；若其提供 prompt 片段则一并收集（基座统一并入 system prompt）。
    pub fn register_planner(&mut self, planner: Arc<dyn Planner>) {
        if let Some(section) = planner.prompt_section() {
            if !section.trim().is_empty() {
                self.prompt_sections.push(section);
            }
        }
        self.planners.push(planner);
    }

    pub fn register_skill(&mut self, pack: SkillPack) {
        if !pack.name.trim().is_empty() {
            self.skills.insert(pack.name.clone(), pack);
        }
    }

    // ---- 角色与 prompt -------------------------------------------------

    /// 设置领域角色（覆盖基座默认通用角色）。
    pub fn set_persona(&mut self, persona: impl Into<String>) {
        let p = persona.into();
        if !p.trim().is_empty() {
            self.persona = Some(p);
        }
    }

    pub fn add_prompt_section(&mut self, section: impl Into<String>) {
        let s = section.into();
        if !s.trim().is_empty() {
            self.prompt_sections.push(s);
        }
    }

    // ---- 群组清单（审计） ----------------------------------------------

    pub fn note_group(&mut self, manifest: GroupManifest) -> Result<(), RegisterError> {
        if self.groups.iter().any(|g| g.id == manifest.id) {
            return Err(RegisterError::DuplicateGroup(manifest.id));
        }
        self.groups.push(manifest);
        Ok(())
    }

    // ---- 只读访问 ------------------------------------------------------

    pub fn knowledge(&self) -> &[Arc<dyn KnowledgeProvider>] {
        &self.providers
    }

    pub fn planners(&self) -> &[Arc<dyn Planner>] {
        &self.planners
    }

    pub fn skills(&self) -> Vec<SkillPack> {
        self.skills.values().cloned().collect()
    }

    pub fn persona(&self) -> Option<&str> {
        self.persona.as_deref()
    }

    pub fn prompt_sections(&self) -> &[String] {
        &self.prompt_sections
    }

    pub fn groups(&self) -> &[GroupManifest] {
        &self.groups
    }

    pub fn groups_of(&self, kind: GroupKind) -> Vec<&GroupManifest> {
        self.groups.iter().filter(|g| g.kind == kind).collect()
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.isolated.is_empty()
            && self.providers.is_empty()
            && self.planners.is_empty()
            && self.skills.is_empty()
            && self.groups.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::ToolReply;
    use crate::manifest::RiskTier;
    use crate::runtime::{GroupRuntime, HttpClient, HttpRequest, HttpResponse};
    use async_trait::async_trait;
    use serde_json::json;

    struct DummyTool(&'static str);

    #[async_trait]
    impl ApiTool for DummyTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> String {
            "dummy".into()
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn risk(&self) -> RiskTier {
            RiskTier::Medium
        }
        async fn call(&self, _req: crate::group::ToolRequest) -> ToolReply {
            ToolReply::ok("ok", 1)
        }
    }

    struct DummyHttp;

    #[async_trait]
    impl HttpClient for DummyHttp {
        async fn send(&self, _req: HttpRequest) -> Result<HttpResponse, String> {
            Ok(HttpResponse { status: 200, headers: vec![], body: "ok".into() })
        }
    }

    struct DummyRt;

    #[async_trait]
    impl GroupRuntime for DummyRt {
        fn session_id(&self) -> &str {
            "s"
        }
        fn http(&self) -> Arc<dyn HttpClient> {
            Arc::new(DummyHttp)
        }
        fn emit(&self, _e: &str, _p: Value) {}
    }

    #[test]
    fn duplicate_tool_rejected() {
        let mut reg = GroupRegistry::new();
        reg.register_tool(Arc::new(DummyTool("a.b"))).unwrap();
        let err = reg.register_tool(Arc::new(DummyTool("a.b"))).unwrap_err();
        assert!(matches!(err, RegisterError::Duplicate(_)));
        assert_eq!(reg.tool_count(), 1);
    }

    #[test]
    fn isolated_tools_not_in_schema_until_promoted() {
        let mut reg = GroupRegistry::new();
        reg.register_tool_isolated(Arc::new(DummyTool("ghost"))).unwrap();
        assert!(reg.tool_schemas().is_empty());
        assert_eq!(reg.isolated_names(), vec!["ghost"]);
        reg.promote_isolated("ghost", "tools.demo").unwrap();
        assert_eq!(reg.tool_count(), 1);
        let s = &reg.tool_schemas()[0];
        assert_eq!(s.name, "ghost");
        assert_eq!(s.group.as_deref(), Some("tools.demo"));
    }

    #[test]
    fn unregister_frees_name() {
        let mut reg = GroupRegistry::new();
        reg.register_tool(Arc::new(DummyTool("x"))).unwrap();
        assert!(reg.unregister_tool("x").is_some());
        assert!(reg.register_tool(Arc::new(DummyTool("x"))).is_ok());
    }

    #[test]
    fn schema_sorted_by_name() {
        let mut reg = GroupRegistry::new();
        for n in ["z.tool", "a.tool", "m.tool"] {
            reg.register_tool(Arc::new(DummyTool(n))).unwrap();
        }
        let names: Vec<String> = reg.tool_schemas().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["a.tool", "m.tool", "z.tool"]);
    }

    #[test]
    fn empty_registry_is_empty() {
        let reg = GroupRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.tool_count(), 0);
        assert!(reg.persona().is_none());
    }

    #[tokio::test]
    async fn dummy_tool_runs() {
        let t = DummyTool("d");
        let rt: Arc<dyn GroupRuntime> = Arc::new(DummyRt);
        let r = t
            .call(crate::group::ToolRequest {
                session_id: "s".into(),
                authorized_scope: vec![],
                denylist: vec![],
                args: json!({}),
                runtime: rt,
            })
            .await;
        assert!(r.ok);
    }

    // ---- 能力解析：规划器要能力，不要工具名 -----------------------------

    struct CappedTool(&'static str, &'static [&'static str]);

    #[async_trait]
    impl ApiTool for CappedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> String {
            "capped".into()
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn capabilities(&self) -> Vec<String> {
            self.1.iter().map(|s| s.to_string()).collect()
        }
        async fn call(&self, _req: crate::group::ToolRequest) -> ToolReply {
            ToolReply::ok("ok", 1)
        }
    }

    fn build(tools: &[CappedTool]) -> GroupRegistry {
        let mut reg = GroupRegistry::new();
        for t in tools {
            let name = t.0;
            reg.register_tool_in(Arc::new(CappedTool(t.0, t.1)), Some("g"))
                .unwrap_or_else(|e| panic!("注册 {name} 失败：{e}"));
        }
        reg
    }

    #[test]
    fn resolve_capability_finds_every_provider() {
        let reg = build(&[
            CappedTool("net.scan", &["port_scan"]),
            CappedTool("asset.scan", &["port_scan", "asset_discover"]),
            CappedTool("http.get", &["http_fetch"]),
        ]);
        // 同一能力多个提供者都要返回（可互换的前提），且字典序稳定
        assert_eq!(
            reg.resolve_capability("port_scan").unwrap(),
            vec!["asset.scan".to_string(), "net.scan".to_string()]
        );
        assert_eq!(
            reg.resolve_capability("http_fetch").unwrap(),
            vec!["http.get".to_string()]
        );
    }

    /// fail-closed：解析不到是硬错误，**不是空列表**。
    /// 返回空列表会让上游以为"这步不需要工具"，问题被吞掉。
    #[test]
    fn unknown_capability_is_a_hard_error() {
        let reg = build(&[CappedTool("net.scan", &["port_scan"])]);
        match reg.resolve_capability("portscan") {
            Err(ResolveError::Unknown(c)) => assert_eq!(c, "portscan"),
            other => panic!("拼错的标签必须报错，实际：{other:?}"),
        }
        assert!(matches!(
            reg.resolve_capability("  "),
            Err(ResolveError::Empty)
        ));
    }

    /// 部分解析成功也不算成功——缺关键手段的计划比没有计划更难查。
    #[test]
    fn resolve_needs_is_all_or_nothing() {
        let reg = build(&[CappedTool("net.scan", &["port_scan"])]);
        assert_eq!(
            reg.resolve_needs(&["port_scan".to_string()]).unwrap(),
            vec!["net.scan".to_string()]
        );
        assert!(reg
            .resolve_needs(&["port_scan".to_string(), "dns_resolve".to_string()])
            .is_err());
        assert!(matches!(reg.resolve_needs(&[]), Err(ResolveError::Empty)));
    }

    #[test]
    fn resolve_needs_dedups_while_keeping_order() {
        let reg = build(&[CappedTool("net.scan", &["port_scan", "asset_discover"])]);
        let got = reg
            .resolve_needs(&["port_scan".to_string(), "asset_discover".to_string()])
            .unwrap();
        assert_eq!(got, vec!["net.scan".to_string()], "同一工具只出现一次");
    }

    /// 隔离区工具默认不给 LLM，也不该被规划器指到。
    #[test]
    fn isolated_tools_are_not_resolvable() {
        let mut reg = GroupRegistry::new();
        reg.register_tool_isolated(Arc::new(CappedTool("secret.tool", &["port_scan"])))
            .unwrap();
        assert!(matches!(
            reg.resolve_capability("port_scan"),
            Err(ResolveError::Unknown(_))
        ));
    }

    /// 拼写漂移要能被诊断出来（`portscan` vs `port_scan`），但不影响装配。
    #[test]
    fn spelling_drift_is_reported_not_fatal() {
        let reg = build(&[
            CappedTool("net.scan", &["port_scan"]),
            CappedTool("weird.scan", &["portscan"]),
        ]);
        assert_eq!(reg.unknown_capabilities(), vec!["portscan".to_string()]);
        let idx = reg.capability_index();
        assert_eq!(idx["port_scan"], vec!["net.scan".to_string()]);
        assert_eq!(idx["portscan"], vec!["weird.scan".to_string()]);
        assert!(caps::is_known("port_scan"));
        assert!(!caps::is_known("portscan"));
    }

    /// schema 列表必须带上能力标签，否则 UI 与诊断页拿不到。
    #[test]
    fn tool_schemas_carry_capabilities() {
        let reg = build(&[CappedTool("net.scan", &["port_scan"])]);
        let s = reg.tool_schemas();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].capabilities, vec!["port_scan".to_string()]);
    }
}
