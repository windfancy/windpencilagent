//! 知识条目与加载：把树包里的知识文件读成可检索的结构。
//!
//! 支持三种格式（同一包可混用）：
//! - `.jsonl`：每行一个 `{id,title,body,tags,source}`；
//! - `.json`：上述对象的数组；
//! - `.md`：按二级标题切分（`## 标题` 起新条目，标题行之下为正文）。
//!
//! 约束单独放 `.toml`（`[[constraint]] id/text/severity`），因为约束要带强度，
//! 混在正文里会让"哪条是红线"变得不可判定。

use std::path::Path;

use serde::{Deserialize, Serialize};
use wpa_api::{Constraint, ConstraintSeverity};

/// 一条知识。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub source: String,
}

impl KnowledgeEntry {
    pub fn new(id: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self { id: id.into(), title: title.into(), body: body.into(), ..Default::default() }
    }

    pub(crate) fn searchable(&self) -> String {
        format!("{} {} {}", self.id, self.title, self.body).to_lowercase()
    }
}

/// 约束文件（`.toml`）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConstraintFile {
    pub constraint: Vec<ConstraintDecl>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConstraintDecl {
    pub id: String,
    pub text: String,
    /// `must` / `should` / `info`（缺省 should）。
    pub severity: Option<String>,
    // ---- 可选：直接给出可执行规则（否则 engine 侧按工具前缀猜，猜不出就降级）----
    /// 适用工具（精确名或 `前缀.*`）。
    pub tools: Vec<String>,
    /// 风险门槛（达到才拦）。
    pub min_risk: Option<String>,
    /// 目标子串（命中才拦）。
    pub target_contains: Vec<String>,
}

impl ConstraintDecl {
    pub fn to_constraint(&self) -> Constraint {
        Constraint {
            id: self.id.clone(),
            text: self.text.clone(),
            severity: match self.severity.as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
                Some("must") => ConstraintSeverity::Must,
                Some("info") => ConstraintSeverity::Info,
                _ => ConstraintSeverity::Should,
            },
            tools: self.tools.clone(),
            min_risk: self.min_risk.clone(),
            target_contains: self.target_contains.clone(),
        }
    }
}

/// 加载结果。
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub entries: Vec<KnowledgeEntry>,
    pub constraints: Vec<ConstraintDecl>,
    pub warnings: Vec<String>,
}

/// 从一组文件加载（路径相对群组目录）。
pub fn load_files(base: &Path, entries: &[String], constraints: Option<&str>) -> Loaded {
    let mut out = Loaded::default();
    for rel in entries {
        let path = base.join(rel);
        match std::fs::read_to_string(&path) {
            Ok(text) => match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "jsonl" | "ndjson" => out.entries.extend(parse_jsonl(&text, &path, &mut out.warnings)),
                "json" => out.entries.extend(parse_json(&text, &path, &mut out.warnings)),
                "md" | "markdown" => out.entries.extend(parse_markdown(&text, &path)),
                other => out.warnings.push(format!(
                    "知识文件 {} 扩展名 {other} 不支持（仅 jsonl/json/md）",
                    path.display()
                )),
            },
            Err(e) => out.warnings.push(format!("读取知识文件失败 {}：{e}", path.display())),
        }
    }
    if let Some(rel) = constraints {
        let path = base.join(rel);
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<ConstraintFile>(&text) {
                Ok(f) => out.constraints = f.constraint,
                Err(e) => out.warnings.push(format!("约束文件解析失败 {}：{e}", path.display())),
            },
            Err(e) => out.warnings.push(format!("读取约束文件失败 {}：{e}", path.display())),
        }
    }
    out
}

fn parse_jsonl(text: &str, path: &Path, warnings: &mut Vec<String>) -> Vec<KnowledgeEntry> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<KnowledgeEntry>(l) {
            Ok(mut e) => {
                if e.id.trim().is_empty() {
                    e.id = format!("{}#{}", path.file_stem().and_then(|s| s.to_str()).unwrap_or("kb"), i + 1);
                }
                out.push(e);
            }
            Err(e) => warnings.push(format!("{} 第 {} 行解析失败：{e}", path.display(), i + 1)),
        }
    }
    out
}

fn parse_json(text: &str, path: &Path, warnings: &mut Vec<String>) -> Vec<KnowledgeEntry> {
    match serde_json::from_str::<Vec<KnowledgeEntry>>(text) {
        Ok(v) => v,
        Err(e) => {
            warnings.push(format!("{} 解析失败：{e}", path.display()));
            Vec::new()
        }
    }
}

