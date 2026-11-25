//! Python 运行沙箱插件（sandbox.python + python-sandbox 组件）。
//!
//! - 解释器：exe 同级 `python/python.exe`（兼容用户预置的 `python-3.15-embed/python.exe`），
//!   不接受任意 command（防绕过）；
//! - 脚本：固定 exe 同级 `scripts/`，拒绝 `..`、绝对路径、盘符；
//! - argv 直传不经 shell；cwd=scripts；超时 + 输出截断 + 审计日志；
//! - 一切皆插件：Tool 供 LLM 调用；Component 供插件清单登记。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use kern_core::{Component, ComponentKind, ComponentStatus};
use kern_core::model::ExecutionContext;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::Duration;

use crate::sandbox::{Sandbox, SandboxConfig};
use crate::tools::{Tool, ToolResult};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const OUTPUT_CAP: usize = 512 * 1024;

#[derive(Debug, Error)]
pub enum PySandboxError {
    #[error("Python 解释器未找到（检查 exe 同级 python/ 或 python-3.15-embed/）")]
    InterpreterMissing,
    #[error("非法脚本路径：{0}")]
    IllegalPath(String),
    #[error("脚本不存在：{0}")]
    ScriptMissing(PathBuf),
    #[error("执行超时（{0}ms）")]
    Timeout(u64),
    #[error("spawn 失败：{0}")]
    Spawn(std::io::Error),
}

