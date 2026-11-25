//! 经验库：跨会话沉淀的可复用知识（持久化，sled domain=`exp`）。
//!
//! 数据来源：对话引擎在上下文压缩时由 LLM 抽取的「目标 / 关键发现 / 技术 /
//! 死路 / 待办」字段化条目；消费方向：新会话 system prompt 按关键词加权召回注入
//! （R1-4：kind 分类 + 文本去重合并 + dead_ends 显性标注 + token 注入预算）。

use serde::{Deserialize, Serialize};

use crate::store::{Store, StoreError};

/// 经验类别：0=摘要/通用 1=工具用法 2=目标特征 3=死路（dead_end，失败方向勿重复尝试）。
pub const KIND_SUMMARY: u8 = 0;
pub const KIND_TECHNIQUE: u8 = 1;
pub const KIND_TARGET: u8 = 2;
pub const KIND_DEAD_END: u8 = 3;

/// 一条经验。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceEntry {
    /// 毫秒时间戳（写入顺序兜底）
    pub time_ms: u64,
    /// 经验正文（一两句话）
    pub text: String,
    /// 标签（工具名 / 目标特征等，用于召回）
    pub tags: Vec<String>,
    /// 类别（R1-4，旧记录缺省=0 摘要）
    #[serde(default)]
    pub kind: u8,
}

/// 经验库句柄（sled 线程安全，可 Arc 共享到工作线程）。
pub struct ExperienceStore {
    db: Store,
}

