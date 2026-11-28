//! `os.exec`：经进程沙箱执行命令行程序（argv 直传，不经 shell）。
//!
//! 安全设计：
//! - 参数以 argv 数组传递，**不经 shell**，天然免疫命令拼接注入；
//! - 每次执行限时（默认 30s，上限 120s）+ 输出配额 + 审计元数据；
//! - risk=Medium（问题 12：执行类降级，无需 HITL 审批门）。

use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Instant;

use kern_core::model::{ExecutionContext, RiskLevel};

use crate::sandbox::{Sandbox, SandboxConfig};
use crate::tools::{Tool, ToolResult};

/// 回显给 LLM 的单通道输出上限（全量在 detail，由沙箱配额兜底）。
const SUMMARY_LIMIT: usize = 4 * 1024;

pub struct ProcExec {
    default_cfg: SandboxConfig,
}

impl ProcExec {
    pub fn new() -> Self {
        Self {
            default_cfg: SandboxConfig::default(),
        }
    }

    pub fn with_config(cfg: SandboxConfig) -> Self {
        Self { default_cfg: cfg }
    }

    /// R5-6：启用受限令牌执行（Safer NORMALUSER，fail-closed）。
    pub fn with_restricted(mut self, restricted: bool) -> Self {
        self.default_cfg.restricted = restricted;
        self
    }
}

impl Default for ProcExec {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= SUMMARY_LIMIT {
        s.to_string()
    } else {
        let mut end = SUMMARY_LIMIT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n…[输出已截断]", &s[..end])
    }
}

#[async_trait]
impl Tool for ProcExec {
    fn name(&self) -> &str {
        "os.exec"
    }

    fn description(&self) -> String {
        "在进程沙箱中执行命令行程序（argv 直传、不经 shell），限时并截断输出。\
参数：command(必填，程序名)、args(可选，字符串数组)、timeout_secs(可选，1-120 默认 30)。\
风险按参数判定：业务规则白名单内的程序免审；未知/破坏性命令需人工审批。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": { "type": "string", "description": "程序名，如 nmap / curl" },
                "args": { "type": "array", "items": { "type": "string" },
                          "description": "参数列表，逐项传递不经 shell" },
                "timeout_secs": { "type": "number", "description": "超时秒数，1-120" }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        // 静态典型档位（UI 展示）；实际审批判定走 risk_for 参数级分类（R1-1）。
        RiskLevel::Medium
    }

    fn risk_for(&self, args: &Value) -> RiskLevel {
        crate::risk_policy::classify_exec_args(args)
    }

    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();

        let Some(program) = args.get("command").and_then(Value::as_str).filter(|s| !s.is_empty())
        else {
            return ToolResult::error("os.exec 缺少 command 参数", 0);
        };

        let argv: Vec<String> = args
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(self.default_cfg.max_exec_ms / 1000)
            .clamp(1, 120);

        let cfg = SandboxConfig {
            max_exec_ms: timeout_secs * 1000,
            // 空间「沙箱启用/关闭」优先，否则用装配期的全局默认。
            restricted: crate::sandbox::effective_restricted(self.default_cfg.restricted),
            ..SandboxConfig::default()
        };
        let sandbox = Sandbox::new(cfg);
        let argrefs: Vec<&str> = argv.iter().map(String::as_str).collect();

        match sandbox.exec(ctx, program, &argrefs).await {
            Ok(out) => {
                let ms = start.elapsed().as_millis() as u64;
                let mut summary = String::new();
                if !out.stdout.trim().is_empty() {
                    summary.push_str(&format!("stdout:\n{}", truncate(&out.stdout)));
                }
                if !out.stderr.trim().is_empty() {
                    summary.push_str(&format!("\nstderr:\n{}", truncate(&out.stderr)));
                }
                if summary.is_empty() {
                    summary = match out.exit_code {
                        Some(0) => "(无输出，退出码 0)".into(),
                        Some(c) => format!("(无输出，退出码 {c})"),
                        None => "(无输出)".into(),
                    };
                }
                let ok = out.ok;
                ToolResult {
                    ok,
                    kind: crate::tools::ResultKind::Text,
                    summary: format!("{program} 退出码 {:?}\n{summary}", out.exit_code),
                    detail: Some(json!({
                        "command": program,
                        "args": argv,
                        "exit_code": out.exit_code,
                        "stdout_bytes": out.stdout.len(),
                        "stderr_bytes": out.stderr.len(),
                        "duration_ms": out.duration_ms,
                    })),
                    duration_ms: ms,
                }
            }
            Err(e) => ToolResult::error(
                format!("os.exec `{program}` 失败: {e}"),
                start.elapsed().as_millis() as u64,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kern_core::model::{AgentId, SessionId, TaskId};

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("test"),
            task_id: TaskId::new("t1"),
            target: "127.0.0.1".into(),
            risk: RiskLevel::High,
            approved: true,
        }
    }

    #[tokio::test]
    async fn echo_via_sandbox() {
        let t = ProcExec::new();
        let r = t
            .call(&ctx(), json!({"command": "cmd", "args": ["/c", "echo", "hello"]}))
            .await;
        assert!(r.ok, "summary={}", r.summary);
        assert!(r.summary.contains("hello"), "summary={}", r.summary);
    }

    #[tokio::test]
    async fn missing_command_is_error() {
        let t = ProcExec::new();
        let r = t.call(&ctx(), json!({})).await;
        assert!(!r.ok);
    }

    #[tokio::test]
    async fn timeout_is_error() {
        let t = ProcExec::new();
        let r = t
            .call(
                &ctx(),
                json!({"command": "ping", "args": ["-n", "10", "127.0.0.1"], "timeout_secs": 1}),
            )
            .await;
        assert!(!r.ok, "summary={}", r.summary);
    }

    #[test]
    fn truncate_keeps_char_boundary() {
        let s = "汉".repeat(3000);
        let t = truncate(&s);
        assert!(t.ends_with("[输出已截断]"));
    }
}
