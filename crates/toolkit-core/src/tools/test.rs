//! 内置工具：echo —— 沙箱与工具链的自检工具。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Instant;

use super::ToolResult;
use crate::tools::Tool;
use kern_core::model::{ExecutionContext, RiskLevel};

pub struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "util.echo"
    }

    fn description(&self) -> String {
        "原样返回输入文本，用于自检与链路验证。参数：text".into()
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "required": ["text"], "properties": { "text": { "type": "string" } } })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        ToolResult::text(format!("echo: {text}"), start.elapsed().as_millis() as u64)
    }
}
