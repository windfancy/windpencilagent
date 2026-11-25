//! shell-core：持久 shell 会话（基座能力，无业务语义）。
//!
//! 会话名 → {shell 类型, cwd, 环境变量增量}；每次命令以当前状态执行。
//! 与 os.exec（argv 直传不经 shell）互补：shell.run 覆盖管道/重定向/环境变量场景。
//! 安全：风险由 `toolkit-core::risk_policy` 参数级判定（基座默认可疑命令全需审批）、
//! 限时、输出截断 32KB、审计留痕；会话状态不落盘。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use toolkit_core::tools::{Tool, ToolResult};
use kern_core::{Component, ComponentKind, ComponentStatus};
use serde_json::{json, Value};

pub mod jobs;
pub use jobs::JobStore;

/// 单会话状态。
#[derive(Debug, Clone)]
pub struct ShellState {
    pub kind: ShellKind,
    pub cwd: String,
    /// 会话级环境变量增量（每次执行注入）
    pub env: HashMap<String, String>,
    pub run_count: u64,
}

/// 支持的 shell。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Cmd,
    PowerShell,
}

impl ShellKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "cmd" => Some(Self::Cmd),
            "powershell" | "pwsh" => Some(Self::PowerShell),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cmd => "cmd",
            Self::PowerShell => "powershell",
        }
    }
    /// 构造进程参数（argv 直传；command 作为整体参数交给 shell 解释）。
    pub fn spawn_args(&self, command: &str) -> (String, Vec<String>) {
        match self {
            Self::Cmd => (
                "cmd".to_string(),
                vec!["/D".to_string(), "/C".to_string(), command.to_string()],
            ),
            Self::PowerShell => (
                "powershell".to_string(),
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ],
            ),
        }
    }
}

/// 会话注册表。
#[derive(Default)]
pub struct ShellSessions {
    map: Mutex<HashMap<String, ShellState>>,
    /// 工作空间装配的默认 cwd（Profile 语义：空间 cwd 非空时新会话默认落此目录）
    default_cwd: Mutex<String>,
}

impl ShellSessions {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 设置默认 cwd（工作空间 v2 装配；空=还原系统当前目录语义）。
    pub fn set_default_cwd(&self, cwd: &str) {
        *self.default_cwd.lock().expect("lock") = cwd.to_string();
    }

    /// 创建或重置会话（cwd 必须存在；空 cwd 时优先用空间默认，再退系统当前目录）。
    pub fn create(&self, name: &str, kind: ShellKind, cwd: &str) -> Result<(), String> {
        if !cwd.is_empty() && !std::path::Path::new(cwd).is_dir() {
            return Err(format!("cwd 不存在: {cwd}"));
        }
        let cwd = if !cwd.is_empty() {
            cwd.to_string()
        } else {
            let ws_default = self.default_cwd.lock().expect("lock").clone();
            if !ws_default.is_empty() && std::path::Path::new(&ws_default).is_dir() {
                ws_default
            } else {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }
        };
        self.map.lock().expect("lock").insert(
            name.to_string(),
            ShellState { kind, cwd, env: HashMap::new(), run_count: 0 },
        );
        Ok(())
    }

    pub fn remove(&self, name: &str) -> bool {
        self.map.lock().expect("lock").remove(name).is_some()
    }

