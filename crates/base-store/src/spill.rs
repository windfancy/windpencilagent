//! spill：工具结果全文落盘（R1-2，对标 dsh spill 机制）。
//!
//! 超过阈值的工具 detail 全文落盘到 `data/spill/<session>/`，模型可见内容只保留
//! 头尾与 locator；模型可用 `file.read`（白名单含 data/）按需回读全文。
//! 保留 30 天由清扫函数提供（与 dsh spill-local 一致的刻意保留策略）。

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// 文件名清洗：session/tool 只保留安全字符，其余替换为 `_`。
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// 工具结果溢出存储。`root` = file.read 白名单的 data/ 根（exe 同级）。
#[derive(Debug, Clone)]
pub struct SpillStore {
    root: PathBuf,
}

impl SpillStore {
    /// 以 data/ 根构造（溢出文件落在 `data/spill/` 下，file.read 可直读）。
    pub fn with_data_root(data_root: impl Into<PathBuf>) -> Self {
        Self { root: data_root.into().join("spill") }
    }

    /// 保存全文，返回 `file.read` 可用的相对路径 locator：
    /// `data/spill/<session>/<seq>_<tool>.txt`（data/ 为白名单根，固定前缀）。
    pub fn save(&self, session: &str, seq: u64, tool: &str, text: &str) -> io::Result<String> {
        let dir = self.root.join(sanitize(session));
        fs::create_dir_all(&dir)?;
        let name = format!("{}_{}.txt", seq, sanitize(tool));
        fs::write(dir.join(&name), text)?;
        Ok(format!("data/spill/{}/{}", sanitize(session), name))
    }

    /// 清扫超期溢出文件（默认保留 30 天），返回删除的文件数。
    pub fn cleanup(&self, keep: Duration) -> io::Result<usize> {
        let now = SystemTime::now();
        let mut removed = 0usize;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                for f in fs::read_dir(entry.path())? {
                    let f = f?;
                    if is_expired(f.path(), now, keep) {
                        let _ = fs::remove_file(f.path());
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }
}

fn is_expired(path: PathBuf, now: SystemTime, keep: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .map(|age| age > keep)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("wcb_spill_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn save_returns_readable_locator() {
        let root = tmp_root("save");
        let store = SpillStore::with_data_root(&root);
        let loc = store.save("sess-1", 7, "os.exec", &"A".repeat(50_000)).unwrap();
        assert!(loc.starts_with("data/spill/"), "locator={loc}");
        // 相对路径可被 file.read 白名单规则接受（data/ 前缀）
        assert!(loc.to_ascii_lowercase().starts_with("data/spill/"));
        // 落盘内容完整（locator 去掉 data/ 前缀即相对 data_root 的真实路径）
        let full = root.join(loc.trim_start_matches("data/"));
        assert_eq!(fs::read_to_string(full).unwrap().len(), 50_000);
    }

    #[test]
    fn session_and_tool_names_sanitized() {
        let root = tmp_root("san");
        let store = SpillStore::with_data_root(&root);
        let loc = store.save("s/../x", 1, "kb.search", "t").unwrap();
        assert!(!loc.contains(".."));
        assert!(loc.contains("kb_search"));
    }

    #[test]
    fn cleanup_removes_expired_only() {
        let root = tmp_root("clean");
        let store = SpillStore::with_data_root(&root);
        store.save("a", 1, "t", "x").unwrap();
        // 保留窗口极大：不删
        assert_eq!(store.cleanup(Duration::from_secs(86_400 * 365)).unwrap(), 0);
        // 保留窗口为 0：全删
        assert_eq!(store.cleanup(Duration::ZERO).unwrap(), 1);
    }
}
