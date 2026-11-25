//! 工具前置钩子扩展点（R3-1，dsh guard 语义）。
//!
//! 钩子在 denylist 硬校验之后、审批链之前执行，三级裁决：
//! - [`HookVerdict::Allow`]：放行（跳过后续钩子继续）；
//! - [`HookVerdict::Deny`]：拦截（不进审批、不执行，观察回填拒绝原因）；
//! - [`HookVerdict::Ask`]：交还审批链（本钩子不表态）。

use serde_json::Value;

use kern_core::model::RiskLevel;

/// 前置裁决结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookVerdict {
    /// 放行
    Allow,
    /// 拦截（附原因，写入工具观察）
    Deny(String),
    /// 不表态，交还审批链
    Ask,
}

/// 工具前置钩子：插件可声明接入，在每次工具调用前裁决。
pub trait ToolHook: Send + Sync + 'static {
    /// 钩子名（审计与插件展示用）
    fn name(&self) -> &str;
    /// 前置裁决。args_summary 为参数摘要（JSON 压缩串）。
    fn on_tool_call(&self, tool: &str, risk: RiskLevel, args_summary: &str) -> HookVerdict;
}

/// 类型擦除钩子句柄。
pub type DynHook = std::sync::Arc<dyn ToolHook>;

/// 遍历钩子链得出综合裁决：首个 Deny 生效；无 Deny 但有 Ask → Ask；全 Allow → Allow。
pub fn evaluate(hooks: &[DynHook], tool: &str, risk: RiskLevel, args: &Value) -> HookVerdict {
    let args_summary = serde_json::to_string(args).unwrap_or_default();
    let mut asked = false;
    for h in hooks {
        match h.on_tool_call(tool, risk, &args_summary) {
            HookVerdict::Deny(reason) => return HookVerdict::Deny(reason),
            HookVerdict::Ask => asked = true,
            HookVerdict::Allow => {}
        }
    }
    if asked {
        HookVerdict::Ask
    } else {
        HookVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    struct DenyListHook(Vec<String>);
    impl ToolHook for DenyListHook {
        fn name(&self) -> &str {
            "deny-list"
        }
        fn on_tool_call(&self, tool: &str, _r: RiskLevel, _a: &str) -> HookVerdict {
            if self.0.iter().any(|t| t == tool) {
                HookVerdict::Deny(format!("{tool} 被策略禁止"))
            } else {
                HookVerdict::Allow
            }
        }
    }

    struct AskHook;
    impl ToolHook for AskHook {
        fn name(&self) -> &str {
            "ask"
        }
        fn on_tool_call(&self, _: &str, _: RiskLevel, _: &str) -> HookVerdict {
            HookVerdict::Ask
        }
    }

    #[test]
    fn first_deny_wins() {
        let hooks: Vec<DynHook> = vec![
            Arc::new(DenyListHook(vec!["os.exec".to_string()])),
            Arc::new(AskHook),
        ];
        let v = evaluate(&hooks, "os.exec", RiskLevel::Medium, &json!({"command":"x"}));
        assert_eq!(v, HookVerdict::Deny("os.exec 被策略禁止".into()));
    }

    #[test]
    fn ask_falls_through_when_no_deny() {
        let hooks: Vec<DynHook> = vec![Arc::new(AskHook)];
        let v = evaluate(&hooks, "util.echo", RiskLevel::Low, &json!({}));
        assert_eq!(v, HookVerdict::Ask);
        // 无钩子 → Allow
        let v = evaluate(&[], "util.echo", RiskLevel::Low, &json!({}));
        assert_eq!(v, HookVerdict::Allow);
    }
}
