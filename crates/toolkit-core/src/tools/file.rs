//! 文件读写工具：file.read / file.write。
//!
//! 落点 = 当前工作区根（空间 cwd；无 cwd 时回落 exe 同级目录）。
//! - `file.read` 默认允许任意路径（含工作区外绝对路径）：读不改变系统状态，用户已拍板放开；
//! - `file.write` 默认只允许工作区根下 `scripts/`、`output/`、`data/` 相对路径；
//!   工作区外写入默认拒绝，仅空间「允许完全访问」时放开（绝对路径）。
//! 相对路径禁 `..`、盘符与反斜杠转义；保留 8.3 短名 / junction / canonicalize 三层围栏。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Value};

use crate::encoding::decode_best;
use crate::tools::{Tool, ToolResult};

/// 读回给 LLM 的全文上限（256KB，防上下文爆炸）。
const READ_LIMIT: usize = 256 * 1024;

/// 工作区根下允许写入的子目录（相对路径前缀，小写匹配）。
const WRITE_PREFIXES: &[&str] = &["scripts/", "output/", "data/"];

/// 判断是否"绝对路径形态"：`/x`、`\x`、`C:`（含 UNC `\\server\share`）。
fn looks_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with('\\')
        || (b.len() > 1 && b[1] == b':')
}

/// 统一斜杠后拼接根目录，并做三层围栏校验。
fn join_under(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let normalized = rel.replace('\\', "/");
    let joined = root.join(&normalized);
    fence_check(root, &joined)
}

/// 解析写白名单相对路径：仅允许 `scripts/`、`output/`、`data/` 前缀，禁 `..`/盘符。
fn resolve_writable(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let s = rel.trim();
    if s.is_empty() {
        return Err("path 为空".into());
    }
    if s.contains("..") {
        return Err(format!("path 禁止包含 `..`: {rel}"));
    }
    if s.contains('\0') {
        return Err(format!("path 含非法字符: {rel}"));
    }
    let bytes = s.as_bytes();
    if s.contains(':') && !(bytes.len() > 1 && bytes[1] == b':') {
        return Err(format!("path 禁止包含盘符或冒号: {rel}"));
    }
    if bytes.len() > 1 && bytes[1] == b':' {
        return Err(format!("path 禁止绝对路径/盘符: {rel}"));
    }
    let lower = s.replace('\\', "/").to_ascii_lowercase();
    if !WRITE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return Err(format!(
            "path 仅允许工作区下 scripts/、output/、data/ 相对路径: {rel}"
        ));
    }
    join_under(root, s)
}

/// R4-5（dsh X5 路径围栏强化）：三层检查——
/// ① 拒绝 8.3 短文件名（`ABCDEF~1` 可绕过前缀白名单匹配）；
/// ② 逐祖先拒绝符号链接/junction（root 之内出现即拒绝）；
/// ③ canonicalize 后必须仍在 canonical root 之内（防重解析点逃逸）。
/// 存在的路径返回 canonical 绝对路径；不存在的（写场景）返回原拼接路径。
fn fence_check(root: &Path, joined: &Path) -> Result<PathBuf, String> {
    // ① 8.3 短名：组件形如 `NAME~N`（~ 后全数字）
    for comp in joined.components() {
        if let std::path::Component::Normal(os) = comp {
            let s = os.to_string_lossy();
            // 8.3 短名形如 `LONGNA~1` / `CFG~1.TXT`：主名（最后一个 `.` 之前）尾部 `~N`（N 纯数字）
            let stem = s.split('.').next().unwrap_or(&s);
            if let Some(i) = stem.rfind('~') {
                let tail = &stem[i + 1..];
                if i > 0 && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("path 禁止 8.3 短文件名（防白名单前缀绕过）: {s}"));
                }
            }
        }
    }
    // ② 逐祖先 symlink/junction 检查（root 自身除外）
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("root 目录解析失败: {e}"))?;
    let mut cur = joined.to_path_buf();
    let mut chain = Vec::new();
    while cur.starts_with(root) && cur != root {
        chain.push(cur.clone());
        cur = match cur.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
    for p in chain.iter().rev() {
        if p.is_symlink() {
            return Err(format!("path 祖先包含符号链接/junction，拒绝: {}", p.display()));
        }
    }
    // ③ canonical 包含性兜底：路径已存在 → canonicalize 终点必须在 root 内；
    // 不存在（写场景）→ 最深存在祖先 canonicalize 后必须在 root 内。
    if joined.exists() {
        let canon = joined
            .canonicalize()
            .map_err(|e| format!("path 解析失败（{}）: {e}", joined.display()))?;
        if !canon.starts_with(&canon_root) {
            return Err(format!(
                "path 逃逸白名单根（重解析点/符号链接）: {} → {}",
                joined.display(),
                canon.display()
            ));
        }
        Ok(canon)
    } else {
        // 写场景：找到最深的存在祖先做 canonical 包含性校验
        let mut probe: PathBuf = joined.to_path_buf();
        while let Some(parent) = probe.parent() {
            if parent == root {
                break; // root 自身已 canonical 校验
            }
            if parent.exists() {
                let canon = parent
                    .canonicalize()
                    .map_err(|e| format!("path 解析失败（{}）: {e}", parent.display()))?;
                if !canon.starts_with(&canon_root) {
                    return Err(format!(
                        "path 祖先逃逸白名单根: {} → {}",
                        parent.display(),
                        canon.display()
                    ));
                }
                break;
            }
            probe = parent.to_path_buf();
        }
        Ok(joined.to_path_buf())
    }
}

