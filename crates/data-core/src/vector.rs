//! 本地向量库：一个 JSON 文件就是一座库，余弦相似度检索。
//!
//! 为什么用 JSON 文件而不是外部向量数据库：基座不引入常驻服务、不依赖第三方进程
//! （这条是硬约束）。几万条量级下，全量扫描 + 余弦足够快，文件还能直接看、直接备份。
//!
//! **向量只能来自模型 embedding**。没有可用模型时工具直接报错——用哈希"伪造"向量
//! 会让检索看起来能用、结果却是字面巧合，比报错更糟。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// embedding 提供者（由装配层用当前模型档案实现）。
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
    /// 记录进库里的模型标识（换模型后向量空间不通用，必须能看出来）。
    fn model(&self) -> String;
}

/// 一条向量记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VecItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
    pub vector: Vec<f32>,
    #[serde(default)]
    pub meta: Value,
}

/// 一座向量库。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VecStore {
    pub version: u32,
    /// 向量维度；0 = 还没写入过（首条写入时定死）
    #[serde(default)]
    pub dims: usize,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub items: Vec<VecItem>,
}

impl Default for VecStore {
    fn default() -> Self {
        Self { version: 1, dims: 0, model: String::new(), items: Vec::new() }
    }
}

/// 把 `store` 参数解析成实际文件路径（缺扩展名时补 `.vec.json`）。
pub fn store_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut s = rel.trim().to_string();
    if s.is_empty() {
        return Err("store 不能为空".into());
    }
    if !s.ends_with(".json") {
        s.push_str(".vec.json");
    }
    crate::resolve_under(root, &s)
}

/// 读库；文件不存在时报错（查询/更新不该悄悄建一座空库）。
pub fn load(path: &Path) -> Result<VecStore, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取向量库 {} 失败：{e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("向量库 {} 解析失败：{e}", path.display()))
}

/// 新建空库（已存在则报错，避免覆盖）。
pub fn create(path: &Path, dims: usize) -> Result<VecStore, String> {
    if path.exists() {
        return Err(format!("向量库已存在：{}", path.display()));
    }
    let store = VecStore { dims, ..Default::default() };
    save(path, &store)?;
    Ok(store)
}

/// 原子写回。
pub fn save(path: &Path, store: &VecStore) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| format!("序列化向量库失败：{e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入临时文件失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换向量库失败：{e}")
    })
}

/// 余弦相似度（任一向量为零向量时返回 0）。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// 检索 top-k（返回分数与下标）。
pub fn search(store: &VecStore, query: &[f32], top_k: usize) -> Vec<(f32, usize)> {
    let mut scored: Vec<(f32, usize)> = store
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| (cosine(query, &it.vector), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k.max(1));
    scored
}

/// 维度检查：库里已有维度时，新向量必须一致。
pub fn check_dims(store: &VecStore, vec_len: usize) -> Result<(), String> {
    if store.dims == 0 || store.dims == vec_len {
        Ok(())
    } else {
        Err(format!(
            "向量维度不一致：库里是 {} 维，本次是 {} 维（换了 embedding 模型就只能重建库）",
            store.dims, vec_len
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kbvec_{tag}_{}.json", std::process::id()))
    }

    #[test]
    fn cosine_and_search_rank() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &b).abs() < 1e-6);
        let store = VecStore {
            version: 1,
            dims: 2,
            model: "m".into(),
            items: vec![
                VecItem { id: "a".into(), text: "a".into(), vector: vec![1.0, 0.0], meta: Value::Null },
                VecItem { id: "b".into(), text: "b".into(), vector: vec![0.0, 1.0], meta: Value::Null },
            ],
        };
        let hits = search(&store, &[0.9, 0.1], 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(store.items[hits[0].1].id, "a");
    }

    #[test]
    fn create_load_roundtrip_and_no_overwrite() {
        let p = tmp("roundtrip");
        let _ = std::fs::remove_file(&p);
        create(&p, 0).unwrap();
        assert!(create(&p, 0).is_err(), "已存在不应覆盖");
        let mut s = load(&p).unwrap();
        s.dims = 3;
        s.items.push(VecItem { id: "x".into(), text: "x".into(), vector: vec![0.1, 0.2, 0.3], meta: Value::Null });
        save(&p, &s).unwrap();
        assert_eq!(load(&p).unwrap().items.len(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn dims_mismatch_is_reported() {
        let s = VecStore { dims: 4, ..Default::default() };
        assert!(check_dims(&s, 4).is_ok());
        assert!(check_dims(&s, 8).is_err());
    }
}