/// Markdown：`## 标题` 起新条目；`###` 及以下算正文。
fn parse_markdown(text: &str, path: &Path) -> Vec<KnowledgeEntry> {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("kb");
    let mut out: Vec<KnowledgeEntry> = Vec::new();
    let mut cur: Option<KnowledgeEntry> = None;
    let mut seq = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("## ") {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            seq += 1;
            // id 取标题的 ASCII 安全片段不够稳，直接 stem-序号；标题保可读。
            cur = Some(KnowledgeEntry {
                id: format!("{stem}-{seq}"),
                title: title.trim().to_string(),
                source: format!("{}#{}", path.display(), title.trim()),
                ..Default::default()
            });
        } else if let Some(e) = cur.as_mut() {
            e.body.push_str(line);
            e.body.push('\n');
        }
    }
    if let Some(e) = cur.take() {
        out.push(e);
    }
    out
}

/// 从经验沉淀新条目（学习接口的落盘形态）。
pub fn experience_to_entry(exp: &wpa_api::Experience, seq: usize) -> KnowledgeEntry {
    let mut body = String::new();
    if !exp.findings.is_empty() {
        body.push_str("发现：\n");
        for f in &exp.findings {
            body.push_str(&format!("- {f}\n"));
        }
    }
    if !exp.techniques.is_empty() {
        body.push_str("有效手法：\n");
        for t in &exp.techniques {
            body.push_str(&format!("- {t}\n"));
        }
    }
    if !exp.dead_ends.is_empty() {
        body.push_str("死路（不要重复）：\n");
        for d in &exp.dead_ends {
            body.push_str(&format!("- {d}\n"));
        }
    }
    KnowledgeEntry {
        id: format!("exp-{seq:05}"),
        title: if exp.objective.trim().is_empty() {
            "经验沉淀".into()
        } else {
            exp.objective.clone()
        },
        body,
        tags: exp.tags.clone(),
        source: "experience".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kb-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_jsonl_md_and_constraints() {
        let d = tmpdir();
        let mut f = std::fs::File::create(d.join("vuln.jsonl")).unwrap();
        writeln!(f, r#"{{"id":"CVE-2021-44228","title":"Log4Shell","body":"JNDI 注入","tags":["java","rce"]}}"#).unwrap();
        writeln!(f, r#"{{"title":"无 id 条目","body":"x"}}"#).unwrap();
        std::fs::write(d.join("guide.md"), "# 手册\n\n## SMB 空会话\n枚举共享\n\n## DNS 区域传送\nAXFR\n").unwrap();
        std::fs::write(
            d.join("rules.toml"),
            r#"
[[constraint]]
id = "roe-1"
text = "禁止攻击授权范围外目标"
severity = "must"
tools = ["exploit.*"]

[[constraint]]
id = "roe-2"
text = "先侦察再验证"
"#,
        )
        .unwrap();

        let out = load_files(
            &d,
            &["vuln.jsonl".into(), "guide.md".into()],
            Some("rules.toml"),
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert_eq!(out.entries.len(), 4);
        assert_eq!(out.entries[0].id, "CVE-2021-44228");
        assert!(out.entries[1].id.starts_with("vuln#"), "缺 id 自动补");
        assert_eq!(out.entries[2].title, "SMB 空会话");
        assert!(out.entries[2].body.contains("枚举共享"));
        assert_eq!(out.constraints.len(), 2);
        assert_eq!(out.constraints[0].to_constraint().severity, ConstraintSeverity::Must);
        assert_eq!(out.constraints[1].to_constraint().severity, ConstraintSeverity::Should);
        assert_eq!(out.constraints[0].tools, vec!["exploit.*"]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_file_is_warning_not_panic() {
        let out = load_files(Path::new("."), &["nope.jsonl".into()], Some("nope.toml"));
        assert_eq!(out.entries.len(), 0);
        assert_eq!(out.warnings.len(), 2);
    }

    #[test]
    fn experience_becomes_entry() {
        let exp = wpa_api::Experience {
            objective: "打穿 SMB".into(),
            findings: vec!["445 开放".into()],
            techniques: vec!["空会话枚举".into()],
            dead_ends: vec!["MS17-010 已打补丁".into()],
            tags: vec!["smb".into()],
        };
        let e = experience_to_entry(&exp, 7);
        assert_eq!(e.id, "exp-00007");
        assert!(e.body.contains("死路"));
        assert!(e.body.contains("空会话枚举"));
    }
}
