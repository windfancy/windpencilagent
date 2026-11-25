//! 基座内建工具（通用，无业务语义）：知识检索、阶段推进、以及把树侧能力
//! 接到基座执行链上的两个适配器（原语调用器 / 约束钩子）。
//!
//! 为什么这些在基座：
//! - `kb.search` 是"检索机制"，不是"知识内容"——任何树装进来都能用；
//! - `plan.phase` 是"阶段机"的操作面，阶段定义由树提供；
//! - 原语调用器与约束钩子是**执行链的一部分**，属基础设施。

use std::sync::Arc;

use async_trait::async_trait;
use kern_core::model::{AgentId, ExecutionContext, RiskLevel, SessionId, TaskId};
use kern_core::Registry;
use serde_json::{json, Value};
use toolkit_core::hooks::{HookVerdict, ToolHook};
use toolkit_core::tools::{Tool, ToolResult};
use toolkit_core::ToolRegistryExt;

use wpa_api::{KnowledgeProvider, Phase, ToolReply};
use tree_core::exec::PrimitiveInvoker;
use tree_core::phases::SharedPhases;

// ---------------------------------------------------------------------------
// kb.search —— 跨提供者知识检索
// ---------------------------------------------------------------------------

/// 检索所有已装配的知识提供者。
pub struct KbSearch {
    providers: Vec<Arc<dyn KnowledgeProvider>>,
}

impl KbSearch {
    pub fn new(providers: Vec<Arc<dyn KnowledgeProvider>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl Tool for KbSearch {
    fn name(&self) -> &str {
        "kb.search"
    }

    fn description(&self) -> String {
        "在已装入的领域知识库中检索（精确 id / 标题 / 标签 / 正文分级匹配）。\
         不知道答案时先查知识，不要凭空推断。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "关键词、CVE 编号、技术名"},
                "limit": {"type": "integer", "description": "返回条数（默认 5，上限 20）"}
            },
            "required": ["query"]
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if query.is_empty() {
            return ToolResult::error("缺少 query", 0);
        }
        if self.providers.is_empty() {
            return ToolResult::json(
                "当前未装入任何知识库（基座模式）".to_string(),
                json!({"hits": [], "providers": 0}),
                0,
            );
        }
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5).clamp(1, 20) as usize;
        let mut hits = Vec::new();
        for p in &self.providers {
            for h in p.search(&query, limit).await {
                hits.push((p.name().to_string(), h));
            }
        }
        hits.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit);
        if hits.is_empty() {
            return ToolResult::json(
                format!("知识库里没有与「{query}」相关的内容"),
                json!({"hits": [], "providers": self.providers.len()}),
                0,
            );
        }
        let lines: Vec<String> = hits
            .iter()
            .map(|(src, h)| format!("[{}] {}（{}）", h.id, h.title, src))
            .collect();
        ToolResult::json(
            format!("命中 {} 条：{}", hits.len(), lines.join("；")),
            json!({
                "providers": self.providers.iter().map(|p| p.name().to_string()).collect::<Vec<_>>(),
                "hits": hits.iter().map(|(src, h)| json!({
                    "id": h.id, "title": h.title, "body": h.body, "score": h.score, "provider": src
                })).collect::<Vec<_>>()
            }),
            0,
        )
    }
}

// ---------------------------------------------------------------------------
// plan.phase —— 阶段推进
// ---------------------------------------------------------------------------

/// 显示/切换当前作业阶段。
pub struct PlanPhase {
    phases: SharedPhases,
}

impl PlanPhase {
    pub fn new(phases: SharedPhases) -> Self {
        Self { phases }
    }
}

fn phase_rows(phases: &[Phase]) -> Value {
    json!(phases
        .iter()
        .map(|p| json!({
            "id": p.id,
            "label": p.label,
            "entry_hint": p.entry_hint,
            "exit_condition": p.exit_condition,
            "allowed_groups": p.allowed_groups,
        }))
        .collect::<Vec<_>>())
}

#[async_trait]
impl Tool for PlanPhase {
    fn name(&self) -> &str {
        "plan.phase"
    }