    pub fn snapshot(&self) -> Vec<(String, ShellState)> {
        let m = self.map.lock().expect("lock");
        m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn get_state(&self, name: &str) -> Option<ShellState> {
        self.map.lock().expect("lock").get(name).cloned()
    }

    pub fn bump(&self, name: &str) {
        if let Some(s) = self.map.lock().expect("lock").get_mut(name) {
            s.run_count += 1;
        }
    }

    pub fn set_cwd(&self, name: &str, cwd: &str) -> Result<(), String> {
        if !std::path::Path::new(cwd).is_dir() {
            return Err(format!("cwd 不存在: {cwd}"));
        }
        self.map
            .lock()
            .expect("lock")
            .get_mut(name)
            .map(|s| s.cwd = cwd.to_string())
            .ok_or_else(|| format!("会话 {name} 不存在"))
    }

    pub fn set_env(&self, name: &str, key: &str, value: Option<String>) -> Result<(), String> {
        self.map
            .lock()
            .expect("lock")
            .get_mut(name)
            .map(|s| {
                match value {
                    Some(v) => {
                        s.env.insert(key.to_string(), v);
                    }
                    None => {
                        s.env.remove(key);
                    }
                }
            })
            .ok_or_else(|| format!("会话 {name} 不存在"))
    }
}

const SUMMARY_LIMIT: usize = 8 * 1024;

fn truncate(s: &str) -> String {
    if s.len() <= SUMMARY_LIMIT {
        s.to_string()
    } else {
        let mut end = SUMMARY_LIMIT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[输出已截断]", &s[..end])
    }
}

/// 执行一条命令（共享实现，供测试直接调用）。
pub async fn run_command(
    state: &ShellState,
    command: &str,
    timeout_secs: u64,
) -> Result<(u16, String, String), String> {
    let (program, args) = state.kind.spawn_args(command);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args).current_dir(&state.cwd);
    // R4-5 凭据洗涤：父进程环境剥离 KEY/PASSWORD/SECRET/TOKEN 形命名后回灌
    // （保留 PATH 等系统变量），再叠加 shell 会话显式 env（set_env 注入项）。
    cmd.env_clear();
    for (k, v) in toolkit_core::sandbox::scrubbed_env() {
        cmd.env(k, v);
    }
    for (k, v) in &state.env {
        if !toolkit_core::sandbox::is_credential_var(k) {
            cmd.env(k, v);
        }
    }
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());

    let timeout = timeout_secs.clamp(1, 300);
    let child = cmd.spawn().map_err(|e| format!("进程启动失败: {e}"))?;
    let output = match tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("执行失败: {e}")),
        Err(_) => {
            return Err(format!("执行超时（>{timeout}s）；长任务请拆分或调大 timeout_secs"));
        }
    };
    let code = output.status.code().unwrap_or(-1) as u16;
    // Windows 控制台输出按系统 ANSI 代码页解释不可靠；直接按 UTF-8 损失容忍
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((code, truncate(&stdout), truncate(&stderr)))
}

use std::time::Duration;

// ---------------- shell.run ----------------

/// shell.run：命名会话中执行命令。
pub struct ShellRun(pub Arc<ShellSessions>, pub Arc<JobStore>);

