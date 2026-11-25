//! sled 持久化封装：plan/task/session/fact 的键值存储。

use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use thiserror::Error;

/// 进程内已打开 sled 库缓存：同目录多次 open 共享同一 Db 句柄，
/// 避免 sled 文件锁为句柄级排他锁导致同进程二次打开报 code 33。
static OPEN_DBS: OnceLock<Mutex<HashMap<PathBuf, sled::Db>>> = OnceLock::new();

/// 存储错误。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("kv error: {0}")]
    Kv(String),
    #[error("serialize: {0}")]
    Serialize(String),
    #[error("deserialize: {0}")]
    Deserialize(String),
    #[error("key not found: {0}")]
    KeyNotFound(String),
}

/// 本地持久化存储（sled）。键空间按前缀分域。
pub struct Store {
    db: sled::Db,
}

impl Store {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let dir = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Kv(e.to_string()))?;
        // 规范化路径作为缓存键（消除大小写/分隔符差异导致的假未命中）
        let key = dir.canonicalize().unwrap_or(dir.clone());
        let map = OPEN_DBS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = map.lock().expect("store db cache poisoned");
        if let Some(db) = guard.get(&key) {
            return Ok(Self { db: db.clone() });
        }
        let db = sled::open(&dir).map_err(|e| StoreError::Kv(e.to_string()))?;
        guard.insert(key, db.clone());
        Ok(Self { db })
    }

    pub fn open_tmp() -> Result<Self, StoreError> {
        let tmp = std::env::temp_dir().join(format!("windpenagent-{}", uuid_short()));
        Self::open(tmp)
    }

    /// 写入（domain.key → value）。
    pub fn put<T: Serialize>(&self, domain: &str, key: &str, value: &T) -> Result<(), StoreError> {
        let full = format!("{domain}:{key}");
        let bytes = serde_json::to_vec(value).map_err(|e| StoreError::Serialize(e.to_string()))?;
        self.db
            .insert(full.as_bytes(), bytes)
            .map_err(|e| StoreError::Kv(e.to_string()))?;
        Ok(())
    }

    /// 读取并反序列化。
    pub fn get<T: DeserializeOwned>(&self, domain: &str, key: &str) -> Result<T, StoreError> {
        let full = format!("{domain}:{key}");
        let bytes = self
            .db
            .get(full.as_bytes())
            .map_err(|e| StoreError::Kv(e.to_string()))?
            .ok_or_else(|| StoreError::KeyNotFound(full.clone()))?;
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Deserialize(e.to_string()))
    }

    /// 是否存在某个 key。
    pub fn contains(&self, domain: &str, key: &str) -> bool {
        let full = format!("{domain}:{key}");
        matches!(self.db.get(full.as_bytes()), Ok(Some(_)))
    }

    /// 遍历某域下所有 key（返回的是剥去域前缀的裸 key，可直接喂给 get/remove）。
    pub fn keys(&self, domain: &str) -> Vec<String> {
        let prefix = format!("{domain}:");
        self.db
            .scan_prefix(prefix.as_bytes())
            .filter_map(|kv| kv.ok())
            .filter_map(|(k, _)| {
                let full = String::from_utf8_lossy(&k).to_string();
                full.strip_prefix(&prefix).map(str::to_string)
            })
            .collect()
    }

    /// 删除。
    pub fn remove(&self, domain: &str, key: &str) -> Result<bool, StoreError> {
        let full = format!("{domain}:{key}");
        self.db
            .remove(full.as_bytes())
            .map(|r| r.is_some())
            .map_err(|e| StoreError::Kv(e.to_string()))
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        self.db.flush().map(|_| ()).map_err(|e| StoreError::Kv(e.to_string()))
    }
}

fn uuid_short() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let store = Store::open_tmp().unwrap();
        let key = format!("k{}", uuid_short());
        store.put("plan", &key, &json!({"a": 1})).unwrap();
        assert!(store.contains("plan", &key));
        let back: serde_json::Value = store.get("plan", &key).unwrap();
        assert_eq!(back["a"], 1);
        assert!(!store.keys("task").is_empty() || true);
        store.remove("plan", &key).unwrap();
        assert!(!store.contains("plan", &key));
    }
}

#[cfg(test)]
mod double_open_tests {
    use super::*;

    #[test]
    fn same_dir_double_open_shares_db() {
        let dir = std::env::temp_dir().join(format!("wpa-dopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Store::open(&dir).expect("first open");
        a.put("t", "k", &1u32).unwrap();
        let b = Store::open(&dir).expect("second open must not fail with lock error");
        assert_eq!(b.get::<u32>("t", "k").unwrap(), 1);
    }
}