/// Tool: file.read —— 读取白名单内文本文件（多编码梯度解码）。
pub struct FileReadTool {
    root: PathBuf,
}

impl FileReadTool {
    pub fn new() -> Self {
        Self { root: default_root() }
    }
    /// 注入工作区根（生产为当前空间 cwd；测试亦可）。
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// 读路径解析：绝对路径默认允许（用户拍板：file.read 默认放开）；
    /// 相对路径按工作区根解析，仍禁 `..` 与 NUL。
    fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let s = rel.trim();
        if s.is_empty() {
            return Err("path 为空".into());
        }
        if s.contains('\0') {
            return Err(format!("path 含非法字符: {rel}"));
        }
        if looks_absolute(s) {
            let p = PathBuf::from(s);
            return Ok(p.canonicalize().unwrap_or(p));
        }
        if s.contains("..") {
            return Err(format!("path 禁止包含 `..`: {rel}"));
        }
        join_under(&self.root, s)
    }
}

impl Default for FileReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str { "file.read" }
    fn description(&self) -> String {
        "读取文件（多编码自动识别，UTF-8/GBK/Big5）。path 可为工作区内相对路径，也可为绝对路径\
（默认允许读工作区外文件）。全文上限 256KB。用途：读回脚本输出、检查项目文件与下载内容。低风险。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "工作区内相对路径（如 scripts/recon.py、output/report.md），或绝对路径"}
            }
        })
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }

    fn is_concurrency_safe(&self, _args: &Value) -> bool {
        true
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(rel) = args.get("path").and_then(Value::as_str) else {
            return ToolResult::error("缺少参数 path", ms());
        };
        let path = match self.resolve(rel) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms()),
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("读取失败 {}: {e}", path.display()), ms()),
        };
        let size = bytes.len();
        let truncated = size > READ_LIMIT;
        let slice = if truncated { &bytes[..READ_LIMIT] } else { &bytes[..] };
        let content = decode_best(slice);
        let summary = if truncated {
            format!("已读取 {}（{}B，截断至 {READ_LIMIT}B）", rel.trim(), size)
        } else {
            format!("已读取 {}（{size}B）", rel.trim())
        };
        ToolResult::json(
            summary,
            json!({
                "path": rel.trim(),
                "size_bytes": size,
                "truncated": truncated,
                "content": content,
            }),
            ms(),
        )
    }
}

/// Tool: file.write —— 写工作区内文件（自动建父目录）。
pub struct FileWriteTool {
    root: PathBuf,
    /// 是否允许工作区外写入（仅空间「允许完全访问」时为 true）。
    allow_outside: bool,
}

impl FileWriteTool {
    pub fn new() -> Self {
        Self { root: default_root(), allow_outside: false }
    }
    /// 注入工作区根（生产为当前空间 cwd；测试亦可）。
    pub fn with_root(root: PathBuf) -> Self {
        Self { root, allow_outside: false }
    }
    /// 完全访问空间：放开工作区外写入（绝对路径）。
    pub fn with_allow_outside(mut self, allow: bool) -> Self {
        self.allow_outside = allow;
        self
    }