#[async_trait]
impl Tool for ShellRun {
    fn name(&self) -> &str {
        "shell.run"
    }
    fn description(&self) -> String {
        "在持久 shell 会话中执行一条命令（cmd 或 powershell，cwd/环境变量跨命令保留）。\
参数：command(必填)、session(可选,默认 default)、shell(可选,cmd/powershell,首次创建用)、cwd(可选,切目录)、\
        timeout_secs(可选,1-300 默认 60)、env(可选,对象,本条命令临时环境变量)、\
background(可选,true=转后台执行,立即返回 job_id,用 shell.jobs 读取输出/终止——长扫描/长安装建议后台)。\
风险按命令判定：业务白名单（基座默认空）内的程序免审；未知/破坏性命令需人工审批。"
            .into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": { "type": "string" },
                "session": { "type": "string" },
                "shell": { "type": "string", "enum": ["cmd", "powershell"] },
                "cwd": { "type": "string" },
                "timeout_secs": { "type": "number" },
                "env": { "type": "object" },
                "background": { "type": "boolean", "description": "true=后台执行（立即返回 job_id）" }
            }
        })
    }
    fn risk(&self) -> kern_core::model::RiskLevel {
        // 静态典型档位（UI 展示）；实际审批判定走 risk_for 参数级分类（R1-1）。
        kern_core::model::RiskLevel::Medium
    }
    fn risk_for(&self, args: &serde_json::Value) -> kern_core::model::RiskLevel {
        toolkit_core::risk_policy::classify_shell_args(args)
    }
    async fn call(&self, _ctx: &kern_core::model::ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(command) = args.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) else {
            return ToolResult::error("缺少参数 command", ms());
        };
        let session = args.get("session").and_then(Value::as_str).unwrap_or("default").to_string();
        let timeout = args.get("timeout_secs").and_then(Value::as_u64).unwrap_or(60);
        // 会话不存在则按参数创建
        if self.0.get_state(&session).is_none() {
            let kind = args
                .get("shell")
                .and_then(Value::as_str)
                .and_then(ShellKind::parse)
                .unwrap_or(ShellKind::Cmd);
            let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or("");
            if let Err(e) = self.0.create(&session, kind, cwd) {
                return ToolResult::error(e, ms());
            }
        }
        // 临时切 cwd（持久）+ 临时 env（仅本条命令）
        if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
            if let Err(e) = self.0.set_cwd(&session, cwd) {
                return ToolResult::error(e, ms());
            }
        }
        let temp_env: HashMap<String, String> = args
            .get("env")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default();

        let Some(mut state) = self.0.get_state(&session) else {
            return ToolResult::error(format!("会话 {session} 不存在"), ms());
        };
        for (k, v) in &temp_env {
            state.env.insert(k.clone(), v.clone());
        }
        // R5-1：后台模式——spawn 后立即返回 job_id，输出经 shell.jobs 增量读取
        if args.get("background").and_then(Value::as_bool).unwrap_or(false) {
            match self.1.spawn(&state, command) {
                Ok(job_id) => {
                    self.0.bump(&session);
                    return ToolResult::json(
                        format!("已转入后台执行，job_id={job_id}。用 shell.jobs(action=output, job_id={job_id}) 增量读取输出；完成后读取 done=true 与退出码。"),
                        json!({ "job_id": job_id, "background": true }),
                        ms(),
                    );
                }
                Err(e) => return ToolResult::error(e, ms()),
            }
        }
        let result = run_command(&state, command, timeout).await;
        // 读取子进程退出后的 cwd 变化无法跨进程感知；支持 cd 内建同步：
        if let Ok((code, stdout, stderr)) = &result {
            let _ = (code, stdout, stderr);
            // cd 同步：命令形如 "cd <dir>"（cmd/PowerShell）时把 cwd 同步到目标
            let trimmed = command.trim();
            if let Some(rest) = trimmed.strip_prefix("cd ").or_else(|| trimmed.strip_prefix("cd")) {
                let target = rest.trim().trim_matches('"');
                if !target.is_empty() {
                    let _ = self.0.set_cwd(&session, target);
                }
            }
        }
        self.0.bump(&session);
        match result {
            Ok((code, stdout, stderr)) => {
                let cur = self.0.get_state(&session).map(|s| s.cwd).unwrap_or_default();
                ToolResult::json(
                    format!("shell.run {session} exit={code} cwd={cur}"),
                    json!({
                        "session": session, "exit_code": code, "cwd": cur,
                        "stdout": stdout, "stderr": stderr,
                        "duration_ms": ms(),
                    }),
                    ms(),
                )
            }
            Err(e) => ToolResult::error(e, ms()),
        }
    }
}

// ---------------- shell.jobs ----------------

/// R5-1：后台任务管理——list / output（增量读取）/ kill。
pub struct ShellJobs(pub Arc<JobStore>);

