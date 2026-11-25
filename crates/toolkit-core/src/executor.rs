//! ToolkitExecutor：kern-core ToolExecutor 契约的实现。
//!
//! 按 tool_hint 顺序在 Registry 中解析第一个可用工具并执行，
//! 把 ToolResult 折叠为内核层的 ToolOutcome（事实抽取 + 截断回显）。

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use kern_core::exec::{ToolExecutor, ToolOutcome};
use kern_core::model::{ExecutionContext, FactEntry, RiskLevel};

use crate::registry_ext::ToolRegistryExt;
use kern_core::Registry;

/// 摘要最大长度（给 LLM / 日志的回显截断）。
const SUMMARY_MAX: usize = 512;

pub struct ToolkitExecutor {
    registry: Arc<Registry>,
    /// Profile 装配白名单（dsh 语义）：Some(清单) 时仅清单内工具可解析执行；None = 全部。
    allow: Option<Vec<String>>,
}

impl ToolkitExecutor {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry, allow: None }
    }

    /// 按工作空间 plugins 清单装配（白名单过滤）。
    pub fn with_allow(registry: Arc<Registry>, allow: Vec<String>) -> Self {
        Self { registry, allow: Some(allow) }
    }

    fn allowed(&self, tool: &str) -> bool {
        match &self.allow {
            None => true,
            Some(list) => list.iter().any(|x| x == tool),
        }
    }

    /// 从工具结构化结果抽取黑板事实（**通用约定**，不按工具名硬编码语义）。
    ///
    /// 约定：工具可在 `detail` 里返回
    /// `facts: [{entity, attribute, value, confidence?}]`，基座统一投射到黑板。
    /// 具体"什么算事实"属于业务知识，由专业智能树的工具自行声明。
    fn extract_facts(_tool: &str, ctx: &ExecutionContext, detail: &Value) -> Vec<FactEntry> {
        let Some(arr) = detail.get("facts").and_then(Value::as_array) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|f| {
                let entity = f.get("entity")?.as_str()?.to_string();
                let attribute = f.get("attribute")?.as_str()?.to_string();
                if entity.is_empty() || attribute.is_empty() {
                    return None;
                }
                Some(FactEntry {
                    entity,
                    attribute,
                    value: f.get("value").cloned().unwrap_or(Value::Null),
                    confidence: f.get("confidence").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                    source_task: Some(ctx.task_id.clone()),
                })
            })
            .take(64)
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for ToolkitExecutor {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        tool_hint: &[String],
        args: Value,
    ) -> ToolOutcome {
        let start = Instant::now();

        // 按 hint 顺序解析第一个可用工具
        let mut resolved: Option<Arc<dyn crate::tools::Tool>> = None;
        let mut used = String::new();
        for hint in tool_hint {
            if !self.allowed(hint) {
                continue; // Profile 白名单外：跳过（等同未注册）
            }
            if let Some(t) = self.registry.tool(hint) {
                resolved = Some(t);
                used = hint.clone();
                break;
            }
        }
        let tool = match resolved {
            Some(t) => t,
            None => {
                return ToolOutcome {
                    ok: false,
                    tool_name: tool_hint.first().cloned().unwrap_or_default(),
                    summary: format!("无可解析工具（hints: {tool_hint:?}）"),
                    facts: vec![],
                    detail: None,
                    duration_ms: 0,
                }
            }
        };

        // 工具入参：任务级 target 优先，其余透传
        let mut tool_args = args.clone();
        if tool_args.get("target").is_none()
            && tool_args.get("url").is_none()
            && tool_args.get("host").is_none()
        {
            tool_args["target"] = serde_json::json!(ctx.target);
            // http.* 工具的入参名是 url（工具内部会补 http:// 前缀）
            if used.starts_with("http.") {
                tool_args["url"] = serde_json::json!(ctx.target);
            }
        }

        let result = tool.call(ctx, tool_args).await;
        let ms = start.elapsed().as_millis() as u64;
        let mut summary = result.summary;
        if summary.chars().count() > SUMMARY_MAX {
            summary = summary.chars().take(SUMMARY_MAX).collect::<String>() + "…";
        }

        let facts = result
            .detail
            .as_ref()
            .map(|d| Self::extract_facts(used.as_str(), ctx, d))
            .unwrap_or_default();

        ToolOutcome {
            ok: result.ok,
            tool_name: used,
            summary,
            facts,
            detail: result.detail,
            duration_ms: result.duration_ms.max(ms),
        }
    }

    fn available_tools(&self) -> Vec<String> {
        self.registry.tools().into_iter().filter(|t| self.allowed(t)).collect()
    }

    /// 参数级风险判定（R1-1，dsh 对齐）：解析工具后调用其 `risk_for(args)`；
    /// 白名单外按 Critical（fail-closed，必然进审批）。
    fn risk_for(&self, tool: &str, args: &Value) -> Option<RiskLevel> {
        if !self.allowed(tool) {
            return Some(RiskLevel::Critical);
        }
        self.registry.tool(tool).map(|t| t.risk_for(args))
    }

    /// 并发安全查询（R4-1）：解析工具后调用其 `is_concurrency_safe(args)`；
    /// 白名单外一律 false（fail-closed 串行）。
    fn concurrency_safe(&self, tool: &str, args: &Value) -> bool {
        if !self.allowed(tool) {
            return false;
        }
        self.registry.tool(tool).is_some_and(|t| t.is_concurrency_safe(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use kern_core::model::{AgentId, RiskLevel, SessionId, TaskId};

    struct DummyTool {
        n: &'static str,
    }
    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str { self.n }
        fn description(&self) -> String { "dummy".into() }
        fn input_schema(&self) -> Value { json!({}) }
        fn risk(&self) -> RiskLevel { RiskLevel::Low }
        async fn call(&self, _ctx: &ExecutionContext, _args: Value) -> ToolResult {
            ToolResult::text("dummy 完成", 1)
        }
    }

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("a"),
            task_id: TaskId::new("t"),
            target: String::new(),
            risk: RiskLevel::Low,
            approved: true,
        }
    }

    fn registry() -> Arc<Registry> {
        let r = Registry::new();
        r.register_tool(Arc::new(DummyTool { n: "alpha" })).unwrap();
        r.register_tool(Arc::new(DummyTool { n: "beta" })).unwrap();
        Arc::new(r)
    }

    #[test]
    fn extract_generic_facts() {
        let detail = json!({
            "facts": [
                {"entity": "host-a", "attribute": "role", "value": "gateway", "confidence": 0.9},
                {"entity": "", "attribute": "ignored", "value": 1},
                {"entity": "host-b", "attribute": "os", "value": "linux"}
            ]
        });
        let c = ctx();
        let facts = ToolkitExecutor::extract_facts("any.tool", &c, &detail);
        assert_eq!(facts.len(), 2, "空 entity 应被跳过");
        assert_eq!(facts[0].entity, "host-a");
        assert_eq!(facts[0].attribute, "role");
        assert_eq!(facts[0].value, json!("gateway"));
        assert!((facts[0].confidence - 0.9).abs() < 1e-6);
        assert_eq!(facts[1].entity, "host-b");
        assert!(facts[0].source_task.is_some());
    }

    #[test]
    fn extract_facts_without_convention_yields_none() {
        // 未按约定返回 facts 的工具：基座不做猜测
        let detail = json!({"results": [{"id": "x"}]});
        assert!(ToolkitExecutor::extract_facts("any.tool", &ctx(), &detail).is_empty());
    }

    #[tokio::test]
    async fn allowlist_filters_resolution_and_listing() {
        let reg = registry();
        // 全量：两个工具可见
        let full = ToolkitExecutor::new(reg.clone());
        let mut t = full.available_tools();
        t.sort();
        assert_eq!(t, vec!["alpha", "beta"]);
        // 白名单只含 alpha：beta 解析被拒、列表过滤
        let filtered = ToolkitExecutor::with_allow(reg.clone(), vec!["alpha".into()]);
        let mut t = filtered.available_tools();
        t.sort();
        assert_eq!(t, vec!["alpha"]);
        let out = filtered.execute(&ctx(), &["beta".to_string()], json!({})).await;
        assert!(!out.ok, "白名单外工具不得执行");
        assert!(out.summary.contains("无可解析工具"));
        let out = filtered.execute(&ctx(), &["alpha".to_string()], json!({})).await;
        assert!(out.ok);
        assert_eq!(out.tool_name, "alpha");
    }
}