    fn description(&self) -> String {
        "查看或切换当前作业阶段。省略 phase 参数 = 查看当前阶段与全部可选阶段；\
         给出 id = 切到该阶段（切换后可用工具面会变化）。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "phase": {"type": "string", "description": "目标阶段 id（省略则只查看）"}
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let mut guard = match self.phases.lock() {
            Ok(g) => g,
            Err(_) => return ToolResult::error("阶段状态锁中毒", 0),
        };
        let all = phase_rows(guard.phases());
        match args.get("phase").and_then(Value::as_str).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            None => {
                let cur = guard.current();
                ToolResult::json(
                    match cur {
                        Some(p) => format!("当前阶段：{}（{}）", p.label, p.id),
                        None => "尚未进入任何阶段".to_string(),
                    },
                    json!({
                        "current": guard.current_id(),
                        "current_label": cur.map(|p| p.label.clone()),
                        "exit_condition": cur.map(|p| p.exit_condition.clone()).unwrap_or_default(),
                        "phases": all,
                    }),
                    0,
                )
            }
            Some(id) => {
                let from = guard.current_id().map(|s| s.to_string());
                match guard.advance(Some(id)) {
                    Ok(t) => {
                        let cur = guard.current();
                        ToolResult::json(
                            format!(
                                "阶段已切换：{} → {}（{}）；本阶段可用工具组：{}",
                                from.unwrap_or_else(|| "无".into()),
                                cur.map(|p| p.label.clone()).unwrap_or_default(),
                                t.to,
                                cur.map(|p| {
                                    if p.allowed_groups.is_empty() {
                                        "不限制".to_string()
                                    } else {
                                        p.allowed_groups.join("、")
                                    }
                                })
                                .unwrap_or_default()
                            ),
                            json!({
                                "from": t.from,
                                "current": t.to,
                                "label": cur.map(|p| p.label.clone()),
                                "exit_condition": cur.map(|p| p.exit_condition.clone()),
                                "allowed_groups": cur.map(|p| p.allowed_groups.clone()).unwrap_or_default(),
                                "phases": all,
                            }),
                            0,
                        )
                    }
                    Err(e) => ToolResult::error(e, 0),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 约束钩子适配器
// ---------------------------------------------------------------------------

/// 把知识约束集接成基座钩子（denylist 之后、审批之前裁决）。
pub struct ConstraintHook {
    set: Arc<tree_core::ConstraintSet>,
}

impl ConstraintHook {
    pub fn new(set: Arc<tree_core::ConstraintSet>) -> Self {
        Self { set }
    }
}

impl ToolHook for ConstraintHook {
    fn name(&self) -> &str {
        "knowledge-constraint"
    }

    fn on_tool_call(&self, tool: &str, risk: RiskLevel, args_summary: &str) -> HookVerdict {
        match self.set.check(tool, host_risk(risk), args_summary) {
            Some(reason) => HookVerdict::Deny(reason),
            None => HookVerdict::Allow,
        }
    }
}

fn host_risk(r: RiskLevel) -> wpa_api::RiskTier {
    match r {
        RiskLevel::None => wpa_api::RiskTier::None,
        RiskLevel::Low => wpa_api::RiskTier::Low,
        RiskLevel::Medium => wpa_api::RiskTier::Medium,
        RiskLevel::High => wpa_api::RiskTier::High,
        RiskLevel::Critical => wpa_api::RiskTier::Critical,
    }
}

// ---------------------------------------------------------------------------
// 原语调用器
// ---------------------------------------------------------------------------

/// 让声明式工具（`exec.kind = "primitive"`）能调到基座原语。
///
/// 注意：这条路**不是**绕过执行链——外层声明式工具已经过 ScopeGuard/约束/审批，
/// 这里只是把"业务命名"转发到"通用原语实现"。
pub struct KernPrimitiveInvoker {
    kern: Arc<Registry>,
    session_id: SessionId,
}

impl KernPrimitiveInvoker {
    pub fn new(kern: Arc<Registry>, session_id: SessionId) -> Self {
        Self { kern, session_id }
    }
}

fn target_from_args(args: &Value) -> String {
    ["target", "host", "url", "name"]
        .iter()
        .find_map(|k| args.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

#[async_trait]
impl PrimitiveInvoker for KernPrimitiveInvoker {
    async fn invoke(&self, primitive: &str, args: Value) -> Result<ToolReply, String> {
        let tool = self.kern.tool(primitive).ok_or_else(|| format!("原语未注册：{primitive}"))?;
        let ctx = ExecutionContext {
            scope: Vec::new(),
            session_id: self.session_id,
            agent_id: AgentId::new("primitive"),
            task_id: TaskId::new("primitive"),
            target: target_from_args(&args),
            // 外层已审批；内层按原语自身风险记录（不再触发二次审批）。
            risk: tool.risk(),
            approved: true,
        };
        let out = tool.call(&ctx, args).await;
        Ok(ToolReply {
            ok: out.ok,
            summary: out.summary,
            detail: out.detail,
            duration_ms: out.duration_ms,
        })
    }

    fn has(&self, primitive: &str) -> bool {
        self.kern.tool(primitive).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wpa_api::{KnowledgeHit, Phase};
    use tree_core::hooks::GuardRule;

    struct StubProvider;

    #[async_trait]
    impl KnowledgeProvider for StubProvider {
        fn name(&self) -> &str {
            "stub-kb"
        }
        async fn search(&self, q: &str, _limit: usize) -> Vec<KnowledgeHit> {
            if q.contains("smb") {
                vec![KnowledgeHit {
                    id: "K1".into(),
                    title: "SMB 空会话".into(),
                    body: "枚举共享".into(),
                    score: 10.0,
                    source: "stub".into(),
                }]
            } else {
                vec![]
            }
        }
    }

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("t"),
            task_id: TaskId::new("t"),
            target: String::new(),
            risk: RiskLevel::None,
            approved: true,
        }
    }

    #[tokio::test]
    async fn kb_search_merges_and_reports() {
        let t = KbSearch::new(vec![Arc::new(StubProvider)]);
        let r = t.call(&ctx(), json!({"query": "smb 空会话"})).await;
        assert!(r.ok);
        assert!(r.summary.contains("SMB 空会话"), "{}", r.summary);
        let d = r.detail.unwrap();
        assert_eq!(d["hits"][0]["provider"], "stub-kb");
    }

    #[tokio::test]
    async fn kb_search_without_providers_is_graceful() {
        let t = KbSearch::new(vec![]);
        let r = t.call(&ctx(), json!({"query": "x"})).await;
        assert!(r.ok);
        assert!(r.summary.contains("未装入"));
    }

    #[tokio::test]
    async fn plan_phase_view_and_switch() {
        let shared = tree_core::phases::shared(vec![
            Phase::new("recon", "侦察").with_allowed(&["tools.scan"]),
            Phase::new("verify", "验证"),
        ]);
        let t = PlanPhase::new(shared.clone());
        let r = t.call(&ctx(), json!({})).await;
        assert!(r.ok);
        assert!(r.summary.contains("尚未进入"), "{}", r.summary);

        let r = t.call(&ctx(), json!({"phase": "recon"})).await;
        assert!(r.ok);
        assert!(r.summary.contains("侦察"), "{}", r.summary);

        let r = t.call(&ctx(), json!({"phase": "nope"})).await;
        assert!(!r.ok);
        assert!(shared.lock().unwrap().current_id() == Some("recon"), "失败不得改状态");
    }

    #[test]
    fn constraint_hook_denies() {
        let set = Arc::new(
            tree_core::ConstraintSet::new()
                .with_rule(GuardRule::new("roe", "禁止高危利用").for_tools(&["exploit.*"]).min_risk(wpa_api::RiskTier::High)),
        );
        let h = ConstraintHook::new(set);
        assert!(matches!(
            h.on_tool_call("exploit.rce", RiskLevel::High, "{}"),
            HookVerdict::Deny(_)
        ));
        assert!(matches!(
            h.on_tool_call("scan.port", RiskLevel::Low, "{}"),
            HookVerdict::Allow
        ));
    }
}
