//! 文件式知识提供者：分级检索 + 约束输出 + 学习回写。
//!
//! 分级检索（由强到弱）：精确 id → 标题/标签 → 正文关键词 → 模糊包含。
//! 这样"查 CVE-2021-44228"能精确命中，而"smb 空会话"这类自然语言也能召回。
//!
//! 学习回写落到群组目录的 `learn.jsonl`：**不动用户手写的知识文件**，
//! 避免程序在别人的资料里掺私货（可审计、可随时删）。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use wpa_api::{
    Constraint, ConstraintQuery, Experience, KnowledgeHit, KnowledgeProvider,
};

use crate::loader::{experience_to_entry, load_files, ConstraintDecl, KnowledgeEntry};

/// 文件式知识库。
#[derive(Debug, Clone)]
pub struct FileKnowledgeProvider {
    name: String,
    base: PathBuf,
    entries: Vec<KnowledgeEntry>,
    constraints: Vec<ConstraintDecl>,
    /// 加载期告警（装配时汇总给 UI）。
    warnings: Vec<String>,
}

impl FileKnowledgeProvider {
    /// 从群组目录构建。
    pub fn from_dir(
        name: impl Into<String>,
        base: impl AsRef<Path>,
        entries: &[String],
        constraints: Option<&str>,
    ) -> Self {
        let base = base.as_ref().to_path_buf();
        let mut loaded = load_files(&base, entries, constraints);
        // 学习沉淀（上一轮写回的）自动并入检索面。
        let learn = base.join("learn.jsonl");
        if learn.is_file() {
            let extra = load_files(&base, &["learn.jsonl".to_string()], None);
            loaded.entries.extend(extra.entries);
            loaded.warnings.extend(extra.warnings);
        }
        Self {
            name: name.into(),
            base,
            entries: loaded.entries,
            constraints: loaded.constraints,
            warnings: loaded.warnings,
        }
    }

    /// 直接喂条目（测试/内嵌知识用）。
    pub fn with_entries(
        name: impl Into<String>,
        base: impl AsRef<Path>,
        entries: Vec<KnowledgeEntry>,
    ) -> Self {
        Self {
            name: name.into(),
            base: base.as_ref().to_path_buf(),
            entries,
            constraints: vec![],
            warnings: vec![],
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn constraints_decl(&self) -> &[ConstraintDecl] {
        &self.constraints
    }

    /// 分级检索（纯函数，便于单测）。
    pub fn rank(&self, query: &str, limit: usize) -> Vec<(f32, &KnowledgeEntry)> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let terms: Vec<&str> = q.split_whitespace().filter(|t| !t.is_empty()).collect();
        let mut scored: Vec<(f32, &KnowledgeEntry)> = Vec::new();

        for e in &self.entries {
            let id = e.id.to_lowercase();
            let title = e.title.to_lowercase();
            let tags: Vec<String> = e.tags.iter().map(|t| t.to_lowercase()).collect();
            let all = e.searchable();

            let mut score = 0f32;
            // 级 1：精确 / 包含 id
            if id == q {
                score += 1000.0;
            } else if !id.is_empty() && id.contains(&q) {
                score += 200.0;
            }
            // 级 2：标题与标签
            if title.contains(&q) {
                score += 100.0;
            }
            if tags.iter().any(|t| t.contains(&q)) {
                score += 60.0;
            }
            // 级 3：正文/标题词命中
            let mut term_hits = 0usize;
            for t in &terms {
                if title.contains(t) {
                    score += 40.0;
                    term_hits += 1;
                } else if tags.iter().any(|x| x.contains(t)) {
                    score += 25.0;
                    term_hits += 1;
                } else if all.contains(t) {
                    score += 10.0;
                    term_hits += 1;
                }
            }
            // 级 4：整串模糊包含（中文长句常整串命中正文）
            if score == 0.0 && all.contains(&q) {
                score += 5.0;
            }
            // 多词全命中加成
            if !terms.is_empty() && term_hits == terms.len() {
                score += 20.0;
            }
            if score > 0.0 {
                scored.push((score, e));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit.max(1));
        scored
    }
}

#[async_trait]
impl KnowledgeProvider for FileKnowledgeProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, query: &str, limit: usize) -> Vec<KnowledgeHit> {
        self.rank(query, limit)
            .into_iter()
            .map(|(score, e)| KnowledgeHit {
                id: e.id.clone(),
                title: e.title.clone(),
                body: e.body.clone(),
                score,
                source: if e.source.is_empty() { self.name.clone() } else { e.source.clone() },
            })
            .collect()
    }

