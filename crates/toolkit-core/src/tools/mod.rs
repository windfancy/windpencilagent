//! 工具：LLM 通过 function-calling 调用的原子能力单元。
//!
//! 定义 `Tool` trait 与通用工具集。所有工具在被调用前都必须经过
//! 授权校验（授权范围 / 拒绝清单 / 风险审批，由会话执行链统一执行）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use kern_core::model::{ExecutionContext, RiskLevel};

pub mod dict;
pub mod ext;
pub mod file;
pub mod procexec;
pub mod pysandbox;
pub mod session_search;
pub mod test;

/// 工具结果的种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultKind {
    Text,
    Json,
    FileRef,
    Error,
}

/// 工具执行产出（精简回显 + 全量载荷分道）。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub ok: bool,
    pub kind: ResultKind,
    /// 给 LLM 的精简回显（必须截断到安全长度）
    pub summary: String,
    /// 结构化结果（可选，供黑板/报告）
    pub detail: Option<Value>,
    pub duration_ms: u64,
}

impl ToolResult {
    pub fn text(summary: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            ok: true,
            kind: ResultKind::Text,
            summary: summary.into(),
            detail: None,
            duration_ms,
        }
    }

    pub fn json(summary: impl Into<String>, detail: Value, duration_ms: u64) -> Self {
        Self {
            ok: true,
            kind: ResultKind::Json,
            summary: summary.into(),
            detail: Some(detail),
            duration_ms,
        }
    }

    pub fn error(summary: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            ok: false,
            kind: ResultKind::Error,
            summary: summary.into(),
            detail: None,
            duration_ms,
        }
    }
}

/// 工具接口：名称 / 语义描述 / JSON Schema / 调用。
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn input_schema(&self) -> Value;
    /// 工具自身声明的风险级别（静态典型档位，供 UI 展示与缺省判定）。
    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }
    /// 参数级风险判定（R1-1，dsh 对齐）：默认回落静态 `risk()`；
    /// 执行类工具（os.exec / shell.run / sandbox.python）覆写为按参数分类。
    fn risk_for(&self, _args: &Value) -> RiskLevel {
        self.risk()
    }
    /// 并发安全标记（R4-1，dsh 并行池对齐）：无共享可变副作用、可并行执行的
    /// 只读工具覆写为 true；默认 false 保守串行。
    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        false
    }
    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult;
}

/// 类型擦除的工具句柄。
pub type DynTool = Arc<dyn Tool>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_utils() {
        let r = ToolResult::text("ok", 1);
        assert!(r.ok);
        assert_eq!(r.kind, ResultKind::Text);
        let r = ToolResult::error("boom", 2);
        assert!(!r.ok);
        assert_eq!(r.kind, ResultKind::Error);
    }

    #[test]
    fn risk_levels_follow_severity_guide() {
        use kern_core::model::RiskLevel;
        // 通用工具：只读/回显=无风险，写文件=中，命令执行=高
        assert!(matches!(super::super::builtin::Echo.risk(), RiskLevel::None | RiskLevel::Low));
        // 只有 High/Critical 才需人工审批
        assert!(!RiskLevel::None.needs_approval());
        assert!(!RiskLevel::Low.needs_approval());
        assert!(!RiskLevel::Medium.needs_approval());
        assert!(RiskLevel::High.needs_approval());
        assert!(RiskLevel::Critical.needs_approval());
    }
}
