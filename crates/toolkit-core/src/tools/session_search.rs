//! R5-2（dsh S7 会话检索）：跨会话关键词检索工具。

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Value};

use base_store::SessionStore;

use crate::tools::{Tool, ToolResult};

/// `session.search`：跨会话关键词检索，命中会话 id/标题/片段。
/// 与 `/recall <id>` 配合：先检索定位旧会话，再注入其滚动摘要与经验。
pub struct SessionSearchTool {
    store: Arc<SessionStore>,
}

impl SessionSearchTool {
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session.search"
    }

    fn description(&self) -> String {
        "跨会话关键词检索（标题/对话正文/滚动摘要/归档，大小写不敏感），返回命中会话的 id、标题与片段；配合 /recall <id> 可注入旧会话上下文。参数：query(必填)、limit(可选,默认5)".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "description": "检索关键词" },
                "limit": { "type": "number", "description": "返回上限，默认 5" }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(query) = args.get("query").and_then(Value::as_str).map(|s| s.trim().to_string()) else {
            return ToolResult::error("缺少参数 query", ms());
        };
        if query.is_empty() {
            return ToolResult::error("query 不能为空", ms());
        }
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5).clamp(1, 20) as usize;
        let hits = self.store.search(&query, limit);
        let detail = json!({
            "total": hits.len(),
            "hits": hits.iter().map(|(rec, snippets)| json!({
                "id": rec.id,
                "title": rec.title,
                "workspace": rec.workspace,
                "updated_ms": rec.updated_ms,
                "snippets": snippets,
            })).collect::<Vec<_>>(),
        });
        if hits.is_empty() {
            return ToolResult::json(format!("无命中「{query}」的会话"), json!({"total": 0, "hits": []}), ms());
        }
        let mut summary = format!("命中 {} 个会话：\n", hits.len());
        for (rec, snippets) in &hits {
            summary.push_str(&format!("- {}（{}）：{}\n", rec.title, rec.id, snippets.first().cloned().unwrap_or_default()));
        }
        summary.push_str("提示：用 /recall <id> 注入对应旧会话的滚动摘要与经验。");
        ToolResult::json(summary, detail, ms())
    }
}