    async fn constraints(&self, q: &ConstraintQuery) -> Vec<Constraint> {
        self.constraints
            .iter()
            .filter(|c| {
                // 工具面过滤：声明了 tools 的约束只在相关工具在场时给出。
                if c.tools.is_empty() || q.tools.is_empty() {
                    return true;
                }
                c.tools.iter().any(|p| {
                    let prefix = p.strip_suffix(".*").unwrap_or(p);
                    q.tools.iter().any(|t| t == p || t.starts_with(prefix))
                })
            })
            .map(|c| c.to_constraint())
            .collect()
    }

    async fn learn(&self, exp: &Experience) -> Result<(), String> {
        use std::io::Write;
        let path = self.base.join("learn.jsonl");
        let seq = self.entries.iter().filter(|e| e.id.starts_with("exp-")).count() + 1;
        let entry = experience_to_entry(exp, seq);
        let line = serde_json::to_string(&entry).map_err(|e| format!("序列化经验失败：{e}"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("打开 {} 失败：{e}", path.display()))?;
        writeln!(f, "{line}").map_err(|e| format!("写入经验失败：{e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> FileKnowledgeProvider {
        FileKnowledgeProvider::with_entries(
            "vulndb",
            ".",
            vec![
                {
                    let mut e = KnowledgeEntry::new("CVE-2021-44228", "Log4Shell", "Log4j2 JNDI 注入，可 RCE");
                    e.tags = vec!["java".into(), "rce".into()];
                    e
                },
                {
                    let mut e = KnowledgeEntry::new("CVE-2017-0144", "MS17-010 永恒之蓝", "SMBv1 远程溢出");
                    e.tags = vec!["smb".into(), "windows".into()];
                    e
                },
                KnowledgeEntry::new("technique-001", "SMB 空会话枚举", "使用空会话列共享与用户"),
            ],
        )
    }

    #[tokio::test]
    async fn exact_id_ranks_first() {
        let hits = kb().search("CVE-2021-44228", 5).await;
        assert_eq!(hits[0].id, "CVE-2021-44228");
        assert!(hits[0].score >= 1000.0);
    }

    #[tokio::test]
    async fn tag_and_title_retrieval() {
        let hits = kb().search("smb", 5).await;
        assert!(hits.len() >= 2, "SMB 应召回 MS17-010 与空会话条目：{hits:?}");
        assert!(hits.iter().any(|h| h.title.contains("空会话")));
    }

    #[tokio::test]
    async fn chinese_query_hits_body() {
        let hits = kb().search("空会话 共享", 5).await;
        assert!(!hits.is_empty());
        assert_eq!(hits[0].id, "technique-001");
    }

    #[tokio::test]
    async fn empty_query_returns_nothing() {
        assert!(kb().search("   ", 5).await.is_empty());
    }

    #[tokio::test]
    async fn learn_appends_and_is_retrievable() {
        let d = std::env::temp_dir().join(format!("kb-learn-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = FileKnowledgeProvider::from_dir("kb", &d, &[], None);
        p.learn(&Experience {
            objective: "SMB 横向".into(),
            findings: vec!["445 开放".into()],
            ..Default::default()
        })
        .await
        .unwrap();
        // 重新加载后应能检索到（学习闭环）
        let p2 = FileKnowledgeProvider::from_dir("kb", &d, &[], None);
        let hits = p2.search("SMB 横向", 3).await;
        assert!(!hits.is_empty(), "学习内容应重新可检索");
        std::fs::remove_dir_all(&d).ok();
    }

    #[tokio::test]
    async fn constraints_filtered_by_tools() {
        let mut kb = kb();
        kb.constraints = vec![
            ConstraintDecl {
                id: "roe-1".into(),
                text: "仅限授权范围".into(),
                severity: Some("must".into()),
                tools: vec!["exploit.*".into()],
                ..Default::default()
            },
            ConstraintDecl {
                id: "roe-2".into(),
                text: "全程留痕".into(),
                ..Default::default()
            },
        ];
        let all = kb
            .constraints(&ConstraintQuery {
                task: "t".into(),
                stage: None,
                tools: vec!["scan.port".into()],
            })
            .await;
        assert_eq!(all.len(), 1, "exploit 约束不该出现在纯扫描面");
        let with_exploit = kb
            .constraints(&ConstraintQuery {
                task: "t".into(),
                stage: None,
                tools: vec!["exploit.rce".into()],
            })
            .await;
        assert_eq!(with_exploit.len(), 2);
    }
}
