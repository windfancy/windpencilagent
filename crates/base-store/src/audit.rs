//! 审计日志：管理操作与工具调用的结构化留痕。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 审计记录（不可变，append-only 语义由调用方保证）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts_epoch_ms: u64,
    pub session_id: String,
    pub task_id: Option<String>,
    pub operator: String,
    /// 动作类别：tool / approval / takeover / interrupt / admin
    pub category: String,
    pub action: String,
    pub target: Option<String>,
    pub args_summary: Option<String>,
    pub outcome: String,
}

impl AuditRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: &str,
        task_id: Option<String>,
        operator: &str,
        category: &str,
        action: &str,
        target: Option<String>,
        args_summary: Option<String>,
        outcome: &str,
    ) -> Self {
        let ts_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            ts_epoch_ms,
            session_id: session_id.into(),
            task_id,
            operator: operator.into(),
            category: category.into(),
            action: action.into(),
            target,
            args_summary,
            outcome: outcome.into(),
        }
    }
}

/// 审计日志错误。
#[derive(Debug, Error)]
pub enum AuditError {
    #[error("io: {0}")]
    Io(std::io::Error),
}

/// 审计日志：JSONL 追加写入 + 可导出。
pub struct AuditLog {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    records: std::sync::Mutex<Vec<AuditRecord>>,
}

impl AuditLog {
    /// 建立审计日志。`path` 可为 None（仅内存，供测试）。
    pub fn open(path: Option<impl AsRef<Path>>) -> Result<Self, AuditError> {
        let (file, path) = match path {
            Some(p) => {
                let pb = p.as_ref().to_path_buf();
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&pb)
                    .map_err(AuditError::Io)?;
                (Some(f), Some(pb))
            }
            None => (None, None),
        };
        Ok(Self {
            file,
            path,
            records: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// 追加一条记录（写文件 + 内存缓冲）。
    pub fn append(&self, rec: AuditRecord) -> Result<(), AuditError> {
        {
            let mut buf = self.records.lock().unwrap();
            buf.push(rec.clone());
        }
        if let Some(f) = &self.file {
            // 立即刷写当前记录到文件尾部（内存缓冲保留，供 export 导出全量）
            let line = serde_json::to_string(&rec).unwrap_or_default();
            let mut writer = f;
            writer.write_all(line.as_bytes()).map_err(AuditError::Io)?;
            writer.write_all(b"\n").map_err(AuditError::Io)?;
            writer.flush().map_err(AuditError::Io)?;
        }
        Ok(())
    }

    /// 当前路径。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 导出全部记录（从内存扇区）。磁盘持久化见 `export_jsonl`。
    pub fn export_jsonl(&self, out: impl AsRef<Path>) -> Result<usize, AuditError> {
        let mut f = std::fs::File::create(out).map_err(AuditError::Io)?;
        let guard = self.records.lock().unwrap();
        for r in guard.iter() {
            let line = serde_json::to_string(r).unwrap_or_default();
            f.write_all(line.as_bytes()).map_err(AuditError::Io)?;
            f.write_all(b"\n").map_err(AuditError::Io)?;
        }
        Ok(guard.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_append_and_export() {
        let tmp = std::env::temp_dir().join(format!("audit-{}.log", uuid_short()));
        let log = AuditLog::open(Some(&tmp)).unwrap();
        let rec = AuditRecord::new("s1", Some("t1".into()), "op", "tool", "net.dns", Some("10.0.0.1".into()), Some("{\"host\":\"a\"}".into()), "ok");
        log.append(rec).unwrap();
        let out = std::env::temp_dir().join(format!("audit-export-{}.jsonl", uuid_short()));
        let n = log.export_jsonl(&out).unwrap();
        assert!(n >= 1);
        std::fs::remove_file(&out).ok();
        std::fs::remove_file(&tmp).ok();
    }

    fn uuid_short() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos().to_string())
            .unwrap_or_else(|_| "0".into())
    }
}