/// 解析 exe 根目录（与 app::app_root 同语义）。
pub fn app_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 定位 Python 解释器：优先 python/python.exe，兼容 python-3.15-embed/python.exe。
pub fn find_interpreter(root: &Path) -> Option<PathBuf> {
    let candidates = [
        root.join("python").join("python.exe"),
        root.join("python-3.15-embed").join("python.exe"),
        root.join("python").join("python3.exe"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// 脚本根（exe 同级 scripts/）。
pub fn scripts_root(root: &Path) -> PathBuf {
    root.join("scripts")
}

/// 校验脚本相对路径，返回 scripts/ 下的绝对路径。
pub fn resolve_script(root: &Path, script: &str) -> Result<PathBuf, PySandboxError> {
    let s = script.trim();
    if s.is_empty() || s.contains("..") || s.starts_with('/') || s.starts_with('\\')
        || s.contains(':') || s.contains('\0') {
        return Err(PySandboxError::IllegalPath(script.to_string()));
    }
    if !s.to_ascii_lowercase().ends_with(".py") {
        return Err(PySandboxError::IllegalPath("仅允许 .py 脚本".into()));
    }
    let path = scripts_root(root).join(s);
    if !path.is_file() {
        return Err(PySandboxError::ScriptMissing(path));
    }
    Ok(path)
}

/// 结构化执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyRunResult {
    pub interpreter: String,
    pub script: String,
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// 沙箱执行（共享引擎，供 Tool 与未来组件调用）。
pub async fn run_script(
    ctx: &ExecutionContext,
    script: &str,
    args: &[String],
    stdin: Option<&str>,
    timeout_ms: u64,
) -> Result<PyRunResult, PySandboxError> {
    let root = app_root();
    let interp = find_interpreter(&root).ok_or(PySandboxError::InterpreterMissing)?;
    let script_path = resolve_script(&root, script)?;
    let timeout = timeout_ms.clamp(1000, MAX_TIMEOUT_MS);
    let scripts = scripts_root(&root);

    // PYTHONPATH = scripts/ + exe 同级 site-packages/（存在才追加，sandbox.pip 安装目标）
    let mut py_paths = vec![scripts.to_string_lossy().into_owned()];
    let site_packages = root.join("site-packages");
    if site_packages.is_dir() {
        py_paths.push(site_packages.to_string_lossy().into_owned());
    }

    let mut cmd = Command::new(&interp);
    cmd.arg(&script_path)
        .args(args)
        .current_dir(&scripts)
        .env_clear()
        .env("PYTHONPATH", py_paths.join(";"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .kill_on_drop(true)
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // R4-5 凭据洗涤：剥离 KEY/PASSWORD/SECRET/TOKEN 形命名后回灌其余环境
    for (k, v) in crate::sandbox::scrubbed_env() {
        cmd.env(k, v);
    }

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(PySandboxError::Spawn)?;
    if let Some(input) = stdin {
        if let Some(mut sin) = child.stdin.take() {
            let _ = sin.write_all(input.as_bytes()).await;
        }
    }
    let out = tokio::time::timeout(Duration::from_millis(timeout), child.wait_with_output())
        .await
        .map_err(|_| PySandboxError::Timeout(timeout))?
        .map_err(PySandboxError::Spawn)?;
    let duration_ms = started.elapsed().as_millis() as u64;

    let trunc = |b: &[u8]| -> String {
        let slice = if b.len() > OUTPUT_CAP { &b[..OUTPUT_CAP] } else { b };
        let mut s = String::from_utf8_lossy(slice).into_owned();
        if b.len() > OUTPUT_CAP {
            s.push_str(&format!("\n[OUTPUT TRUNCATED at {OUTPUT_CAP} bytes]"));
        }
        s
    };

    tracing::info!(
        session = %ctx.session_id.as_uuid(),
        task = %ctx.task_id.as_ref(),
        script = %script,
        exit = ?out.status.code(),
        duration_ms,
        "python sandbox executed"
    );

    Ok(PyRunResult {
        interpreter: interp.to_string_lossy().into_owned(),
        script: script.to_string(),
        args: args.to_vec(),
        exit_code: out.status.code(),
        stdout: trunc(&out.stdout),
        stderr: trunc(&out.stderr),
        duration_ms,
    })
}

/// R5-6：受限令牌版 python 执行（Safer NORMALUSER，fail-closed）。
/// 差异：stdin 不支持（受限管道未接 stdin，传参走 argv）；环境块由受限令牌
/// CreateEnvironmentBlock 生成后叠加 PYTHONPATH/PYTHONDONTWRITEBYTECODE。
pub async fn run_script_restricted(
    ctx: &ExecutionContext,
    script: &str,
    args: &[String],
    timeout_ms: u64,
) -> Result<PyRunResult, PySandboxError> {
    let root = app_root();
    let interp = find_interpreter(&root).ok_or(PySandboxError::InterpreterMissing)?;
    let script_path = resolve_script(&root, script)?;
    let timeout = timeout_ms.clamp(1000, MAX_TIMEOUT_MS);
    let scripts = scripts_root(&root);

    let mut py_paths = vec![scripts.to_string_lossy().into_owned()];
    let site_packages = root.join("site-packages");
    if site_packages.is_dir() {
        py_paths.push(site_packages.to_string_lossy().into_owned());
    }

    let spec = crate::restricted::RestrictedSpec {
        program: interp.to_string_lossy().into_owned(),
        args: std::iter::once(script_path.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .collect(),
        cwd: scripts.to_string_lossy().into_owned(),
        env: vec![
            ("PYTHONPATH".into(), py_paths.join(";")),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
        ],
        timeout_ms: timeout.min(u32::MAX as u64) as u32,
    };

    tracing::info!(session = %ctx.session_id.as_uuid(), script = %script, "python sandbox (restricted token)");
    let out = tokio::task::spawn_blocking(move || crate::restricted::launch_restricted(&spec))
        .await
        .map_err(|e| PySandboxError::Spawn(std::io::Error::other(e.to_string())))?
        .map_err(|e| PySandboxError::Spawn(std::io::Error::other(e)))?;
    if out.timed_out {
        return Err(PySandboxError::Timeout(timeout));
    }
    let duration_ms = out.duration_ms;
    let trunc = |s: &str| -> String {
        let b = s.as_bytes();
        let mut r = s.to_string();
        if b.len() > OUTPUT_CAP {
            r = String::from_utf8_lossy(&b[..OUTPUT_CAP]).into_owned();
            r.push_str(&format!("\n[OUTPUT TRUNCATED at {OUTPUT_CAP} bytes]"));
        }
        r
    };

    Ok(PyRunResult {
        interpreter: "restricted-token".into(),
        script: script.to_string(),
        args: args.to_vec(),
        exit_code: Some(out.exit_code),
        stdout: trunc(&out.stdout),
        stderr: trunc(&out.stderr),
        duration_ms,
    })
}

/// Tool: sandbox.python
/// R5-6：`restricted` 为 true 时经受限令牌（Safer NORMALUSER，fail-closed）执行。
#[derive(Default)]
pub struct PySandboxTool {
    restricted: bool,
}

impl PySandboxTool {
    pub fn with_restricted(mut self, restricted: bool) -> Self {
        self.restricted = restricted;
        self
    }
}

#[async_trait]
impl Tool for PySandboxTool {
    fn name(&self) -> &str { "sandbox.python" }
    fn description(&self) -> String {
        "Python 运行沙箱：在 exe 同级 scripts/ 目录内执行 .py 脚本（解释器来自随包 python/），\
argv 直传、限时、输出截断、审计留痕。script 与 code 二选一：script 为 scripts/ 下相对路径（禁 .. / 绝对路径）；\
code 为内联源码（自动落盘 scripts/_inline_<hash>.py 后执行）。依赖可先用 sandbox.pip 安装。\
风险按内容判定：脚本路径默认免审；内联代码命中破坏性/逃逸模式需人工审批。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": [],
            "properties": {
                "script": {"type": "string", "description": "scripts/ 下相对路径，如 recon.py（与 code 二选一）"},
                "code": {"type": "string", "description": "内联 Python 源码（与 script 二选一，自动落盘 scripts/_inline_<hash>.py）"},
                "args": {"type": "array", "items": {"type": "string"}},
                "stdin": {"type": "string"},
                "timeout_secs": {"type": "number"}
            }
        })
    }
    fn risk(&self) -> kern_core::model::RiskLevel { kern_core::model::RiskLevel::Medium }
    /// R1-1：内联 code 走内容扫描（破坏性/逃逸模式 → High），脚本路径免审。
    fn risk_for(&self, args: &Value) -> kern_core::model::RiskLevel {
        crate::risk_policy::classify_python_args(args)
    }

    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        // script 与 code 二选一：code 优先级仅在其显式给出且 script 缺省时生效
        let script = match args.get("script").and_then(Value::as_str).map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                // inline code：内容 hash 前 8 位命名，覆盖写，防落盘路径注入（文件名纯 hash）
                match args.get("code").and_then(Value::as_str) {
                    Some(c) if !c.trim().is_empty() => {
                        let root = app_root();
                        let mut hasher = DefaultHasher::new();
                        c.hash(&mut hasher);
                        let name = format!("_inline_{:08x}.py", (hasher.finish() >> 32) as u32);
                        let scripts = scripts_root(&root);
                        if let Err(e) = tokio::fs::create_dir_all(&scripts).await {
                            return ToolResult::error(format!("创建 scripts/ 失败: {e}"), ms());
                        }
                        if let Err(e) = tokio::fs::write(scripts.join(&name), c).await {
                            return ToolResult::error(format!("inline 脚本落盘失败: {e}"), ms());
                        }
                        name
                    }
                    _ => return ToolResult::error("缺少参数 script 或 code（二选一）", ms()),
                }
            }
        };
        let argv: Vec<String> = match args.get("args") {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            Some(Value::String(s)) => s.split_whitespace().map(|x| x.to_string()).collect(),
            _ => vec![],
        };
        let stdin = args.get("stdin").and_then(Value::as_str);
        let timeout_ms = args.get("timeout_secs").and_then(Value::as_u64)
            .map(|v| v.saturating_mul(1000)).unwrap_or(DEFAULT_TIMEOUT_MS);
        let run = if self.restricted {
            run_script_restricted(ctx, &script, &argv, timeout_ms).await
        } else {
            run_script(ctx, &script, &argv, stdin, timeout_ms).await
        };
        match run {
            Ok(r) => {
                let summary = format!(
                    "Python {} {} → exit {:?}（{}ms，stdout {}B）",
                    r.script, r.args.join(" "), r.exit_code, r.duration_ms, r.stdout.len()
                );
                ToolResult::json(summary, json!(r), ms())
            }
            Err(e) => ToolResult::error(format!("Python 沙箱失败: {e}"), ms()),
        }
    }
}

/// Component: python-sandbox（一切皆插件：登记进 Registry）。
pub struct PythonSandboxComponent;

impl Component for PythonSandboxComponent {
    fn id(&self) -> &str { "python-sandbox" }
    fn kind(&self) -> ComponentKind { ComponentKind::Plugin }
    fn version(&self) -> &str { "1.1.0" }
    fn description(&self) -> String {
        "Python 运行沙箱：随包解释器 + scripts/ 脚本隔离 + inline code + sandbox.pip 依赖 + 限时/截断/审计".into()
    }
    fn status(&self) -> ComponentStatus { ComponentStatus::Loaded }
    fn as_any(&self) -> &dyn std::any::Any { self }
}

/// Tool: sandbox.pip —— pip 依赖安装（--target exe 同级 site-packages/）。
///
/// 脚本执行的依赖补齐环节：file.write 写脚本 → sandbox.pip 装依赖 → sandbox.python 执行。
/// PYTHONPATH 已含 site-packages/，安装后脚本可直接 import。
pub struct PipTool;

#[async_trait]
impl Tool for PipTool {
    fn name(&self) -> &str { "sandbox.pip" }
    fn description(&self) -> String {
        "Python 依赖安装：python -m pip install --target <exe 同级 site-packages/>（先 ensurepip 幂等自举，失败仅记录）。\
sandbox.python 的 PYTHONPATH 已包含 site-packages/，安装完成即可 import。\
参数：packages(必填,字符串数组)、upgrade(可选,bool,默认 false)。中风险。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["packages"],
            "properties": {
                "packages": {"type": "array", "items": {"type": "string"},
                             "description": "包名列表，如 [\"requests\", \"pycryptodome\"]"},
                "upgrade": {"type": "boolean", "description": "是否加 --upgrade，默认 false"}
            }
        })
    }
    fn risk(&self) -> kern_core::model::RiskLevel { kern_core::model::RiskLevel::Medium }

    async fn call(&self, ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let packages: Vec<String> = match args.get("packages") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect(),
            Some(Value::String(s)) => s.split_whitespace().map(|x| x.to_string()).collect(),
            _ => return ToolResult::error("缺少参数 packages（字符串数组）", ms()),
        };
        if packages.is_empty() {
            return ToolResult::error("packages 为空", ms());
        }
        // 包名安全检查：包名会进入 argv，禁路径分隔符与转义
        if packages.iter().any(|p| p.contains("..") || p.contains('/') || p.contains('\\') || p.contains('\0')) {
            return ToolResult::error("packages 含非法字符（禁路径分隔符）", ms());
        }
        let upgrade = args.get("upgrade").and_then(Value::as_bool).unwrap_or(false);

        let root = app_root();
        let Some(interp) = find_interpreter(&root) else {
            return ToolResult::error("Python 解释器未找到（检查 exe 同级 python/）", ms());
        };
        let interp = interp.to_string_lossy().into_owned();
        let site = root.join("site-packages");

        // 沙箱统一执行：限时 + decode_output 截断 + 审计
        let sandbox = Sandbox::new(SandboxConfig {
            max_exec_ms: MAX_TIMEOUT_MS,
            ..SandboxConfig::default()
        });

        // ensurepip 幂等自举（失败仅记录继续——多数随包解释器已带 pip）
        match sandbox.exec(ctx, &interp, &["-m", "ensurepip"]).await {
            Ok(o) => tracing::info!(exit = ?o.exit_code, "sandbox.pip ensurepip"),
            Err(e) => tracing::warn!(error = %e, "ensurepip 失败（继续尝试 pip install）"),
        }

        let mut install_args: Vec<String> = vec![
            "-m".into(), "pip".into(), "install".into(),
            "--target".into(), site.to_string_lossy().into_owned(),
        ];
        if upgrade {
            install_args.push("--upgrade".into());
        }
        install_args.extend(packages.iter().cloned());
        let argrefs: Vec<&str> = install_args.iter().map(String::as_str).collect();

        match sandbox.exec(ctx, &interp, &argrefs).await {
            Ok(out) => {
                let summary = format!(
                    "pip install {} → exit {:?}（{}ms，stdout {}B）",
                    packages.join(" "), out.exit_code, out.duration_ms, out.stdout.len()
                );
                ToolResult::json(
                    summary,
                    json!({
                        "packages": packages,
                        "target": site.to_string_lossy(),
                        "exit_code": out.exit_code,
                        "stdout": out.stdout,
                        "stderr": out.stderr,
                        "duration_ms": out.duration_ms,
                    }),
                    ms(),
                )
            }
            Err(e) => ToolResult::error(format!("pip 安装失败: {e}"), ms()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_interpreter_from_fixture() {
        // 用临时目录模拟两种布局（不依赖真实部署）
        let tmp = std::env::temp_dir().join(format!("wpa_py_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(tmp.join("python"));
        let fake = tmp.join("python").join("python.exe");
        let _ = std::fs::write(&fake, b"fake");
        assert_eq!(find_interpreter(&tmp), Some(fake.clone()));
        let _ = std::fs::remove_file(&fake);
        assert!(find_interpreter(&tmp).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reject_illegal_paths() {
        let root = PathBuf::from("/fake");
        assert!(matches!(resolve_script(&root, "../etc.py"), Err(PySandboxError::IllegalPath(_))));
        assert!(matches!(resolve_script(&root, "C:\\x.py"), Err(PySandboxError::IllegalPath(_))));
        assert!(matches!(resolve_script(&root, "/x.py"), Err(PySandboxError::IllegalPath(_))));
        assert!(matches!(resolve_script(&root, "x.txt"), Err(PySandboxError::IllegalPath(_))));
        assert!(matches!(resolve_script(&root, ""), Err(PySandboxError::IllegalPath(_))));
    }

    #[test]
    fn resolve_valid_script() {
        let tmp = std::env::temp_dir().join(format!("wpa_py_scripts_{}", std::process::id()));
        let _ = std::fs::create_dir_all(tmp.join("scripts"));
        let s = tmp.join("scripts").join("a.py");
        let _ = std::fs::write(&s, b"print(1)");
        assert_eq!(resolve_script(&tmp, "a.py").unwrap(), s);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn component_and_tool_meta() {
        let c = PythonSandboxComponent;
        assert_eq!(c.id(), "python-sandbox");
        assert_eq!(c.kind(), ComponentKind::Plugin);
        assert_eq!(PySandboxTool::default().name(), "sandbox.python");
        // 问题12：执行类工具降为中风险
        assert_eq!(PySandboxTool::default().risk(), kern_core::model::RiskLevel::Medium);
        assert_eq!(PipTool.name(), "sandbox.pip");
        assert_eq!(PipTool.risk(), kern_core::model::RiskLevel::Medium);
    }

    #[tokio::test]
    async fn inline_code_roundtrip_meta() {
        // inline code 路径只验证元数据分支：缺 script 且缺 code 必须报错
        let ctx = kern_core::model::ExecutionContext {
            scope: Vec::new(),
            session_id: kern_core::model::SessionId::new(),
            agent_id: kern_core::model::AgentId::new("t"),
            task_id: kern_core::model::TaskId::new("t"),
            target: "127.0.0.1".into(),
            risk: kern_core::model::RiskLevel::Medium,
            approved: true,
        };
        let r = PySandboxTool::default().call(&ctx, json!({})).await;
        assert!(!r.ok);
        assert!(r.summary.contains("script 或 code"));
    }
}
