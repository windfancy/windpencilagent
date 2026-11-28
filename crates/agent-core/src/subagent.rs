//! R5-7（dsh D3 最小子代理）：单层 spawn——独立 ChatSession + 受限工具集
//! + 结果回传 + stopReason。防递归：子代理工具面不含 subagent.spawn 自身。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base_llm::{LlmProvider, ToolSchema};
use toolkit_core::ToolRegistryExt;
use kern_core::model::{ExecutionContext, RiskLevel};
use kern_core::{Registry, ToolExecutor};
use serde_json::{json, Value};

use crate::chat::{ApprovalCallback, ChatConfig, ChatSession};
use toolkit_core::tools::{Tool, ToolResult};

/// 可替换的 provider 槽（模型切换后子代理要跟着换，工具本身不重建）。
pub type ProviderCell = Arc<std::sync::RwLock<Option<Arc<dyn LlmProvider>>>>;

/// `subagent.spawn`：派生一个受限子代理执行独立子任务并取回结论。
pub struct SubagentTool {
    provider: ProviderCell,
    /// 共享主 registry：call 时动态收集受限工具面（含后注册工具）
    registry: Arc<Registry>,
    executor: Arc<dyn ToolExecutor>,
    authorized_scope: Vec<String>,
    max_rounds: usize,
    /// 领域参数 `allow_high_risk`：true 时 High/Critical 也可进子代理工具面并自动放行。
    /// 装配时先按 true 注册，领域加载完成后由 `base.rs` 写入真实值。
    allow_high_risk: Arc<std::sync::atomic::AtomicBool>,
}

impl SubagentTool {
    pub fn new(
        provider: ProviderCell,
        registry: Arc<Registry>,
        executor: Arc<dyn ToolExecutor>,
        authorized_scope: Vec<String>,
        max_rounds: usize,
    ) -> Self {
        Self {
            provider,
            registry,
            executor,
            authorized_scope,
            max_rounds: max_rounds.clamp(2, 8),
            allow_high_risk: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// 共享同一开关：装配层在领域加载后写入，工具运行时读取。
    pub fn with_high_risk_flag(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.allow_high_risk = flag;
        self
    }

    /// 动态收集受限工具面：排除 spawn 自身（防递归）与高危（High/Critical）。
    fn restricted_toolset(&self) -> (Vec<ToolSchema>, HashMap<String, RiskLevel>) {
        let allow_high = self
            .allow_high_risk
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut schemas = Vec::new();
        let mut risks = HashMap::new();
        for name in self.registry.tools() {
            if name == "subagent.spawn" {
                continue;
            }
            if let Some(t) = self.registry.tool(&name) {
                let risk = t.risk();
                let ok = matches!(risk, RiskLevel::None | RiskLevel::Low | RiskLevel::Medium)
                    || (allow_high && matches!(risk, RiskLevel::High | RiskLevel::Critical));
                if ok {
                    schemas.push(ToolSchema {
                        name: t.name().to_string(),
                        description: t.description(),
                        parameters: t.input_schema(),
                    });
                    risks.insert(name, risk);
                }
            }
        }
        (schemas, risks)
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent.spawn"
    }

    fn description(&self) -> String {
        "派生一个受限子代理独立执行子任务并取回最终结论（子代理工具面仅含低/中风险工具，\
高危动作一律被拒；单层派生不可再嵌套）。适合：把可并行的独立子任务隔离执行、\
避免子任务上下文污染主对话。参数：task(必填，子任务完整描述与预期产出)、max_rounds(可选,2-8,默认6). \
返回 stopReason（completed=已得出结论 / max_rounds=轮数耗尽 / failed=执行失败）与结论全文。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["task"],
            "properties": {
                "task": { "type": "string", "description": "子任务完整描述（含预期产出）" },
                "max_rounds": { "type": "number", "description": "工具轮数上限，默认 6" }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        // 派生执行：需人工审批（子代理工具面本身受限，审批防滥用）
        RiskLevel::High
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(task) = args.get("task").and_then(Value::as_str).map(|s| s.trim().to_string()) else {
            return ToolResult::error("缺少参数 task", ms());
        };
        if task.is_empty() {
            return ToolResult::error("task 不能为空", ms());
        }
        let rounds = args
            .get("max_rounds")
            .and_then(Value::as_u64)
            .unwrap_or(self.max_rounds as u64)
            .clamp(2, 8) as usize;

        // 子代理：高危审批一律拒绝（fail-closed 受限执行）；
        // 领域显式 allow_high_risk=true（如渗透领域）时放行，否则模型看得到也执行不了。
        let Some(provider) = self.provider.read().ok().and_then(|g| g.clone()) else {
            return ToolResult::error("子代理不可用：当前会话没有可用模型", ms());
        };
        let allow_hr = self
            .allow_high_risk
            .load(std::sync::atomic::Ordering::Relaxed);
        let deny_high_risk: ApprovalCallback = Arc::new(move |_tool, _risk, _summary| allow_hr);
        let (schemas, risks) = self.restricted_toolset();
        let cfg = ChatConfig {
            authorized_scope: self.authorized_scope.clone(),
            max_tool_rounds: rounds,
            summarize_history: false, // 短命会话无需摘要
            extract_every: 0,         // 不写经验库（子任务经验由主会话沉淀）
            ..Default::default()
        };
        let mut session = ChatSession::new(
            Some(provider),
            self.executor.clone(),
            schemas,
            risks,
            cfg,
            deny_high_risk,
        );
        let t = session.turn(&task).await;

        // stopReason 判定：失败 / 轮数耗尽（降级提示语）/ 正常结论
        let stop_reason = if !t.ok {
            "failed"
        } else if t.reply.contains("未得出最终结论") || t.reply.contains("轮上限") {
            "max_rounds"
        } else {
            "completed"
        };
        let trace_brief: Vec<String> = t
            .trace
            .iter()
            .map(|s| s.chars().take(160).collect())
            .collect();
        let reply_brief: String = t.reply.chars().take(8000).collect();
        ToolResult::json(
            format!("子代理完成（{stop_reason}）：{}", t.reply.chars().take(160).collect::<String>()),
            json!({
                "stopReason": stop_reason,
                "reply": reply_brief,
                "trace": trace_brief,
                "rounds_used": t.trace.len(),
            }),
            ms(),
        )
    }
}