#[async_trait]
impl Tool for ShellJobs {
    fn name(&self) -> &str {
        "shell.jobs"
    }
    fn description(&self) -> String {
        "管理后台任务：action=list（全部任务概要：id/命令/状态/退出码）、action=output（job_id 必填，增量读取自上次读取后的新输出，done=true 表示已结束）、action=kill（job_id 必填，强制终止）。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": { "type": "string", "enum": ["list", "output", "kill"] },
                "job_id": { "type": "number" }
            }
        })
    }
    fn risk(&self) -> kern_core::model::RiskLevel {
        kern_core::model::RiskLevel::Low
    }
    fn is_concurrency_safe(&self, args: &Value) -> bool {
        // kill 有副作用，其余只读
        args.get("action").and_then(Value::as_str) != Some("kill")
    }
    async fn call(&self, _ctx: &kern_core::model::ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "list" => {
                self.0.reap();
                let mut rows = Vec::new();
                for (id, command, done, code, _started) in self.0.list() {
                    rows.push(json!({
                        "job_id": id, "command": command, "done": done,
                        "exit_code": code,
                        "status": if done { if code == 0 { "completed" } else { "exited" } } else { "running" },
                    }));
                }
                ToolResult::json(
                    if rows.is_empty() { "无后台任务".into() } else { format!("共 {} 个后台任务", rows.len()) },
                    json!({ "jobs": rows }),
                    ms(),
                )
            }
            "output" => {
                let Some(job_id) = args.get("job_id").and_then(Value::as_u64) else {
                    return ToolResult::error("缺少参数 job_id", ms());
                };
                match self.0.output(job_id) {
                    Some((pos, new_text, done, code)) => ToolResult::json(
                        format!("job {job_id} 新增 {} 字符，done={done}，exit={code}", new_text.chars().count()),
                        json!({ "job_id": job_id, "pos": pos, "new_output": new_text, "done": done, "exit_code": code }),
                        ms(),
                    ),
                    None => ToolResult::error(format!("job {job_id} 不存在"), ms()),
                }
            }
            "kill" => {
                let Some(job_id) = args.get("job_id").and_then(Value::as_u64) else {
                    return ToolResult::error("缺少参数 job_id", ms());
                };
                if self.0.kill(job_id).await {
                    ToolResult::text(format!("job {job_id} 已发送终止信号"), ms())
                } else {
                    ToolResult::error(format!("job {job_id} 不存在"), ms())
                }
            }
            _ => ToolResult::error(format!("未知 action: {action}（list/output/kill）"), ms()),
        }
    }
}

// ---------------- shell.sessions ----------------

/// shell.sessions：列出/删除/重置会话。
pub struct ShellSessionsTool(pub Arc<ShellSessions>);

#[async_trait]
impl Tool for ShellSessionsTool {
    fn name(&self) -> &str {
        "shell.sessions"
    }
    fn description(&self) -> String {
        "管理 shell 会话：列出全部（名称/类型/cwd/命令计数）、删除或重置指定会话。参数：list(可选,bool)、remove(可选,会话名)、reset(可选,对象{session,shell?,cwd?})".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "list": { "type": "boolean" },
                "remove": { "type": "string" },
                "reset": {
                    "type": "object",
                    "properties": {
                        "session": { "type": "string" },
                        "shell": { "type": "string" },
                        "cwd": { "type": "string" }
                    }
                }
            }
        })
    }
    fn risk(&self) -> kern_core::model::RiskLevel {
        kern_core::model::RiskLevel::Low
    }
    async fn call(&self, _ctx: &kern_core::model::ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        if let Some(name) = args.get("remove").and_then(Value::as_str) {
            return if self.0.remove(name) {
                ToolResult::text(format!("会话 {name} 已删除"), ms())
            } else {
                ToolResult::error(format!("会话 {name} 不存在"), ms())
            };
        }
        if let Some(reset) = args.get("reset").and_then(Value::as_object) {
            let Some(name) = reset.get("session").and_then(Value::as_str) else {
                return ToolResult::error("reset 缺少 session", ms());
            };
            let kind = reset
                .get("shell")
                .and_then(Value::as_str)
                .and_then(ShellKind::parse)
                .unwrap_or(ShellKind::Cmd);
            let cwd = reset.get("cwd").and_then(Value::as_str).unwrap_or("");
            return match self.0.create(name, kind, cwd) {
                Ok(()) => ToolResult::text(format!("会话 {name} 已重置为 {kind:?} cwd={cwd}"), ms()),
                Err(e) => ToolResult::error(e, ms()),
            };
        }
        let list: Vec<Value> = self
            .0
            .snapshot()
            .into_iter()
            .map(|(name, s)| {
                json!({
                    "session": name,
                    "shell": s.kind.as_str(),
                    "cwd": s.cwd,
                    "runs": s.run_count,
                    "env_keys": s.env.keys().cloned().collect::<Vec<_>>(),
                })
            })
            .collect();
        ToolResult::json(
            format!("shell 会话 {} 个", list.len()),
            json!({ "sessions": list }),
            ms(),
        )
    }
}

