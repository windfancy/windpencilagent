//! 领域（描述包）加载器：`domains/<id>/` 下四个文件的解析与合并。
//!
//! 领域**不是实体**，不拥有实现、不复制文件、不安装插件。它只是四段描述：
//! - `domain.toml`：角色 + 引用哪些组 + 指向另外三个文件（结构化，必须可解析）
//! - `workflow.md`：工作范式，Markdown + front matter，注入 system prompt
//! - `knowledge.md`：常识条目，按 `## 标题` 切分，进知识检索面
//! - `constraints.toml`：红线，带强度与执行条件，编译成 guard
//!
//! 格式刻意不同：意图（自然语言）与判定（结构化）分开写，才不会出现
//! "这条到底是建议还是红线"这种无法判定的问题。
//!
//! 失败策略：**领域装载失败不阻断启动**，降级为「无领域」并把原因写进 warnings。
//! 理由：用户可能只是把 `domain.active` 写错了一个字母，不该让整个程序打不开。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;

/// `domain.toml` 的结构。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DomainFile {
    pub domain: DomainMeta,
    pub params: DomainParams,
    pub tool_filter: Option<ToolFilter>,
}

/// 领域元信息。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DomainMeta {
    pub id: String,
    pub name: String,
    pub version: String,
    /// `general` = 通用领域（软流程）/ `pack` = 专业领域包（可带阶段机）
    pub kind: String,
    pub description: String,
    pub summary: String,
    /// 角色设定（唯一允许出现"你是谁"的地方）。
    pub persona: String,
    /// 引用的组（顺序即工具在 prompt 里的呈现顺序）。
    pub groups: Vec<String>,
    pub workflow: Option<String>,
    pub knowledge: Option<String>,
    pub constraints: Option<String>,
    pub toolkit: Option<String>,
}

/// 领域级参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DomainParams {
    /// 是否允许 High/Critical 风险工具进工具面（仍逐次审批）。
    pub allow_high_risk: bool,
    /// 本领域单轮工具调用上限；0 = 用全局配置。
    pub max_tool_rounds: usize,
}

impl Default for DomainParams {
    fn default() -> Self {
        Self { allow_high_risk: true, max_tool_rounds: 0 }
    }
}

/// 工具面瘦身（只减不增）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolFilter {
    pub drop: Vec<String>,
}

/// workflow.md 的 front matter 与正文。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkflowSection {
    /// 注入策略：`always` / `on_keyword` / `manual`
    pub inject: String,
    /// 注入字符预算（超出按 priority 顺序保留）
    pub budget_chars: usize,
    pub priority: Vec<u32>,
    /// 正文（front matter 已剥离）
    pub text: String,
}

impl WorkflowSection {
    /// 按预算裁剪正文。超预算时保留前 `budget_chars` 个字符并附一句说明，
    /// 让模型知道还有更多内容可主动检索（而不是以为文件就这么短）。
    pub fn fitted(&self, budget_chars: usize) -> String {
        let cap = if budget_chars == 0 { self.budget_chars } else { budget_chars };
        if cap == 0 || self.text.chars().count() <= cap {
            return self.text.clone();
        }
        let head: String = self.text.chars().take(cap).collect();
        format!("{head}\n\n（工作范式较长，以上为前半部分；需要更细的步骤时可调用 `kb.search` 检索本领域常识。）")
    }
}

/// 装载后的领域包。
#[derive(Debug, Clone)]
pub struct DomainPack {
    pub manifest: DomainMeta,
    pub dir: PathBuf,
    pub params: DomainParams,
    pub tool_drop: Vec<String>,
    pub workflow: Option<WorkflowSection>,
    /// 常识条目数（实际条目在知识提供者里，这里只留计数供 UI 展示）
    pub knowledge_entries: usize,
    pub warnings: Vec<String>,
}

impl DomainPack {
    pub fn persona(&self) -> Option<&str> {
        let p = self.manifest.persona.trim();
        if p.is_empty() { None } else { Some(p) }
    }

