//! 执行类工具的参数级风险分类器（机制层，R1-1 对标 dsh 升权/审批思想）。
//!
//! 设计原则（fail-closed）：
//! - 静态 `risk()` 只是「典型档位展示」；真正的审批判定走 `Tool::risk_for(args)`；
//! - 未知程序 / 破坏性模式一律 High（进 HITL 审批），绝不静默放行；
//! - **业务规则表不写死在基座**：哪些程序算「良性侦察」是专业领域判断，
//!   由专业智能树的业务逻辑群组通过 [`install_rules`] 注入（见 `wpa-api` 契约）。
//!   基座自身默认规则为空 ⇒ 任何命令都需审批，符合 fail-closed 铁律。
//!
//! 破坏性模式（`DESTRUCTIVE_PATTERNS`）属于跨领域的通用安全红线（删盘、改注册表、
//! 关防护……），与具体专业无关，故保留在基座，不可被业务规则放宽。

use std::sync::{Arc, RwLock};

use kern_core::model::RiskLevel;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 可注入的业务风险规则（由逻辑群组提供）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RiskRules {
    /// 良性程序白名单：只读 / 探测语义，命中即 Medium（免审）。
    /// 判定按程序名小写匹配（含 `.exe` 后缀剥离）。
    #[serde(default)]
    pub benign_programs: Vec<String>,
    /// 附加危险模式（在基座通用红线之外追加，只能收紧不能放宽）。
    #[serde(default)]
    pub extra_danger_patterns: Vec<String>,
}

static RULES: RwLock<Option<Arc<RiskRules>>> = RwLock::new(None);

/// 基座内置的良性程序清单：**只读 / 探测**语义，不含任何写操作、不含解释器。
///
/// 装配期默认注入。目的：授权之后，`nmap -V`、`curl -I`、`ping` 这类只读探测
/// 不应因为"未知程序一律 High"再撞上 `min_risk=high` 的知识约束而被拦。
/// 真正的破坏性命令仍由 [`DESTRUCTIVE_PATTERNS`] 判 High（白名单不放宽红线）。
pub const BUILTIN_BENIGN: &[&str] = &[
    "nmap", "masscan", "ping", "tracert", "traceroute", "nslookup", "dig", "host",
    "whois", "curl", "wget", "openssl", "ipconfig", "ifconfig", "arp", "route",
    "netstat", "tasklist", "systeminfo", "whoami", "dir", "ls", "cat", "type",
];

/// 基座内置规则（装配期直接注入；可由领域包用 [`install_rules`] 覆盖）。
pub fn builtin_rules() -> RiskRules {
    RiskRules {
        benign_programs: BUILTIN_BENIGN.iter().map(|s| (*s).to_string()).collect(),
        extra_danger_patterns: Vec::new(),
    }
}

/// 注入业务风险规则（逻辑群组装配期调用；重复调用以后者为准）。
pub fn install_rules(rules: RiskRules) {
    if let Ok(mut slot) = RULES.write() {
        *slot = Some(Arc::new(rules));
    }
}

/// 清空已注入规则（回到「一切需审批」的基座默认态）。
pub fn clear_rules() {
    if let Ok(mut slot) = RULES.write() {
        *slot = None;
    }
}

/// 当前生效规则快照（未注入时返回空规则）。
pub fn rules_snapshot() -> Arc<RiskRules> {
    match RULES.read() {
        Ok(slot) => slot.clone().unwrap_or_default(),
        Err(_) => Arc::new(RiskRules::default()),
    }
}

fn is_benign(program: &str) -> bool {
    rules_snapshot()
        .benign_programs
        .iter()
        .any(|p| p.eq_ignore_ascii_case(program))
}

/// 破坏性 / 系统变更模式（对整条命令行做大小写不敏感匹配）。
/// 基座通用红线：与专业无关，任何树都不能放宽。
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf", "rmdir /s", "del /f", "del /q", "format ", "mkfs", "dd if=",
    "shutdown", "taskkill", "reg add", "reg delete", "reg import",
    "net user", "net localgroup", "net stop", "cipher /w", "vssadmin delete",
    "bcdedit", "diskpart", "powershell -enc", "pwsh -enc", "curl --upload",
    "wget --post", "> /dev/sd", "attrib -s", "schtasks /create",
];