// ---------------- 组件 ----------------

/// shell-env 组件（一切皆插件）。
pub struct ShellEnvComponent;

impl Component for ShellEnvComponent {
    fn id(&self) -> &str {
        "shell-env"
    }
    fn kind(&self) -> ComponentKind {
        ComponentKind::Plugin
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> String {
        "持久 shell 环境：shell.run/shell.jobs/shell.sessions；会话保留 cwd 与环境变量；cmd/PowerShell；参数级风险判定".into()
    }
    fn status(&self) -> ComponentStatus {
        ComponentStatus::Loaded
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_lifecycle() {
        let s = ShellSessions::new();
        let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
        assert!(s.create("a", ShellKind::Cmd, &cwd).is_ok());
        assert!(s.create("b", ShellKind::PowerShell, "").is_ok());
        assert!(s.create("bad", ShellKind::Cmd, "Z:\\no_such_dir").is_err());
        assert_eq!(s.snapshot().len(), 2);
        s.bump("a");
        s.bump("a");
        let st = s.get_state("a").unwrap();
        assert_eq!(st.run_count, 2);
        assert_eq!(st.kind, ShellKind::Cmd);
        assert!(s.set_env("a", "K", Some("v".into())).is_ok());
        assert!(s.set_env("a", "K", None).is_ok());
        assert!(s.set_env("nope", "K", Some("v".into())).is_err());
        assert!(s.set_cwd("a", &cwd).is_ok());
        assert!(s.set_cwd("a", "Z:\\no_such_dir").is_err());
        assert!(s.remove("a"));
        assert!(!s.remove("a"));
    }

    #[test]
    fn spawn_args_shapes() {
        let (p, a) = ShellKind::Cmd.spawn_args("dir | findstr txt");
        assert_eq!(p, "cmd");
        assert_eq!(a, vec!["/D".to_string(), "/C".to_string(), "dir | findstr txt".to_string()]);
        let (p, a) = ShellKind::PowerShell.spawn_args("Get-ChildItem");
        assert_eq!(p, "powershell");
        assert!(a.contains(&"-NoProfile".to_string()));
        assert_eq!(ShellKind::parse("PWSH"), Some(ShellKind::PowerShell));
        assert_eq!(ShellKind::parse("sh"), None);
    }

    #[tokio::test]
    async fn run_echo_command() {
        let state = ShellState {
            kind: ShellKind::Cmd,
            cwd: std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            env: HashMap::new(),
            run_count: 0,
        };
        let (code, stdout, _stderr) = run_command(&state, "echo hello-shell", 30).await.unwrap();
        assert_eq!(code, 0);
        assert!(stdout.contains("hello-shell"));
        // 超时路径
        let e = run_command(&state, "ping -n 5 127.0.0.1 > nul", 1).await.unwrap_err();
        assert!(e.contains("超时"));
    }

    #[tokio::test]
    async fn env_injection() {
        let mut env = HashMap::new();
        env.insert("WPA_TEST_VAR".to_string(), "x7".to_string());
        let state = ShellState {
            kind: ShellKind::Cmd,
            cwd: std::env::current_dir().unwrap().to_string_lossy().into_owned(),
            env,
            run_count: 0,
        };
        let (code, stdout, _) = run_command(&state, "echo %WPA_TEST_VAR%", 30).await.unwrap();
        assert_eq!(code, 0);
        assert!(stdout.contains("x7"));
    }
}