    /// 写路径解析：默认只允许工作区根下 scripts//output//data/；
    /// 绝对路径必须 allow_outside（完全访问），否则拒绝。
    fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let s = rel.trim();
        if looks_absolute(s) {
            if !self.allow_outside {
                return Err(format!(
                    "拒绝写入工作区外路径（{}）：当前空间未开启「允许完全访问」",
                    rel.trim()
                ));
            }
            if s.contains('\0') {
                return Err(format!("path 含非法字符: {rel}"));
            }
            return Ok(PathBuf::from(s));
        }
        resolve_writable(&self.root, s)
    }
}

impl Default for FileWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str { "file.write" }
    fn description(&self) -> String {
        "写文件到当前工作区：默认只允许 scripts/（脚本）、output/（报告/产物）、data/（数据）下相对路径；\
禁 .. / 盘符；自动创建父目录，覆盖写。工作区外绝对路径默认拒绝，仅空间「允许完全访问」时放开。\
参数：path(必填)、content(必填)、overwrite(可选,bool,默认 true)。返回写入字节数。中风险。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string", "description": "工作区内相对路径：脚本写 scripts/x.py，报告写 output/report.md，数据写 data/x.csv"},
                "content": {"type": "string", "description": "写入的文本内容"},
                "overwrite": {"type": "boolean", "description": "目标已存在时是否覆盖，默认 true"}
            }
        })
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Medium }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(rel) = args.get("path").and_then(Value::as_str) else {
            return ToolResult::error("缺少参数 path", ms());
        };
        let Some(content) = args.get("content").and_then(Value::as_str) else {
            return ToolResult::error("缺少参数 content", ms());
        };
        let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(true);
        let path = match self.resolve(rel) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e, ms()),
        };
        if !overwrite && path.is_file() {
            return ToolResult::error(format!("目标已存在且 overwrite=false: {}", path.display()), ms());
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::error(format!("创建父目录失败: {e}"), ms());
            }
        }
        match tokio::fs::write(&path, content).await {
            Ok(()) => {}
            Err(e) => return ToolResult::error(format!("写入失败 {}: {e}", path.display()), ms()),
        }
        ToolResult::json(
            format!("已写入 {}（{}B）", rel.trim(), content.len()),
            json!({
                "path": rel.trim(),
                "bytes_written": content.len(),
                "absolute": path.display().to_string(),
            }),
            ms(),
        )
    }
}

