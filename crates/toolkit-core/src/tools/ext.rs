//! 外部工具注册：把外部目录安装好的命令行工具注册为一等智能体工具。
//!
//! 设计：
//! - `ExtToolSpec` 是注册规格（TOML `[[tools]]` / GUI 注册表单共用），
//!   含固定路径、预设参数、风险级别与超时；
//! - `ExtTool` 是规格的 Tool 实现：经进程沙箱执行 `path + 预设参数 + 调用参数`，
//!   argv 直传不经 shell，与 `os.exec` 同级安全约束；
//! - 注册进 Registry 后即为 kind=Tool 组件（一切皆插件），
//!   LLM 工具面与执行器同时可见，无需重启。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kern_core::model::{ExecutionContext, RiskLevel};

use crate::sandbox::{Sandbox, SandboxConfig};
use crate::tools::{Tool, ToolResult};

/// 回显给 LLM 的单通道输出上限（与 os.exec 对齐）。
const SUMMARY_LIMIT: usize = 4 * 1024;

/// 外部工具注册规格（config.toml `[[tools]]` 与 GUI 注册表单共用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtToolSpec {
    /// 工具名（即 LLM 工具名，如 `nmap`；须为合法工具标识）
    pub name: String,
    /// 可执行文件路径（外部目录安装位置）
    pub path: String,
    /// 给 LLM 的能力描述（空则用默认文案）
    #[serde(default)]
    pub description: String,
    /// 预设参数：每次调用自动前置（如 nmap 的 `["-Pn"]`）
    #[serde(default)]
    pub args: Vec<String>,
    /// 风险级别：none/low/medium/high/critical（默认 high，需审批）
    #[serde(default)]
    pub risk: String,
    /// 单次执行超时秒数（默认 120，上限 600）
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ExtToolSpec {
    /// 风险级别解析：未知值回落 Medium（问题5：避免误伤，明确写 high 才按高）。
    pub fn risk_level(&self) -> RiskLevel {
        match self.risk.to_ascii_lowercase().as_str() {
            "none" => RiskLevel::None,
            "low" => RiskLevel::Low,
            "medium" => RiskLevel::Medium,
            "high" => RiskLevel::High,
            "critical" => RiskLevel::Critical,
            _ => RiskLevel::Medium,
        }
    }

    /// 校验注册规格：名字合法、路径存在、超时在界内。
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("工具名不能为空".into());
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            return Err("工具名只允许字母/数字/`.`/`-`/`_`".into());
        }
        // 禁止与内置工具重名（含历史旧名 echo）
        if name == "echo" || name == "util.echo" || name.contains("..") {
            return Err("工具名与内置工具或路径片段冲突".into());
        }
        let p = Path::new(&self.path);
        if self.path.trim().is_empty() || !p.is_file() {
            return Err(format!("可执行文件不存在: {}", self.path));
        }
        Ok(())
    }
}

/// 外部注册工具：`ExtToolSpec` 的一等 Tool 实现。
#[derive(Debug, Clone)]
pub struct ExtTool {
    spec: ExtToolSpec,
    path: PathBuf,
    risk: RiskLevel,
    timeout_secs: u64,
}

impl ExtTool {
    /// 从规格构建；规格非法（名字/路径）返回错误。
    pub fn from_spec(spec: ExtToolSpec) -> Result<Self, String> {
        spec.validate()?;
        let risk = spec.risk_level();
        let timeout_secs = spec.timeout_secs.unwrap_or(120).clamp(1, 600);
        Ok(Self {
            path: PathBuf::from(spec.path.trim()),
            spec,
            risk,
            timeout_secs,
        })
    }

