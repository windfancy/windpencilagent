//! 执行契约：内核层与基础能力层之间的工具执行接口。
//!
//! 内核层只依赖本契约；具体实现由 toolkit-core 提供，
//! 保证「低层不依赖高层」的分层约束。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{ExecutionContext, FactEntry, RiskLevel};

/// 一次工具执行的结构化产出（内核层可消费的最小聚合）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub ok: bool,
    pub tool_name: String,
    /// 给 LLM / 日志的精简回显（截断安全）
    pub summary: String,
    /// 回写黑板的事实
    pub facts: Vec<FactEntry>,
    /// 结构化明细（供报告 / 审计）
    pub detail: Option<Value>,
    pub duration_ms: u64,
}

/// 工具执行器契约：按工具名解析并执行。
///
/// 内核 AgentLoop 通过该 trait 调用基础能力层的工具体系，
/// 两者之间零直接依赖。
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync + 'static {
    /// 按 tool_hint 顺序解析第一个可用工具并执行。
    /// `args` 为任务附加参数；工具自身参数由执行器合成。
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        tool_hint: &[String],
        args: Value,
    ) -> ToolOutcome;

    /// 列出当前可用工具名（供规划器与 UI 展示）。
    fn available_tools(&self) -> Vec<String>;

    /// 参数级风险判定（R1-1，dsh 对齐）：按本次调用参数动态评估风险。
    /// `None` = 该工具无动态判定，调用方回落静态风险表。
    fn risk_for(&self, _tool: &str, _args: &Value) -> Option<RiskLevel> {
        None
    }

    /// 并发安全查询（R4-1，dsh 并行池对齐）：该工具以给定参数执行时
    /// 是否无副作用依赖、可安全并行。默认 false（保守串行）。
    fn concurrency_safe(&self, _tool: &str, _args: &Value) -> bool {
        false
    }
}