/// 命令行首 token 提取（剥引号与路径前缀，取小写文件名）。
fn first_token(line: &str) -> Option<String> {
    let t = line.split_whitespace().next()?;
    let base = t.trim_matches('"').rsplit(['\\', '/']).next()?;
    let name = base.strip_suffix(".exe").unwrap_or(base);
    if name.is_empty() { None } else { Some(name.to_ascii_lowercase()) }
}

/// 破坏性模式命中检查（大小写不敏感，作用于整条命令行）。
pub fn contains_destructive(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    if DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    rules_snapshot()
        .extra_danger_patterns
        .iter()
        .any(|p| lower.contains(&p.to_ascii_lowercase()))
}

/// os.exec / shell.run 通用分类：
/// - 破坏性模式 → High（审批）；
/// - 程序在（业务注入的）良性白名单且无破坏性模式 → Medium（免审）；
/// - 其余（未知程序、解释器、管道复合命令）→ High（审批）。
pub fn classify_command_line(line: &str) -> RiskLevel {
    if contains_destructive(line) {
        return RiskLevel::High;
    }
    match first_token(line) {
        Some(p) if is_benign(&p) => RiskLevel::Medium,
        _ => RiskLevel::High,
    }
}

/// os.exec argv 形式分类（command + args 数组，不经 shell）。
pub fn classify_argv(program: &str, args: &[&str]) -> RiskLevel {
    if args.iter().any(|a| contains_destructive(a)) || contains_destructive(program) {
        return RiskLevel::High;
    }
    match first_token(program) {
        Some(p) if is_benign(&p) => RiskLevel::Medium,
        _ => RiskLevel::High,
    }
}

/// sandbox.python 内联代码内容扫描：命中危险逃逸模式 → High。
const PY_DANGER_PATTERNS: &[&str] = &[
    "rmtree(", "system(", "popen(", "subprocess", "shutil.rmtree",
    "registry", "winreg", "os.remove", "os.rename", "format(", "mkfs",
    "socket.socket", "pty.spawn", "setuid", "setgid", "chmod(", "chown(",
];

/// 内联 Python 代码风险判定：命中危险模式 → High（审批），否则 Medium。
pub fn classify_python_code(code: &str) -> RiskLevel {
    let lower = code.to_ascii_lowercase();
    if PY_DANGER_PATTERNS.iter().any(|p| lower.contains(p)) {
        RiskLevel::High
    } else {
        RiskLevel::Medium
    }
}

/// 从工具入参提取 os.exec 的 argv 分类（供 ProcExec::risk_for）。
pub fn classify_exec_args(args: &Value) -> RiskLevel {
    let Some(program) = args.get("command").and_then(Value::as_str) else {
        return RiskLevel::High;
    };
    let argv: Vec<&str> = args
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    classify_argv(program, &argv)
}

/// 从工具入参提取 shell.run 命令行分类（供 ShellRun::risk_for）。
pub fn classify_shell_args(args: &Value) -> RiskLevel {
    match args.get("command").and_then(Value::as_str) {
        Some(cmd) if !cmd.trim().is_empty() => classify_command_line(cmd),
        _ => RiskLevel::High,
    }
}