    /// 注册规格只读视图（持久化 / UI 展示用）。
    pub fn spec(&self) -> &ExtToolSpec {
        &self.spec
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
impl Tool for ExtTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> String {
        if self.spec.description.trim().is_empty() {
            format!(
                "外部注册工具 {}（{}）。参数：args(可选，字符串数组)。执行路径：{}",
                self.spec.name,
                self.spec.path,
                self.spec.path
            )
        } else {
            format!(
                "{} 参数：args(可选，字符串数组，逐项传递不经 shell)。",
                self.spec.description.trim()
            )
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "命令行参数，逐项传递不经 shell"
                },
                "timeout_secs": {
                    "type": "number",
                    "description": "超时秒数，1-600（默认注册值）"
                }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        self.risk
    }

    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();

        let mut argv: Vec<String> = self.spec.args.clone();
        if let Some(extra) = args.get("args").and_then(Value::as_array) {
            argv.extend(extra.iter().filter_map(|v| v.as_str().map(String::from)));
        }
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(self.timeout_secs)
            .clamp(1, 600);

        let cfg = SandboxConfig {
            max_exec_ms: timeout_secs * 1000,
            ..SandboxConfig::default()
        };
        let sandbox = Sandbox::new(cfg);
        let argrefs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let program = self.path.to_string_lossy().to_string();

        match sandbox.exec(ctx, &program, &argrefs).await {
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
                    summary: format!("{} 退出码 {:?}\n{summary}", self.spec.name, out.exit_code),
                    detail: Some(json!({
                        "tool": self.spec.name,
                        "path": self.spec.path,
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
                format!("{} 执行失败: {e}", self.spec.name),
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

    fn spec() -> ExtToolSpec {
        ExtToolSpec {
            name: "cmd-echo".into(),
            path: "C:\\Windows\\System32\\cmd.exe".into(),
            description: "Windows 命令解释器（测试用）".into(),
            args: vec!["/c".into()],
            risk: "low".into(),
            timeout_secs: Some(10),
        }
    }

    #[test]
    fn validate_rejects_missing_path() {
        let mut s = spec();
        s.path = "Z:\\no\\such\\file.exe".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_name() {
        let mut s = spec();
        s.name = "bad name!".into();
        assert!(s.validate().is_err());
        s.name = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn from_spec_ok_and_defaults() {
        let mut s = spec();
        s.risk = "unknown-value".into(); // 未知风险 → 默认 Medium（问题5：避免误伤）
        s.timeout_secs = None;
        let t = ExtTool::from_spec(s).unwrap();
        assert_eq!(t.risk(), RiskLevel::Medium);
        assert_eq!(t.name(), "cmd-echo");
        assert!(t.description().contains("参数"));
    }

    #[tokio::test]
    async fn runs_with_preset_args() {
        let t = ExtTool::from_spec(spec()).unwrap();
        let r = t.call(&ctx(), json!({"args": ["echo", "hello-ext"]})).await;
        assert!(r.ok, "summary={}", r.summary);
        assert!(r.summary.contains("hello-ext"), "summary={}", r.summary);
    }

    #[test]
    fn missing_binary_rejected_at_construction() {
        // 路径不存在：构建期即拒绝（不必等执行才失败）
        let mut s = spec();
        s.path = "Z:\\no\\such\\file.exe".into();
        assert!(ExtTool::from_spec(s).is_err());
    }

    #[tokio::test]
    async fn spawn_failure_is_error() {
        // 路径是合法文件但不可执行（.txt）→ 沙箱 spawn 失败返回错误结果
        let p = std::env::temp_dir().join("wpn-ext-notexec.txt");
        std::fs::write(&p, "not an executable").unwrap();
        let s = ExtToolSpec {
            path: p.to_string_lossy().to_string(),
            ..spec()
        };
        let t = ExtTool::from_spec(s).unwrap();
        let r = t.call(&ctx(), json!({})).await;
        assert!(!r.ok, "summary={}", r.summary);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn truncate_keeps_char_boundary() {
        let s = "汉".repeat(3000);
        assert!(truncate(&s).ends_with("[输出已截断]"));
    }
}
