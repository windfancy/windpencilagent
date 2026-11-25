//! 文件读写工具：file.read / file.write。
//!
//! 落点白名单 = exe 同级 `scripts/`、`data/` 下相对路径（工作空间 cwd 不可得，暂不开放）；
//! 禁 `..`、绝对路径、盘符、反斜杠转义。file.write 是脚本落盘入口
//! 「写 Python 脚本进 scripts/」的入口，配合 sandbox.pip + sandbox.python 形成闭环。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Value};

use crate::encoding::decode_best;
use crate::tools::{Tool, ToolResult};

/// 读回给 LLM 的全文上限（256KB，防上下文爆炸）。
const READ_LIMIT: usize = 256 * 1024;

/// 解析白名单相对路径：仅允许 `scripts/`、`data/` 前缀，禁 `..`/绝对路径/盘符。
/// 返回 root 下的绝对路径。
fn resolve_whitelisted(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let s = rel.trim();
    if s.is_empty() {
        return Err("path 为空".into());
    }
    if s.contains("..") {
        return Err(format!("path 禁止包含 `..`: {rel}"));
    }
    if s.starts_with('/') || s.starts_with('\\') || s.contains('\0') {
        return Err(format!("path 须为 scripts/ 或 data/ 下相对路径: {rel}"));
    }
    // 盘符：C: / c:\ 等（第二字符为冒号即视为 Windows 盘符）
    let bytes = s.as_bytes();
    if s.contains(':') && !(bytes.len() > 1 && bytes[1] == b':') {
        // 单冒号出现在其他位置同样可疑（如 data/a:b）——统一拒绝
        return Err(format!("path 禁止包含盘符或冒号: {rel}"));
    }
    if bytes.len() > 1 && bytes[1] == b':' {
        return Err(format!("path 禁止绝对路径/盘符: {rel}"));
    }
    // 前缀白名单
    let lower = s.to_ascii_lowercase();
    if !(lower.starts_with("scripts/") || lower.starts_with("scripts\\")
        || lower.starts_with("data/") || lower.starts_with("data\\")) {
        return Err(format!("path 仅允许 scripts/ 或 data/ 下相对路径: {rel}"));
    }
    // 统一用正斜杠拼路径，避免反斜杠绕过前缀检查
    let normalized = s.replace('\\', "/");
    let joined = root.join(&normalized);
    fence_check(root, &joined)
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
    /// 注入根目录（测试用；生产为 exe 同级目录）。
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
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
        "读取文件（多编码自动识别，UTF-8/GBK/Big5）：落点白名单为 exe 同级 scripts/、data/ 下相对路径，\
禁 .. / 绝对路径 / 盘符。全文上限 256KB。用途：读回脚本输出、检查下载内容。低风险。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "相对路径，仅允许 scripts/ 或 data/ 前缀，如 scripts/recon.py、data/downloads/report.html"}
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
        let path = match resolve_whitelisted(&self.root, rel) {
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

/// Tool: file.write —— 写白名单内文件（自动建父目录）。
pub struct FileWriteTool {
    root: PathBuf,
}

impl FileWriteTool {
    pub fn new() -> Self {
        Self { root: default_root() }
    }
    /// 注入根目录（测试用；生产为 exe 同级目录）。
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
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
        "写文件（把脚本写进 scripts/ 后用 sandbox.python 执行）：\
落点白名单为 exe 同级 scripts/、data/ 下相对路径，禁 .. / 绝对路径 / 盘符；自动创建父目录，覆盖写。\
参数：path(必填)、content(必填)、overwrite(可选,bool,默认 true)。返回写入字节数。中风险。".into()
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string", "description": "相对路径，仅允许 scripts/ 或 data/ 前缀，如 scripts/scan.py"},
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
        let path = match resolve_whitelisted(&self.root, rel) {
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
        for bad in ["../etc/x.py", "a/../b.py", "C:\\x.py", "D:/x.py", "/abs/x.py", "\\abs\\x.py", "other/x.txt", "scripts2/a.py", ""] {
            let r = t.call(&ctx(), json!({"path": bad, "content": "x"})).await;
            assert!(!r.ok, "应拒绝 {bad}");
        }
        let rt = FileReadTool::with_root(PathBuf::from("/fake"));
        for bad in ["..", "C:x", "foo.py"] {
            let r = rt.call(&ctx(), json!({"path": bad})).await;
            assert!(!r.ok, "应拒绝 {bad}");
        }
    }

    /// R4-5：8.3 短文件名拒绝（`ABCDEF~1` 可绕过前缀白名单）。
    #[test]
    fn fence_rejects_83_short_names() {
        // fence 需 canonicalize，root 须真实存在
        let root = tmp_root("fence");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        for bad in ["scripts/abcdef~1", "data/CFG~1.TXT", "scripts/x~9/y.py"] {
            assert!(
                resolve_whitelisted(&root, bad).is_err(),
                "应拒绝 8.3 短名: {bad}"
            );
        }
        // 正常名不受影响（~ 出现在词中且后缀非纯数字不算短名）
        assert!(resolve_whitelisted(&root, "scripts/a~b.txt").is_ok());
        assert!(resolve_whitelisted(&root, "scripts/report~final.txt").is_ok());
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