/// exe 同级根目录（与 pysandbox::app_root 同语义）。
fn default_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: kern_core::model::SessionId::new(),
            agent_id: kern_core::model::AgentId::new("t"),
            task_id: kern_core::model::TaskId::new("t"),
            target: "127.0.0.1".into(),
            risk: RiskLevel::Medium,
            approved: true,
        }
    }

    fn tmp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wpa_file_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn whitelist_rejects() {
        let t = FileWriteTool::with_root(PathBuf::from("/fake"));
        for bad in [
            "../etc/x.py", "a/../b.py", "C:\\x.py", "D:/x.py", "/abs/x.py",
            "\\abs\\x.py", "other/x.txt", "scripts2/a.py", "",
        ] {
            let r = t.call(&ctx(), json!({"path": bad, "content": "x"})).await;
            assert!(!r.ok, "应拒绝 {bad}");
        }
        let rt = FileReadTool::with_root(PathBuf::from("/fake"));
        for bad in ["", "..", "a/../b", "scripts/\0x"] {
            let r = rt.call(&ctx(), json!({"path": bad})).await;
            assert!(!r.ok, "应拒绝 {bad}");
        }
    }

    /// file.read 默认允许读工作区外绝对路径（用户拍板）。
    #[tokio::test]
    async fn read_allows_outside_absolute_path() {
        let root = tmp_root("read_inside");
        let outside = tmp_root("read_outside");
        std::fs::write(outside.join("note.txt"), "外部文件").unwrap();
        let rt = FileReadTool::with_root(root.clone());
        let abs = outside.join("note.txt").display().to_string();
        let r = rt.call(&ctx(), json!({"path": abs})).await;
        assert!(r.ok, "{}", r.summary);
        assert_eq!(r.detail.unwrap()["content"].as_str().unwrap(), "外部文件");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// file.write 默认拒绝工作区外；完全访问空间放开。
    #[tokio::test]
    async fn write_outside_denied_unless_unrestricted() {
        let root = tmp_root("write_inside");
        let outside = tmp_root("write_outside");
        let target = outside.join("evil.txt").display().to_string();

        let w = FileWriteTool::with_root(root.clone());
        let r = w.call(&ctx(), json!({"path": target, "content": "x"})).await;
        assert!(!r.ok, "默认应拒绝工作区外写入");

        let w = FileWriteTool::with_root(root.clone()).with_allow_outside(true);
        let r = w.call(&ctx(), json!({"path": target, "content": "x"})).await;
        assert!(r.ok, "完全访问应允许：{}", r.summary);
        assert!(outside.join("evil.txt").is_file());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 产物约定：报告写 output/、脚本写 scripts/，均落在当前工作区根。
    #[tokio::test]
    async fn write_artifacts_under_workspace() {
        let root = tmp_root("artifacts");
        let w = FileWriteTool::with_root(root.clone());
        for p in ["output/report.md", "scripts/scan.py", "data/raw.csv"] {
            let r = w.call(&ctx(), json!({"path": p, "content": "ok"})).await;
            assert!(r.ok, "{p}: {}", r.summary);
            assert!(root.join(p).is_file(), "应落在工作区：{p}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R4-5：8.3 短文件名拒绝（`ABCDEF~1` 可绕过前缀白名单）。
    #[test]
    fn fence_rejects_83_short_names() {
        // fence 需 canonicalize，root 须真实存在
        let root = tmp_root("fence");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        for bad in ["scripts/abcdef~1", "data/CFG~1.TXT", "scripts/x~9/y.py"] {
            assert!(
                resolve_writable(&root, bad).is_err(),
                "应拒绝 8.3 短名: {bad}"
            );
        }
        // 正常名不受影响（~ 出现在词中且后缀非纯数字不算短名）
        assert!(resolve_writable(&root, "scripts/a~b.txt").is_ok());
        assert!(resolve_writable(&root, "scripts/report~final.txt").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R4-5：凭据洗涤——KEY/PASSWORD/SECRET/TOKEN/CREDENTIAL 形命名被剥离。
    #[test]
    fn credential_var_detection() {
        for name in ["API_KEY", "db_password", "MY_SECRET", "auth-token", "client_CREDENTIALS", "PASSWD_FILE"] {
            assert!(crate::sandbox::is_credential_var(name), "{name} 应识别为凭据");
        }
        for name in ["PATH", "SystemRoot", "PYTHONPATH", "HOME", "TEMP", "USERNAME"] {
            assert!(!crate::sandbox::is_credential_var(name), "{name} 不应误判");
        }
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let root = tmp_root("rw");
        let w = FileWriteTool::with_root(root.clone());
        let r = w.call(&ctx(), json!({
            "path": "scripts/sub/hello.py",
            "content": "print('中文输出')"
        })).await;
        assert!(r.ok, "{}", r.summary);
        let d = r.detail.unwrap();
        assert_eq!(d["bytes_written"].as_u64().unwrap(), "print('中文输出')".len() as u64);
        let written = root.join("scripts/sub/hello.py");
        assert!(written.is_file(), "应自动创建父目录");

        let rd = FileReadTool::with_root(root.clone());
        let r = rd.call(&ctx(), json!({"path": "scripts/sub/hello.py"})).await;
        assert!(r.ok, "{}", r.summary);
        let d = r.detail.unwrap();
        assert_eq!(d["content"].as_str().unwrap(), "print('中文输出')");

        // overwrite=false 拒绝覆盖
        let r = w.call(&ctx(), json!({"path": "scripts/sub/hello.py", "content": "x", "overwrite": false})).await;
        assert!(!r.ok, "overwrite=false 应拒绝");
        // 覆盖写成功
        let r = w.call(&ctx(), json!({"path": "scripts/sub/hello.py", "content": "x"})).await;
        assert!(r.ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn read_gbk_file() {
        let root = tmp_root("gbk");
        let p = root.join("data");
        let _ = std::fs::create_dir_all(&p);
        // "密" GBK = 0xC3 0xDC
        std::fs::write(root.join("data/gbk.txt"), [0xc3u8, 0xdc]).unwrap();
        let rd = FileReadTool::with_root(root.clone());
        let r = rd.call(&ctx(), json!({"path": "data/gbk.txt"})).await;
        assert!(r.ok, "{}", r.summary);
        let d = r.detail.unwrap();
        assert_eq!(d["content"].as_str().unwrap(), "密");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tool_meta() {
        assert_eq!(FileReadTool::new().name(), "file.read");
        assert_eq!(FileReadTool::new().risk(), RiskLevel::Low);
        assert_eq!(FileWriteTool::new().name(), "file.write");
        assert_eq!(FileWriteTool::new().risk(), RiskLevel::Medium);
    }
}