impl ExperienceStore {
    /// 打开（目录不存在自动创建）。
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        Ok(Self { db: Store::open(dir)? })
    }

    /// 测试用临时库。
    pub fn open_tmp() -> Result<Self, StoreError> {
        Ok(Self { db: Store::open_tmp()? })
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 追加一条经验（自动盖时间戳 + 文本去重：重复时合并标签并刷新时间）。
    pub fn add(&self, text: &str, tags: Vec<String>) -> Result<(), StoreError> {
        self.add_typed(text, tags, KIND_SUMMARY)
    }

    /// 追加一条指定类别的经验；同一正文重复入库时合并标签（去重并集）并刷新时间戳。
    pub fn add_typed(
        &self,
        text: &str,
        tags: Vec<String>,
        kind: u8,
    ) -> Result<(), StoreError> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        let time_ms = Self::now_ms();
        // 去重：同正文（trim 后全等）视为同一条经验，合并 tags
        for k in self.db.keys("exp") {
            let Ok(mut e) = self.db.get::<ExperienceEntry>("exp", &k) else { continue };
            if e.text == text {
                let mut merged = e.tags.clone();
                for t in &tags {
                    if !merged.contains(t) {
                        merged.push(t.clone());
                    }
                }
                e.tags = merged;
                e.time_ms = time_ms;
                e.kind = kind;
                return self.db.put("exp", &k, &e);
            }
        }
        let entry = ExperienceEntry { time_ms, text, tags, kind };
        let key = format!("{time_ms}-{}", entry.text.chars().take(16).collect::<String>());
        self.db.put("exp", &key, &entry)
    }

    /// 追加一条死路经验（R1-4：失败方向显性沉淀，防重复踩坑）。
    pub fn add_dead_end(&self, text: &str, tags: Vec<String>) -> Result<(), StoreError> {
        self.add_typed(text, tags, KIND_DEAD_END)
    }

    /// 全量经验（按写入时间升序）。
    pub fn all(&self) -> Vec<ExperienceEntry> {
        let mut list: Vec<ExperienceEntry> = self
            .db
            .keys("exp")
            .into_iter()
            .filter_map(|k| self.db.get::<ExperienceEntry>("exp", &k).ok())
            .collect();
        list.sort_by_key(|e| e.time_ms);
        list
    }

    /// 条数。
    pub fn count(&self) -> usize {
        self.db.keys("exp").len()
    }

    /// 关键词加权召回（R1-4）：命中标签 ×2、命中正文 ×1 累计得分；
    /// 死路（kind=3）仅在显式命中时返回，绝不进兜底（无关查询不注入失败史）；
    /// 无任何命中回退为最近 `limit` 条非死路经验。
    pub fn recall(&self, query: &str, limit: usize) -> Vec<ExperienceEntry> {
        let all = self.all();
        let query_lower = query.to_lowercase();
        let terms: Vec<&str> =
            query_lower.split_whitespace().filter(|t| t.len() >= 2).collect();
        let mut scored: Vec<(usize, &ExperienceEntry)> = all
            .iter()
            .map(|e| {
                let hay = e.text.to_lowercase();
                let tag_hay = e.tags.join(" ").to_lowercase();
                let mut score = terms.iter().filter(|t| hay.contains(**t)).count();
                score += terms.iter().filter(|t| tag_hay.contains(**t)).count() * 2;
                // 死路仅在显式命中时参与（score>0 才保留）
                if e.kind == KIND_DEAD_END && score == 0 {
                    score = usize::MAX; // 占位，稍后过滤
                }
                (score, e)
            })
            .filter(|(s, e)| !(e.kind == KIND_DEAD_END && *s == usize::MAX))
            .collect();
        if scored.iter().any(|(s, _)| *s > 0) {
            scored.sort_by_key(|(s, e)| std::cmp::Reverse((*s, e.time_ms)));
            scored.into_iter().take(limit).map(|(_, e)| e.clone()).collect()
        } else {
            // 兜底：最近条目（跳过死路）
            all.iter()
                .rev()
                .filter(|e| e.kind != KIND_DEAD_END)
                .take(limit)
                .cloned()
                .collect()
        }
    }

    /// 清空经验库。
    pub fn clear(&self) -> Result<(), StoreError> {
        for k in self.db.keys("exp") {
            self.db.remove("exp", &k)?;
        }
        Ok(())
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        self.db.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_recall_clear() {
        let store = ExperienceStore::open_tmp().unwrap();
        store.add("nmap -sV 可同时做版本探测", vec!["nmap".into(), "扫描".into()]).unwrap();
        store.add("Grafana 默认端口 3000", vec!["grafana".into()]).unwrap();
        store.add("弱口令字典优先试 admin/admin123", vec!["弱口令".into()]).unwrap();
        assert_eq!(store.count(), 3);

        let hit = store.recall("用 nmap 扫描版本", 3);
        assert_eq!(hit[0].text, "nmap -sV 可同时做版本探测");

        // 无命中 → 最近条目兜底
        let fallback = store.recall("完全无关的查询词", 2);
        assert_eq!(fallback.len(), 2);
        assert_eq!(fallback[0].text, "弱口令字典优先试 admin/admin123");

        store.clear().unwrap();
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn dedup_merges_tags() {
        let store = ExperienceStore::open_tmp().unwrap();
        store.add("Tomcat 默认端口 8080", vec!["tomcat".into()]).unwrap();
        store.add_typed("Tomcat 默认端口 8080", vec!["java".into(), "tomcat".into()], KIND_TARGET)
            .unwrap();
        assert_eq!(store.count(), 1, "同正文去重为一条");
        let all = store.all();
        assert_eq!(all[0].kind, KIND_TARGET, "kind 以最新写入为准");
        assert!(all[0].tags.contains(&"java".into()));
        assert!(all[0].tags.contains(&"tomcat".into()));
        assert_eq!(all[0].tags.len(), 2, "标签合并去重");
    }

    #[test]
    fn dead_end_only_on_explicit_hit() {
        let store = ExperienceStore::open_tmp().unwrap();
        store.add("常规经验 A", vec!["a".into()]).unwrap();
        store.add("常规经验 B", vec!["b".into()]).unwrap();
        store.add_dead_end("对 10.0.0.5 的 445 端口 ms17-010 利用失败：未打补丁前被 AV 拦截", vec!["ms17-010".into()])
            .unwrap();
        // 显式命中 → 死路返回且加权（标签命中×2，压过正文命中）
        let hit = store.recall("ms17-010 利用", 3);
        assert_eq!(hit[0].kind, KIND_DEAD_END);
        assert!(hit[0].text.contains("ms17-010"));
        // 无关查询 → 死路不出现在兜底
        let fb = store.recall("完全无关查询", 5);
        assert!(fb.iter().all(|e| e.kind != KIND_DEAD_END), "死路不进兜底");
        assert_eq!(fb.len(), 2);
    }

    #[test]
    fn legacy_json_without_kind_deserializes() {
        let old = r#"{"time_ms":1,"text":"旧经验","tags":["t1"]}"#;
        let e: ExperienceEntry = serde_json::from_str(old).unwrap();
        assert_eq!(e.kind, KIND_SUMMARY);
        assert_eq!(e.text, "旧经验");
    }
}