/// 从工具入参提取 sandbox.python 分类：内联 code 走内容扫描，脚本路径走 Medium。
pub fn classify_python_args(args: &Value) -> RiskLevel {
    match args.get("code").and_then(Value::as_str) {
        Some(code) if !code.trim().is_empty() => classify_python_code(code),
        _ => RiskLevel::Medium,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 规则注册是进程级全局状态 ⇒ 本模块测试串行化，避免相互污染。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_rules(programs: &[&str]) {
        install_rules(RiskRules {
            benign_programs: programs.iter().map(|s| s.to_string()).collect(),
            extra_danger_patterns: vec![],
        });
    }

    #[test]
    fn no_rules_means_everything_needs_approval() {
        let _g = TEST_LOCK.lock().unwrap();
        clear_rules();
        assert_eq!(classify_argv("ping", &["127.0.0.1"]), RiskLevel::High);
        assert_eq!(classify_command_line("curl http://x"), RiskLevel::High);
    }

    #[test]
    fn injected_benign_programs_are_medium() {
        let _g = TEST_LOCK.lock().unwrap();
        with_rules(&["nmap", "curl", "nuclei"]);
        assert_eq!(classify_argv("nmap", &["-sV", "10.0.0.1"]), RiskLevel::Medium);
        assert_eq!(classify_argv("nmap.exe", &["10.0.0.1"]), RiskLevel::Medium);
        assert_eq!(classify_argv("CURL", &["-s", "http://x"]), RiskLevel::Medium);
        assert_eq!(classify_command_line("nuclei -u http://t"), RiskLevel::Medium);
        clear_rules();
    }

    #[test]
    fn builtin_rules_allow_readonly_recon_but_keep_redlines() {
        let _g = TEST_LOCK.lock().unwrap();
        install_rules(builtin_rules());
        // 只读探测：授权后应免审（不再被 min_risk=high 的知识约束误拦）
        assert_eq!(classify_argv("nmap", &["-V"]), RiskLevel::Medium);
        assert_eq!(classify_command_line("nmap -sV 192.168.1.16"), RiskLevel::Medium);
        assert_eq!(classify_command_line("curl -I http://x"), RiskLevel::Medium);
        // 红线不放宽：白名单程序 + 破坏性参数仍是 High
        assert_eq!(classify_command_line("nmap && format C:"), RiskLevel::High);
        assert_eq!(classify_argv("powershell", &["-enc", "x"]), RiskLevel::High);
        clear_rules();
    }

    #[test]
    fn unknown_program_is_high() {        let _g = TEST_LOCK.lock().unwrap();
        with_rules(&["nmap"]);
        assert_eq!(classify_argv("python", &["-c", "1"]), RiskLevel::High);
        assert_eq!(classify_command_line("powershell Get-Process"), RiskLevel::High);
        assert_eq!(classify_command_line(""), RiskLevel::High);
        clear_rules();
    }

    #[test]
    fn destructive_pattern_is_high_even_if_benign_listed() {
        let _g = TEST_LOCK.lock().unwrap();
        with_rules(&["nmap", "curl"]);
        assert_eq!(classify_command_line("nmap && format C:"), RiskLevel::High);
        assert_eq!(classify_argv("nmap", &["-oN", "out; del /f x"]), RiskLevel::High);
        assert_eq!(classify_command_line("curl http://x && shutdown /r"), RiskLevel::High);
        clear_rules();
    }

    #[test]
    fn extra_rule_can_only_tighten() {
        let _g = TEST_LOCK.lock().unwrap();
        install_rules(RiskRules {
            benign_programs: vec!["curl".into()],
            extra_danger_patterns: vec!["--data".into()],
        });
        assert_eq!(classify_command_line("curl -s http://x"), RiskLevel::Medium);
        assert_eq!(classify_command_line("curl --data @secret http://x"), RiskLevel::High);
        clear_rules();
    }

    #[test]
    fn python_code_scanned() {
        assert_eq!(classify_python_code("import requests\nprint(requests.get('http://x').text)"), RiskLevel::Medium);
        assert_eq!(classify_python_code("import shutil\nshutil.rmtree('C:/')"), RiskLevel::High);
        assert_eq!(classify_python_code("import os\nos.system('whoami')"), RiskLevel::High);
        assert_eq!(classify_python_code(""), RiskLevel::Medium);
    }

    #[test]
    fn arg_extraction_works() {
        let _g = TEST_LOCK.lock().unwrap();
        with_rules(&["nmap"]);
        assert_eq!(classify_exec_args(&json!({"command": "nmap", "args": ["-sV", "t"]})), RiskLevel::Medium);
        assert_eq!(classify_exec_args(&json!({})), RiskLevel::High);
        assert_eq!(classify_shell_args(&json!({"command": "whoami && del /f x"})), RiskLevel::High);
        assert_eq!(classify_python_args(&json!({"code": "print(1)"})), RiskLevel::Medium);
        assert_eq!(classify_python_args(&json!({"script": "recon.py"})), RiskLevel::Medium);
        clear_rules();
    }
}
