//! R5-5（dsh D2 计划评审门）：复杂任务先出计划、经人工批准后执行。
//!
//! 模型产出计划文本后调用 `plan.exit` 提交；工具静态风险 High → 必走 HITL
//! 审批链（复用 run_tool 通用审批）。批准 = 计划生效（观察回填 approved）；
//! 拒绝 = 模型收到审批拒绝观察，修订计划后再次提交。

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Value};

use toolkit_core::tools::{Tool, ToolResult};

/// `plan.exit`：提交执行计划等待人工批准。
pub struct ExitPlanTool;

#[async_trait]
impl Tool for ExitPlanTool {
    fn name(&self) -> &str {
        "plan.exit"
    }

    fn description(&self) -> String {
        "提交执行计划等待人工批准（计划评审门）。对多步骤/有副作用/不可逆的复杂任务，\
先把计划全文写入 plan 参数再调用本工具：批准后返回 approved（按计划执行），\
拒绝则返回审批拒绝（必须修订计划后重新提交，不得跳过评审直接执行）。\
参数：plan(必填，计划全文：目标/步骤/涉及工具与风险/回退方式)".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["plan"],
            "properties": {
                "plan": { "type": "string", "description": "计划全文（目标/步骤/工具与风险/回退方式）" }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        // 评审门语义：始终需人工审批（High → needs_approval 必走 HITL）
        RiskLevel::High
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(plan) = args.get("plan").and_then(Value::as_str).map(|s| s.trim().to_string()) else {
            return ToolResult::error("缺少参数 plan（计划全文）", ms());
        };
        if plan.is_empty() {
            return ToolResult::error("plan 不能为空", ms());
        }
        // 能走到这里 = 用户已在审批链放行
        ToolResult::json(
            "计划已获批准，按计划逐步执行；执行中如需偏离计划，先说明再行动。",
            json!({ "approved": true, "plan": plan }),
            ms(),
        )
    }
}