    /// 注入用的一段文本（带标题，便于模型区分「这是工作范式，不是用户指令」）。
    pub fn workflow_section(&self) -> Option<String> {
        let w = self.workflow.as_ref()?;
        if w.text.trim().is_empty() {
            return None;
        }
        let body = w.fitted(0);
        Some(format!("## 工作范式（{}）\n{body}", self.manifest.name))
    }
}

/// 领域列表项（供 UI 领域页）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DomainRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub summary: String,
    /// `groups` 声明里**真正命中基座组**的那些（未命中的在 UI 上标灰，
    /// 所以这里给名字而不是只给个数）。
    pub group_ids: Vec<String>,
    pub groups: usize,
    pub knowledge_entries: usize,
    pub has_workflow: bool,
    pub active: bool,
    pub error: Option<String>,
}

/// 解析 `---` 包裹的 front matter（仅支持 `key: value` 与 `key: [a, b]`）。
///
/// 刻意不做完整 YAML：这里的字段只有 3 个，引一门 YAML 解析器不值得
/// （多一个依赖、多一类"缩进写错"的失败模式）。
pub fn split_front_matter(raw: &str) -> (Vec<(String, String)>, String) {
    let mut meta: Vec<(String, String)> = Vec::new();
    let text = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = text.lines();
    let first = lines.next().map(str::trim).unwrap_or("");
    if first != "---" {
        return (meta, text.to_string());
    }
    let mut body = String::new();
    let mut closed = false;
    for line in lines {
        if !closed {
            if line.trim() == "---" {
                closed = true;
                continue;
            }
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = t.split_once(':') {
                meta.push((k.trim().to_string(), v.trim().to_string()));
            }
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    if !closed {
        // 没有闭合的 `---`：整份当正文，front matter 视作没有（不静默丢弃内容）
        return (Vec::new(), text.to_string());
    }
    (meta, body)
}

fn meta_get(meta: &[(String, String)], key: &str) -> Option<String> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn parse_bracket_list(v: &str) -> Vec<u32> {
    let t = v.trim().trim_start_matches('[').trim_end_matches(']');
    t.split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect()
}

/// 解析 workflow.md。
pub fn parse_workflow(raw: &str) -> WorkflowSection {
    let (meta, body) = split_front_matter(raw);
    WorkflowSection {
        inject: meta_get(&meta, "inject").unwrap_or_else(|| "always".into()),
        budget_chars: meta_get(&meta, "budget_chars")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
        priority: meta_get(&meta, "priority")
            .map(|v| parse_bracket_list(&v))
            .unwrap_or_default(),
        text: body.trim_start_matches('\n').to_string(),
    }
}

/// 装载一个领域目录。
pub fn load(dir: &Path) -> Result<DomainPack, String> {
    let manifest_path = dir.join("domain.toml");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("读取 {} 失败：{e}", manifest_path.display()))?;
    let file: DomainFile =
        toml::from_str(&raw).map_err(|e| format!("解析 {} 失败：{e}", manifest_path.display()))?;
    let mut warnings = Vec::new();
    if file.domain.id.trim().is_empty() {
        return Err(format!("{} 缺少 [domain].id", manifest_path.display()));
    }
    if file.domain.persona.trim().is_empty() {
        warnings.push(format!("领域 {} 没有 persona，将使用基座默认角色", file.domain.id));
    }

    let workflow = match &file.domain.workflow {
        Some(rel) if !rel.trim().is_empty() => {
            let p = dir.join(rel);
            match std::fs::read_to_string(&p) {
                Ok(text) => Some(parse_workflow(&text)),
                Err(e) => {
                    warnings.push(format!("读取工作范式失败 {}：{e}", p.display()));
                    None
                }
            }
        }
        _ => None,
    };

    // 知识条目与约束交给 knowledge-core 统一加载（同一套格式，避免两处解析漂移）
    let entries: Vec<String> = file
        .domain
        .knowledge
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .into_iter()
        .collect();
    let knowledge_entries = if entries.is_empty() {
        0
    } else {
        let loaded = knowledge_core::load_files(dir, &entries, None);
        warnings.extend(loaded.warnings.iter().cloned());
        loaded.entries.len()
    };

    Ok(DomainPack {
        manifest: file.domain,
        dir: dir.to_path_buf(),
        params: file.params,
        tool_drop: file.tool_filter.map(|t| t.drop).unwrap_or_default(),
        workflow,
        knowledge_entries,
        warnings,
    })
}

/// 列出领域根目录下的全部领域（解析失败的也列出来，带 error 供 UI 显示）。
pub fn list(root: &Path, active: &str) -> Vec<DomainRow> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || !p.join("domain.toml").is_file() {
            continue;
        }
        let id = e.file_name().to_string_lossy().to_string();
        match load(&p) {
            Ok(pack) => out.push(DomainRow {
                id: id.clone(),
                name: pack.manifest.name.clone(),
                kind: pack.manifest.kind.clone(),
                summary: pack.manifest.summary.clone(),
                groups: pack.manifest.groups.len(),
                group_ids: pack.manifest.groups.clone(),
                knowledge_entries: pack.knowledge_entries,
                has_workflow: pack.workflow.is_some(),
                active: id == active,
                error: None,
            }),
            Err(err) => out.push(DomainRow {
                id: id.clone(),
                active: id == active,
                error: Some(err),
                ..Default::default()
            }),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ---------------------------------------------------------------------------
// 默认领域模板（随二进制分发）
// ---------------------------------------------------------------------------

/// 默认领域模板文件（编译期嵌入）。
///
/// 为什么嵌进二进制而不是"让用户去仓库里拷"：分发包只有一个 exe，
/// 用户目录下没有 `domains/`。首次运行时把模板落盘，用户才能看到"领域长什么样"、
/// 才有东西可改 —— 空目录等于把模板藏起来了。
///
/// `toolkit.toml` 不在模板里：它是**生成物**（`registry export` 产出），
/// 嵌一份进来只会立刻过时。
pub const DEFAULT_TEMPLATE: &[(&str, &str)] = &[
    ("domain.toml", include_str!("../../../domains/default/domain.toml")),
    ("workflow.md", include_str!("../../../domains/default/workflow.md")),
    ("knowledge.md", include_str!("../../../domains/default/knowledge.md")),
    ("constraints.toml", include_str!("../../../domains/default/constraints.toml")),
];

/// 补齐默认领域模板：只写缺失的文件，**绝不覆盖**用户改动。
///
/// 返回实际写出的文件名（空 = 已齐全，什么都没做）。
pub fn ensure_default(root: &Path) -> Result<Vec<String>, String> {
    let dir = root.join("default");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建领域目录 {} 失败：{e}", dir.display()))?;
    let mut written = Vec::new();
    for (name, body) in DEFAULT_TEMPLATE {
        let p = dir.join(name);
        if p.is_file() {
            continue;
        }
        std::fs::write(&p, body).map_err(|e| format!("写入 {} 失败：{e}", p.display()))?;
        written.push((*name).to_string());
    }
    Ok(written)
}

/// 按配置装载激活领域。
///
/// 返回 `(领域包, 告警)`。目录不存在时返回 `(None, [])`（纯基座模式，合法）；
/// 目录存在但解析失败时返回 `(None, [原因])` —— 降级但不阻断。
pub fn load_active(cfg: &AppConfig) -> (Option<DomainPack>, Vec<String>) {    let dir = cfg.active_domain_dir();
    if !dir.join("domain.toml").is_file() {
        // 显式指定了非默认领域却不存在，才算异常；默认领域缺失是合法的
        if cfg.domain.active == "default" {
            return (None, Vec::new());
        }
        return (
            None,
            vec![format!(
                "领域 {} 不存在（{}），已降级为纯基座模式",
                cfg.domain.active,
                dir.display()
            )],
        );
    }
    match load(&dir) {
        Ok(pack) => {
            let mut warns: Vec<String> = pack.warnings.clone();
            if !pack.manifest.id.is_empty() && pack.manifest.id != cfg.domain.active {
                warns.push(format!(
                    "领域目录名 {} 与 domain.toml 的 id {} 不一致（以目录名为配置键）",
                    cfg.domain.active, pack.manifest.id
                ));
            }
            (Some(pack), warns)
        }
        Err(e) => (
            None,
            vec![format!("领域装载失败，已降级为纯基座模式：{e}")],
        ),
    }
}

/// 精确改写 `domain.toml` 的 `groups` 数组，**保留原有注释**（组↔领域绑定）。
///
/// 为什么不整份重写：`domain.toml` 是给人读的文件，头部有大段说明、每个组后面
/// 有用途注释。整份序列化会把这些全抹掉——用户下次打开只会看到一堆键值对。
/// 所以只动 `groups = [ ... ]` 那一段：原有组的行注释按 id 复用，新增组给空注释。
pub fn set_groups(domain_toml: &std::path::Path, ids: &[String]) -> Result<(), String> {
    let text = std::fs::read_to_string(domain_toml)
        .map_err(|e| format!("读取 {} 失败：{e}", domain_toml.display()))?;
    let lines: Vec<&str> = text.lines().collect();

    // 定位 groups = [ ... ] 的行区间
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with("groups") && l.contains('['))
        .ok_or_else(|| "domain.toml 里没有 groups 数组".to_string())?;
    let mut end = None;
    for (i, l) in lines.iter().enumerate().skip(start) {
        if l.contains(']') {
            end = Some(i);
            break;
        }
    }
    let end = end.ok_or_else(|| "groups 数组没有闭合的 ]".to_string())?;

    // 收集原有注释（组 id → 行尾注释）
    let mut comments: std::collections::BTreeMap<String, String> = Default::default();
    for l in &lines[start..=end] {
        let t = l.trim();
        if !t.starts_with('"') {
            continue;
        }
        if let Some(close) = t[1..].find('"') {
            let id = &t[1..1 + close];
            let tail = &t[1 + close + 1..];
            if let Some(c) = tail.find('#') {
                comments.insert(id.to_string(), tail[c..].trim_end().to_string());
            }
        }
    }

    let indent = lines[start]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect::<String>();
    let body_indent = format!("{indent}    ");

    let mut out: Vec<String> = lines[..start].iter().map(|s| s.to_string()).collect();
    out.push(format!("{indent}groups = ["));
    for id in ids {
        let c = comments.get(id).cloned().unwrap_or_default();
        if c.is_empty() {
            out.push(format!("{body_indent}\"{id}\","));
        } else {
            // 注释列对齐到同一列（原来就是这么写的）
            out.push(format!("{body_indent}\"{id}\",{}{c}", " ".repeat(12usize.saturating_sub(id.len()))));
        }
    }
    out.push(format!("{indent}]"));
    out.extend(lines[end + 1..].iter().map(|s| s.to_string()));

    let mut final_text = out.join("\n");
    if text.ends_with('\n') {
        final_text.push('\n');
    }
    std::fs::write(domain_toml, final_text)
        .map_err(|e| format!("写入 {} 失败：{e}", domain_toml.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_domains() -> PathBuf {
        // crates/wpa-engine → 项目根
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("domains"))
            .expect("定位项目根失败")
    }

    #[test]
    fn front_matter_splits_and_defaults() {
        let raw = "---\nid: x\ninject: on_keyword\nbudget_chars: 120\npriority: [1, 3, 5]\n---\n# 正文\n内容\n";
        let (meta, body) = split_front_matter(raw);
        assert_eq!(meta_get(&meta, "inject").as_deref(), Some("on_keyword"));
        assert!(body.contains("# 正文"));
        let w = parse_workflow(raw);
        assert_eq!(w.inject, "on_keyword");
        assert_eq!(w.budget_chars, 120);
        assert_eq!(w.priority, vec![1, 3, 5]);
        assert!(!w.text.contains("budget_chars"));
    }

    #[test]
    fn no_front_matter_keeps_whole_body() {
        let raw = "# 直接正文\n没有 front matter\n";
        let (meta, body) = split_front_matter(raw);
        assert!(meta.is_empty());
        assert_eq!(body, raw);
        let w = parse_workflow(raw);
        assert_eq!(w.inject, "always");
        assert!(w.text.contains("没有 front matter"));
    }

    #[test]
    fn unclosed_front_matter_falls_back_to_body() {
        // 少写一个 `---` 时不能把内容全吞掉
        let raw = "---\ninject: always\n# 正文\n";
        let (meta, body) = split_front_matter(raw);
        assert!(meta.is_empty(), "未闭合应整体视为正文");
        assert_eq!(body, raw);
    }

    #[test]
    fn workflow_fitted_respects_budget() {
        let w = WorkflowSection {
            inject: "always".into(),
            budget_chars: 10,
            priority: vec![],
            text: "一二三四五六七八九十十一".into(),
        };
        let s = w.fitted(0);
        assert!(s.starts_with("一二三四五六七八九十"));
        assert!(!s.contains("十一"), "超出预算的尾部不应出现：{s}");
        assert!(s.contains("kb.search"));
        // 预算足够时原样返回
        assert_eq!(w.fitted(1000), w.text);
    }

    #[test]
    fn loads_default_domain() {
        let root = repo_domains();
        if !root.join("default/domain.toml").is_file() {
            return; // 独立拷贝时可能没有 domains/
        }
        let pack = load(&root.join("default")).expect("默认领域应可装载");
        assert_eq!(pack.manifest.id, "default");
        assert!(!pack.manifest.name.is_empty());
        assert!(pack.persona().is_some(), "默认领域必须带角色设定");
        let w = pack.workflow.as_ref().expect("默认领域必须有工作范式");
        assert!(w.text.len() > 200, "工作范式不该是空壳");
        assert!(pack.knowledge_entries > 0, "默认领域应有常识条目");
        assert!(!pack.manifest.groups.is_empty(), "默认领域应引用组");
        let sec = pack.workflow_section().unwrap();
        assert!(sec.starts_with("## 工作范式"));
    }

    #[test]
    fn lists_domains_with_active_flag() {
        let root = repo_domains();
        if !root.is_dir() {
            return;
        }
        let rows = list(&root, "default");
        assert!(rows.iter().any(|r| r.id == "default" && r.active && r.error.is_none()));
    }

    #[test]
    fn missing_domain_degrades_with_warning() {
        let mut cfg = AppConfig::default();
        cfg.domain.active = "no-such-domain".into();
        cfg.domain.dir = Some("/definitely/not/here".into());
        let (pack, warns) = load_active(&cfg);
        assert!(pack.is_none());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("已降级"));
    }

    #[test]
    fn default_missing_is_not_an_error() {
        let mut cfg = AppConfig::default();
        cfg.domain.dir = Some("/definitely/not/here".into());
        let (pack, warns) = load_active(&cfg);
        assert!(pack.is_none());
        assert!(warns.is_empty(), "默认领域缺失是合法的纯基座模式");
    }

    #[test]
    fn ensure_default_writes_template_once_then_loads() {
        let tmp = std::env::temp_dir().join(format!("wpa_domain_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let written = ensure_default(&tmp).expect("应能写出模板");
        assert_eq!(written.len(), DEFAULT_TEMPLATE.len(), "{written:?}");
        // 第二次不该再写（也绝不能覆盖）
        let again = ensure_default(&tmp).expect("重复调用应成功");
        assert!(again.is_empty(), "已存在时不应重写：{again:?}");
        // 写出来的东西必须是**可装载的真领域**，否则模板等于骗人
        let pack = load(&tmp.join("default")).expect("模板应能装载");
        assert_eq!(pack.manifest.id, "default");
        assert!(pack.persona().is_some());
        assert!(pack.workflow.is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_default_never_overwrites_user_edit() {
        let tmp = std::env::temp_dir().join(format!("wpa_domain_e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        ensure_default(&tmp).expect("首次写出");
        let p = tmp.join("default/workflow.md");
        std::fs::write(&p, "---\ninject: always\n---\n# 我自己改的\n").unwrap();
        ensure_default(&tmp).expect("重复调用");
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("我自己改的"), "用户改动被覆盖了：{after}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
