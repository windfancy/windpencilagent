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
    /// 知识库声明（`[[kb]]`）：领域页「知识管理」只读展示的数据来源。
    /// 声明与实现分离——这里只登记"哪座库、在哪、干什么用"，读写走 `kb` 组工具。
    pub kb: Vec<KbDecl>,
    /// 领域**私有**的外部工具（`[[external]]`）。
    ///
    /// 与 `[[kb]]` 同定位：只属于这个领域，不进 `plugins/`、不进全局注册表、
    /// 不出现在能力页。领域切换 = 这批工具跟着一起换，别的领域看不见。
    pub external: Vec<ExternalDecl>,
}

/// 一个领域私有外部工具的声明（`[[external]]`）。
///
/// 两种来源：
/// - `kind = "declarative"`：领域目录里放一个带 `PnC.toml` 的声明式插件目录，
///   `path` 指向它。实现是 http / command / python / primitive 之一。
/// - `kind = "exe"`：直接登记一个本机可执行程序，`path` + `usage` + `args` 就够，
///   不需要写 PnC.toml（省掉一次"手书清单"）。
///
/// **不受插件体系管理**：没有安装/卸载/组的概念，领域目录在就有，删掉就没。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExternalDecl {
    /// `declarative` / `exe`
    pub kind: String,
    /// `declarative`：相对领域目录的插件目录；`exe`：可执行文件路径（相对领域目录或绝对）
    pub path: String,
    /// 展示名（空 = 取 id / 文件名）
    pub name: String,
    /// 使用说明（拼进工具描述，模型据此决定怎么调）
    pub usage: String,
    /// 工具命名空间（空 = `ext`）；最终工具名 = `<namespace>.<动作>`
    pub namespace: String,
    /// 风险档；空 = `high`（外部程序 fail-closed，仍走审批链）
    pub risk: String,
    /// `exe` 专用：固定命令行参数，支持 `{{参数名}}` 占位
    pub args: Vec<String>,
    /// `exe` 专用：占位符 → 说明（决定工具参数 schema）
    pub params: std::collections::BTreeMap<String, String>,
    /// `exe` 专用：传给 stdin 的模板（空 = 不传）
    pub stdin: String,
}

impl ExternalDecl {
    pub fn kind_norm(&self) -> &str {
        match self.kind.trim().to_ascii_lowercase().as_str() {
            "exe" | "command" | "program" => "exe",
            _ => "declarative",
        }
    }

    pub fn namespace_norm(&self) -> String {
        let n = self.namespace.trim();
        if n.is_empty() {
            "ext".to_string()
        } else {
            n.to_string()
        }
    }

    /// 从 `path` 推出一个可用的动作名（exe 用文件名主名，declarative 用目录名）。
    pub fn action_name(&self) -> String {
        let p = std::path::Path::new(self.path.trim());
        let stem = p
            .file_stem()
            .or_else(|| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "tool".to_string());
        let mut out: String = stem
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
            .collect();
        while out.contains("__") {
            out = out.replace("__", "_");
        }
        let out = out.trim_matches('_').to_string();
        if out.is_empty() {
            "tool".to_string()
        } else {
            out
        }
    }

    pub fn display_name(&self) -> String {
        let n = self.name.trim();
        if !n.is_empty() {
            return n.to_string();
        }
        let p = std::path::Path::new(self.path.trim());
        p.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.action_name())
    }
}

/// 领域页「外部工具」的一行。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ExternalRow {
    /// 原始声明下标（UI 操作用）
    pub index: usize,
    pub kind: String,
    pub name: String,
    /// 声明里的路径原文
    pub path: String,
    pub usage: String,
    /// 解析后的绝对路径（UI 只做 tooltip，不直接展示）
    pub resolved: String,
    /// 文件/目录是否真的在
    pub exists: bool,
    /// 这个声明实际产出哪些工具（`namespace.action`）
    pub tools: Vec<String>,
    /// 不可用原因（空 = 正常）
    pub error: String,
}

/// 一座知识库的声明（`[[kb]]`）。
///
/// 为什么要有这层声明：知识库不是领域包的私有资产，而是外部数据（漏洞库、
/// 词表、向量集）。领域只描述"我要用哪座库"，条数与内容由数据自己决定。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct KbDecl {
    /// `sqlite` / `vector` / `csv` / `file`
    pub kind: String,
    /// 存储位置：相对领域目录；以 `domains/` 开头时相对数据根
    pub location: String,
    /// 用途一句话（界面原样显示）
    pub purpose: String,
}

/// 领域页「知识管理」的一行（只读）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct KbRow {
    /// 声明的类型原值
    pub kind: String,
    /// 类型中文名（`data-core` 口径）
    pub label: String,
    /// 声明里的位置原文（界面照抄，不给用户看解析后的绝对路径）
    pub location: String,
    pub purpose: String,
    /// 实际条目数；0 且 `error` 非空 = 数不出来
    pub entries: usize,
    /// 统计失败原因（文件不存在 / 表坏了），空 = 正常
    pub error: String,
}

/// 把 `location` 解析成实际文件路径。
///
/// 两种写法都接受（用户手写 config 时不该被路径前缀绊住）：
/// - `vuln.db` → 相对领域目录 `domains/<id>/vuln.db`
/// - `domains/<id>/vuln.db` → 相对数据根（与快照里 `data_root` 的展示口径一致）
pub fn kb_abs(dir: &Path, location: &str) -> PathBuf {
    let loc = location.trim().replace('\\', "/");
    let loc = loc.trim_start_matches("./");
    if let Some(rest) = loc
        .strip_prefix("domains/")
        .or_else(|| loc.strip_prefix("domain/"))
    {
        // dir = <data>/domains/<id> → 父目录就是 domains 根
        if let Some(domains_root) = dir.parent() {
            return domains_root.join(rest);
        }
    }
    dir.join(loc)
}

/// 把声明解析成可展示的行（含条目数统计）。
pub fn kb_rows(dir: &Path, decls: &[KbDecl]) -> Vec<KbRow> {
    decls
        .iter()
        .map(|d| {
            let kind = d.kind.trim().to_ascii_lowercase();
            let path = kb_abs(dir, &d.location);
            let (entries, error) = match data_core::count_entries(&kind, &path) {
                Ok(n) => (n, String::new()),
                Err(e) => (0, e),
            };
            KbRow {
                label: data_core::kind_label(&kind).to_string(),
                kind,
                location: d.location.trim().to_string(),
                purpose: d.purpose.trim().to_string(),
                entries,
                error,
            }
        })
        .collect()
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
    /// `[[kb]]` 声明（读写走 `kb` 组工具，这里只做展示与计数）
    pub kb: Vec<KbDecl>,
    /// `[[external]]` 声明：领域私有外部工具（不进 plugins/、不进全局注册表）
    pub external: Vec<ExternalDecl>,
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
    /// 知识库（只读清单：类型 / 位置 / 用途 / 条目数）
    pub kb: Vec<KbRow>,
    /// 领域私有外部工具（只读清单：类型 / 路径 / 存在性 / 产出的工具名）
    pub external: Vec<ExternalRow>,
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
        kb: file.kb,
        external: file.external,
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
                kb: kb_rows(&pack.dir, &pack.kb),
                external: external_rows(&pack.dir, &pack.external),
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

/// 领域私有外部工具的展示行（含存在性与实际工具名）。
///
/// 只做"读"：文件在不在、能产出哪些工具名。真正构建工具见 [`external_tools`]。
pub fn external_rows(dir: &Path, decls: &[ExternalDecl]) -> Vec<ExternalRow> {
    decls
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let abs = resolve_external_path(dir, &d.path);
            let kind = d.kind_norm().to_string();
            let (exists, tools, error) = if d.path.trim().is_empty() {
                (false, Vec::new(), "未填写路径".to_string())
            } else if kind == "exe" {
                let ok = abs.is_file();
                (
                    ok,
                    vec![format!("{}.{}", d.namespace_norm(), d.action_name())],
                    if ok { String::new() } else { format!("找不到程序：{}", abs.display()) },
                )
            } else {
                let ok = abs.join("PnC.toml").is_file();
                let mut names = Vec::new();
                let mut err = if ok { String::new() } else {
                    format!("找不到清单：{}", abs.join("PnC.toml").display())
                };
                if ok {
                    match tree_core::PnCManifest::load(&abs) {
                        Ok(m) => {
                            names = m.tool.iter().map(|t| t.name.clone()).collect();
                            if names.is_empty() {
                                err = "清单里没有 [[tool]]".to_string();
                            }
                        }
                        Err(e) => err = e,
                    }
                }
                (ok, names, err)
            };
            ExternalRow {
                index: i,
                kind,
                name: d.display_name(),
                path: d.path.clone(),
                usage: d.usage.clone(),
                resolved: abs.to_string_lossy().to_string(),
                exists,
                tools,
                error,
            }
        })
        .collect()
}

/// 把声明里的路径解析成绝对路径：绝对路径原样，相对路径相对领域目录。
pub fn resolve_external_path(dir: &Path, path: &str) -> PathBuf {
    let p = std::path::Path::new(path.trim());
    if p.is_absolute() {
        return p.to_path_buf();
    }
    dir.join(p)
}

/// 从声明构建工具（领域私有，不进注册表、不进能力页）。
///
/// 返回 `(工具, 问题)`。任何一条声明出问题都只影响它自己：别的工具照常可用，
/// 问题进装配告警，领域页也能看到具体原因（见 [`external_rows`]）。
pub fn external_tools(
    dir: &Path,
    decls: &[ExternalDecl],
    domain_id: &str,
    invoker: Option<std::sync::Arc<dyn tree_core::exec::PrimitiveInvoker>>,
) -> (Vec<std::sync::Arc<dyn wpa_api::ApiTool>>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for d in decls {
        if d.path.trim().is_empty() {
            problems.push(format!("领域 {domain_id}：外部工具 `{}` 未填路径", d.display_name()));
            continue;
        }
        let abs = resolve_external_path(dir, &d.path);
        match d.kind_norm() {
            "exe" => {
                if !abs.is_file() {
                    problems.push(format!(
                        "领域 {domain_id}：外部程序不存在 {}",
                        abs.display()
                    ));
                    continue;
                }
                let decl = exe_tool_decl(d);
                if decl.name.trim().is_empty() {
                    problems.push(format!("领域 {domain_id}：外部程序 `{}` 工具名为空", d.path));
                    continue;
                }
                let mut tools = tree_core::exec::build_tools(
                    &[decl],
                    dir,
                    &format!("ext:{domain_id}"),
                    invoker.clone(),
                );
                out.append(&mut tools);
            }
            _ => {
                let pnc = match tree_core::PnCManifest::load(&abs) {
                    Ok(m) => m,
                    Err(e) => {
                        problems.push(format!(
                            "领域 {domain_id}：声明式插件 `{}` 不可用：{e}",
                            d.path
                        ));
                        continue;
                    }
                };
                if pnc.tool.is_empty() {
                    problems.push(format!(
                        "领域 {domain_id}：声明式插件 `{}` 没有 [[tool]]",
                        d.path
                    ));
                    continue;
                }
                let errs = pnc.validate();
                if !errs.is_empty() {
                    problems.push(format!(
                        "领域 {domain_id}：声明式插件 `{}` 校验失败：{}",
                        d.path,
                        errs.join("；")
                    ));
                    continue;
                }
                let mut tools = tree_core::exec::build_tools(
                    &pnc.tool,
                    &abs,
                    &format!("ext:{domain_id}"),
                    invoker.clone(),
                );
                out.append(&mut tools);
            }
        }
    }
    (out, problems)
}

/// 把一条 `[external]` 声明翻成 `ToolDecl`（只支持 exe 这一路）。
fn exe_tool_decl(d: &ExternalDecl) -> tree_core::ToolDecl {
    // 参数表 → JSON Schema 形状（与 PnC.toml 里 `parameters = { ... }` 等价）
    let mut props = toml::map::Map::new();
    for (k, v) in &d.params {
        let mut one = toml::map::Map::new();
        one.insert("type".into(), toml::Value::String("string".into()));
        if !v.trim().is_empty() {
            one.insert("description".into(), toml::Value::String(v.clone()));
        }
        props.insert(k.clone(), toml::Value::Table(one));
    }
    let mut schema = toml::map::Map::new();
    schema.insert("type".into(), toml::Value::String("object".into()));
    schema.insert("properties".into(), toml::Value::Table(props));

    let desc = {
        let name = d.display_name();
        let usage = d.usage.trim();
        if usage.is_empty() {
            name
        } else {
            format!("{name}——{usage}")
        }
    };
    let risk = {
        let r = d.risk.trim().to_ascii_lowercase();
        if r.is_empty() {
            // 外部程序 fail-closed：默认高风险，仍然逐次审批
            "high".to_string()
        } else {
            r
        }
    };
    tree_core::ToolDecl {
        name: format!("{}.{}", d.namespace_norm(), d.action_name()),
        description: desc,
        risk: Some(risk),
        exec: tree_core::ExecSpec::Command {
            program: d.path.trim().to_string(),
            args: d.args.clone(),
            stdin: if d.stdin.trim().is_empty() {
                None
            } else {
                Some(d.stdin.clone())
            },
        },
        parameters: Some(toml::Value::Table(schema)),
        timeout_secs: None,
        concurrency_safe: Some(false),
        ..Default::default()
    }
}

/// 覆盖写 `domain.toml` 的 `[[external]]` 段（其余内容一字不动）。
///
/// 与 [`set_groups`] 的取舍不同：`groups` 是手写的一段数组，行注释有信息量，
/// 所以精确改写；`[[external]]` 是 UI 加出来的结构（含嵌套表），逐行改反而容易
/// 改坏，整段替换更稳。段落之前的说明性注释保留。
pub fn set_external(domain_toml: &std::path::Path, decls: &[ExternalDecl]) -> Result<(), String> {
    let text = std::fs::read_to_string(domain_toml)
        .map_err(|e| format!("读取 {} 失败：{e}", domain_toml.display()))?;

    let mut out: Vec<String> = Vec::new();
    let mut in_ext = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            // `[[external]]` 与 `[external.params]` 都算这个段；其它表头结束它
            in_ext = t == "[[external]]" || t.starts_with("[external.");
        }
        if !in_ext {
            out.push(line.to_string());
        }
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    if !decls.is_empty() {
        out.push(String::new());
        out.push("# ---------------------------------------------------------------------------".into());
        out.push("# 领域私有外部工具（由领域页维护；不进 plugins/、不进全局注册表）".into());
        out.push("# ---------------------------------------------------------------------------".into());
        for d in decls {
            out.push(String::new());
            out.push("[[external]]".into());
            out.push(format!("kind = \"{}\"", toml_escape(d.kind_norm())));
            out.push(format!("path = \"{}\"", toml_escape(d.path.trim())));
            if !d.name.trim().is_empty() {
                out.push(format!("name = \"{}\"", toml_escape(d.name.trim())));
            }
            if !d.usage.trim().is_empty() {
                out.push(format!("usage = \"{}\"", toml_escape(d.usage.trim())));
            }
            if !d.namespace.trim().is_empty() {
                out.push(format!("namespace = \"{}\"", toml_escape(d.namespace.trim())));
            }
            if !d.risk.trim().is_empty() {
                out.push(format!("risk = \"{}\"", toml_escape(d.risk.trim())));
            }
            if !d.args.is_empty() {
                let args = d
                    .args
                    .iter()
                    .map(|a| format!("\"{}\"", toml_escape(a)))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(format!("args = [{args}]"));
            }
            if !d.stdin.trim().is_empty() {
                out.push(format!("stdin = \"{}\"", toml_escape(&d.stdin)));
            }
            if !d.params.is_empty() {
                out.push("[external.params]".into());
                for (k, v) in &d.params {
                    out.push(format!("{} = \"{}\"", k, toml_escape(v)));
                }
            }
        }
    }
    out.push(String::new());
    atomic_write(domain_toml, &out.join("\n"))
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

// ---------------------------------------------------------------------------
// 领域 CRUD（自由函数：显式 root，不依赖全局配置，便于上层/测试直接调用）
// ---------------------------------------------------------------------------

/// 领域 id 校验：非空、仅字母数字与 `_` `-` `.`。
///
/// 额外排除 `.` / `..`：它们虽然是"安全字符"，但作为目录名会越出 root
/// （这是系统边界，必须挡住路径穿越）。
fn valid_domain_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("领域 id 不能为空".to_string());
    }
    if id == "." || id == ".." {
        return Err(format!("非法领域 id：{id}（不能是 . 或 ..）"));
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return Err(format!("非法领域 id：{id}（只允许字母数字、_、-、.）"));
    }
    Ok(())
}

/// 领域内可读写的四个描述文件（白名单）。
fn valid_domain_file(file: &str) -> Result<(), String> {
    if matches!(file, "domain.toml" | "workflow.md" | "knowledge.md" | "constraints.toml") {
        Ok(())
    } else {
        Err(format!("非法领域文件名：{file}（只允许 domain.toml / workflow.md / knowledge.md / constraints.toml）"))
    }
}

/// 同目录临时文件 + rename：读者只会看到写完的整份内容，不会读到半截文件。
fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), nanos));
    std::fs::write(&tmp, text)
        .map_err(|e| format!("写入临时文件 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("原子替换 {} 失败：{e}", path.display())
    })
}

/// 把模板里的 `name = "…"` 行替换成指定名字；找不到就原样返回模板。
fn replace_template_name(template: &str, name: &str) -> String {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    let mut done = false;
    let mut lines: Vec<String> = Vec::new();
    for line in template.lines() {
        let trimmed = line.trim_start();
        let key = trimmed.split('=').next().unwrap_or("").trim();
        if !done && key == "name" {
            let indent = &line[..line.len() - trimmed.len()];
            lines.push(format!("{indent}name = \"{escaped}\""));
            done = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !done {
        return template.to_string();
    }
    let mut out = lines.join("\n");
    if template.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// 新建领域：按 `DEFAULT_TEMPLATE` 写四个文件，`domain.toml` 的 name 换成传入值。
/// 目录已存在时报错（绝不覆盖用户已有领域）。
pub fn create_domain(root: &Path, id: &str, name: &str) -> Result<(), String> {
    valid_domain_id(id)?;
    let dir = root.join(id);
    if dir.exists() {
        return Err(format!("领域 {id} 已存在：{}", dir.display()));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建领域目录 {} 失败：{e}", dir.display()))?;
    for (file, body) in DEFAULT_TEMPLATE {
        let text = if *file == "domain.toml" {
            replace_template_name(body, name)
        } else {
            (*body).to_string()
        };
        atomic_write(&dir.join(file), &text)?;
    }
    Ok(())
}

/// 删除领域（递归）；`default` 是模板基底，永远不允许删除。
pub fn remove_domain(root: &Path, id: &str) -> Result<(), String> {
    valid_domain_id(id)?;
    if id == "default" {
        return Err("不允许删除默认领域 default".to_string());
    }
    let dir = root.join(id);
    std::fs::remove_dir_all(&dir)
        .map_err(|e| format!("删除领域目录 {} 失败：{e}", dir.display()))
}

/// 写领域的单个描述文件（白名单校验 + 需要时建目录 + 原子写）。
pub fn write_domain_file(root: &Path, id: &str, file: &str, text: &str) -> Result<(), String> {
    valid_domain_id(id)?;
    valid_domain_file(file)?;
    let dir = root.join(id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建领域目录 {} 失败：{e}", dir.display()))?;
    atomic_write(&dir.join(file), text)
}

/// 读领域的单个描述文件（白名单校验）。文件不存在时报错。
pub fn read_domain_file(root: &Path, id: &str, file: &str) -> Result<String, String> {
    valid_domain_id(id)?;
    valid_domain_file(file)?;
    let path = root.join(id).join(file);
    std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))
}

// ---------------------------------------------------------------------------
// 领域概要（≤300 字）
// ---------------------------------------------------------------------------

/// 概要字数上限（界面上的计数器与校验共用这一份口径）。
pub const SUMMARY_MAX_CHARS: usize = 300;

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace('\t', " ")
}

/// 写回 `[domain].summary`，**只改这一行**。
///
/// 为什么不用 toml 反序列化再整体重写：`domain.toml` 是用户手写的，
/// 里面有人写的注释与顺序；程序回收一遍会把它们全抹掉，用户下次打开会不认识。
pub fn set_summary(root: &Path, id: &str, summary: &str) -> Result<(), String> {
    valid_domain_id(id)?;
    let s = summary.trim();
    let n = s.chars().count();
    if n > SUMMARY_MAX_CHARS {
        return Err(format!("领域概要不能超过 {SUMMARY_MAX_CHARS} 字（当前 {n} 字）"));
    }
    let path = root.join(id).join("domain.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_domain = false;
    let mut done = false;
    let mut domain_at: Option<usize> = None;
    for i in 0..lines.len() {
        let line = lines[i].trim().to_string();
        let t = line.as_str();
        if t.starts_with('[') {
            in_domain = t == "[domain]";
            if in_domain {
                domain_at = Some(i);
            }
            continue;
        }
        if in_domain && !done {
            let raw = lines[i].clone();
            let trimmed = raw.trim_start();
            if let Some(rest) = trimmed.strip_prefix("summary") {
                let is_key = rest.trim_start().starts_with('=');
                if is_key {
                    let indent = &raw[..raw.len() - trimmed.len()];
                    lines[i] = format!("{indent}summary = \"{}\"", toml_escape(s));
                    done = true;
                }
            }
        }
    }
    if !done {
        let Some(at) = domain_at else {
            return Err(format!("{} 里没有 [domain] 段落，无法写入概要", path.display()));
        };
        lines.insert(at + 1, format!("summary = \"{}\"", toml_escape(s)));
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    atomic_write(&path, &out)?;
    // 写完立刻回读校验：概要里带引号/反斜杠时，写坏了要当场知道
    let back = load(&root.join(id))?;
    if back.manifest.summary.trim() != s {
        return Err("概要写入后回读不一致（可能包含无法直接写进 TOML 的字符）".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AI 生成领域配置
// ---------------------------------------------------------------------------

/// 生成配置时给模型的四个输出键（顺序即界面展示顺序）。
pub const GEN_FILES: [&str; 4] = ["domain.toml", "workflow.md", "knowledge.md", "constraints.toml"];

/// 生成提示词：system 说明「只输出 JSON」，user 给概要 + 现有文件 + 格式要求。
///
/// 为什么要把现有文件也喂进去：模型需要沿用当前 `[domain].id` 与 `groups`，
/// 否则生成出来的包要么改掉了 id（装载后与目录名不符），要么把组引用清空（工具面归零）。
pub fn gen_prompt(pack: &DomainPack, summary: &str) -> (String, String) {
    let system = "你是领域配置生成器。只输出一个 JSON 对象，不要解释、不要 Markdown 代码围栏。\
JSON 的键固定为 domain.toml、workflow.md、knowledge.md、constraints.toml，值都是文件的完整文本。\
domain.toml 必须保留原有的 [domain].id 与 groups 声明，其余字段按概要重写。"
        .to_string();

    let mut user = String::new();
    user.push_str(&format!("【领域概要】\n{}\n\n", summary.trim()));
    user.push_str(&format!(
        "【当前 id / 名称 / 类型】\nid = {}\nname = {}\nkind = {}\n\n",
        pack.manifest.id, pack.manifest.name, pack.manifest.kind
    ));
    user.push_str(&format!(
        "【必须保留的组引用】\n{}\n\n",
        if pack.manifest.groups.is_empty() {
            "（无）".to_string()
        } else {
            pack.manifest.groups.join(", ")
        }
    ));
    user.push_str("【当前 domain.toml】\n");
    user.push_str(&read_or_empty(&pack.dir.join("domain.toml")));
    user.push_str("\n【格式要求】\n");
    user.push_str(
        "1. domain.toml：TOML，含 [domain]（id/name/kind/summary/persona/groups/workflow/knowledge/constraints）与 [params]（allow_high_risk/max_tool_rounds）。\
summary 不超过 300 字。\n\
2. workflow.md：Markdown，开头是 --- 包裹的 front matter（inject / budget_chars / priority），正文写工作步骤。\n\
3. knowledge.md：Markdown，每条知识一个 `## 标题` + 正文段落。\n\
4. constraints.toml：TOML 数组 [[constraint]]，每项含 id / rule / strength（hard|soft）/ when。\n",
    );
    (system, user)
}

fn read_or_empty(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// 从模型回复里抠出 JSON 对象（容忍 ```json 围栏与前后废话）。
pub fn extract_json_object(raw: &str) -> Result<serde_json::Value, String> {
    let t = raw.trim();
    let start = t.find('{').ok_or_else(|| "模型回复里没有 JSON 对象".to_string())?;
    let end = t.rfind('}').ok_or_else(|| "模型回复里的 JSON 不完整".to_string())?;
    if end <= start {
        return Err("模型回复里的 JSON 不完整".into());
    }
    serde_json::from_str(&t[start..=end]).map_err(|e| format!("解析模型输出失败：{e}"))
}

/// 生成结果落盘：先 `.bak` 备份，全写完后重新装载校验，失败则回滚。
///
/// 回滚是必要的：四个文件互相引用（domain.toml 指向另外三个），
/// 只有三个写成功就返回错误，会让领域处于"说得好好的但打不开"的状态。
pub fn apply_generated(dir: &Path, payload: &serde_json::Value) -> Result<Vec<String>, String> {
    let Some(obj) = payload.as_object() else {
        return Err("模型输出不是 JSON 对象".into());
    };
    let mut pending: Vec<(&str, String)> = Vec::new();
    for f in GEN_FILES {
        let v = obj
            .get(f)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("模型输出缺少 {f}（或内容为空）"))?;
        pending.push((f, v.to_string()));
    }

    // 备份：同名 .bak 覆盖写（只保留最近一次，避免堆一堆看不懂的副本）
    let mut backups: Vec<(&str, Option<String>)> = Vec::new();
    for (f, _) in &pending {
        let p = dir.join(f);
        let old = std::fs::read_to_string(&p).ok();
        if let Some(text) = &old {
            atomic_write(&dir.join(format!("{f}.bak")), text)?;
        }
        backups.push((f, old));
    }

    for (f, text) in &pending {
        atomic_write(&dir.join(f), text)?;
    }

    // 校验：能装载 + id 没被改掉
    match load(dir) {
        Ok(pack) => {
            let dir_id = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if !dir_id.is_empty() && pack.manifest.id != dir_id {
                let e = format!("生成结果里 [domain].id = {}，与目录名 {} 不一致，已回滚", pack.manifest.id, dir_id);
                rollback(dir, &backups)?;
                return Err(e);
            }
        }
        Err(e) => {
            rollback(dir, &backups)?;
            return Err(format!("生成的配置无法装载（{e}），已回滚"));
        }
    }
    Ok(pending.iter().map(|(f, _)| f.to_string()).collect())
}

fn rollback(dir: &Path, backups: &[(&str, Option<String>)]) -> Result<(), String> {
    for (f, old) in backups {
        match old {
            Some(text) => atomic_write(&dir.join(f), text)?,
            None => {
                let _ = std::fs::remove_file(dir.join(f));
            }
        }
    }
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

    fn test_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wpa_domain_crud_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn domain_crud_roundtrip() {
        let root = test_root("roundtrip");
        create_domain(&root, "my-domain.v2", "我的领域").unwrap();
        let dir = root.join("my-domain.v2");
        for (f, _) in DEFAULT_TEMPLATE {
            assert!(dir.join(f).is_file(), "{f} 应写出");
        }
        let manifest = read_domain_file(&root, "my-domain.v2", "domain.toml").unwrap();
        assert!(manifest.contains("name = \"我的领域\""));
        assert!(!manifest.contains("name = \"通用对话\""));
        // 写出的领域应能真正装载（name 替换没破坏 TOML）
        let pack = load(&dir).unwrap();
        assert_eq!(pack.manifest.name, "我的领域");
        assert_eq!(pack.manifest.id, "default", "模板 id 保持不变");

        // 覆盖写 + 读回（原子写路径）
        let wf = "---\ninject: manual\n---\n新流程\n";
        write_domain_file(&root, "my-domain.v2", "workflow.md", wf).unwrap();
        assert_eq!(read_domain_file(&root, "my-domain.v2", "workflow.md").unwrap(), wf);

        // 重复创建报错，且不改动已有内容
        assert!(create_domain(&root, "my-domain.v2", "另一个名字").is_err());
        assert_eq!(read_domain_file(&root, "my-domain.v2", "workflow.md").unwrap(), wf);

        remove_domain(&root, "my-domain.v2").unwrap();
        assert!(!dir.exists());
        assert!(remove_domain(&root, "my-domain.v2").is_err(), "删除不存在的领域应报错");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn domain_rejects_unsafe_ids_and_files() {
        let root = test_root("unsafe");
        for bad in ["", ".", "..", "a/b", "a\\b", "../evil", "中文"] {
            assert!(create_domain(&root, bad, "x").is_err(), "id {bad:?} 应被拒绝");
            assert!(write_domain_file(&root, bad, "domain.toml", "x").is_err());
            assert!(read_domain_file(&root, bad, "domain.toml").is_err());
        }
        create_domain(&root, "safe", "安全").unwrap();
        assert!(write_domain_file(&root, "safe", "evil.toml", "x").is_err());
        assert!(read_domain_file(&root, "safe", "../domain.toml").is_err());
        assert!(read_domain_file(&root, "safe", "missing.md").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_domain_cannot_be_removed() {
        let root = test_root("default");
        create_domain(&root, "default", "自定义名").unwrap();
        assert!(remove_domain(&root, "default").is_err());
        assert!(root.join("default/domain.toml").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summary_write_keeps_comments_and_caps_length() {
        let root = test_root("summary");
        create_domain(&root, "sec", "安全").unwrap();
        // 加一行用户注释，验证写概要不会重排/抹掉其他行
        let p = root.join("sec/domain.toml");
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.insert_str(0, "# 用户手写注释\n");
        std::fs::write(&p, &text).unwrap();

        set_summary(&root, "sec", "做漏洞信息整理与复盘").unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with("# 用户手写注释"), "注释必须保留");
        assert!(after.contains("summary = \"做漏洞信息整理与复盘\""));
        assert_eq!(load(&root.join("sec")).unwrap().manifest.summary, "做漏洞信息整理与复盘");

        // 引号 / 换行必须被转义后仍能回读到原值
        set_summary(&root, "sec", "带\"引号\"与换行\n第二行").unwrap();
        assert_eq!(load(&root.join("sec")).unwrap().manifest.summary, "带\"引号\"与换行\n第二行");

        let long: String = "字".repeat(SUMMARY_MAX_CHARS + 1);
        assert!(set_summary(&root, "sec", &long).is_err(), "超过 300 字必须拒绝");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_rows_resolve_both_location_forms() {
        let root = test_root("kbrows");
        create_domain(&root, "sec", "安全").unwrap();
        std::fs::write(root.join("sec/vuln.csv"), "cve,name\n1,a\n2,b\n").unwrap();
        let dir = root.join("sec");
        let rows = kb_rows(
            &dir,
            &[
                KbDecl { kind: "csv".into(), location: "vuln.csv".into(), purpose: "漏洞".into() },
                KbDecl { kind: "csv".into(), location: "domains/sec/vuln.csv".into(), purpose: "同上".into() },
                KbDecl { kind: "sqlite".into(), location: "nope.db".into(), purpose: "缺文件".into() },
            ],
        );
        assert_eq!(rows[0].entries, 2, "相对领域目录要能解析");
        assert_eq!(rows[1].entries, 2, "domains/ 前缀要按数据根解析");
        assert_eq!(rows[0].label, "csv 数据");
        assert!(!rows[2].error.is_empty(), "缺失的库要给出原因而不是静默 0 条");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn generated_payload_is_validated_and_rolled_back() {
        let root = test_root("gen");
        create_domain(&root, "sec", "安全").unwrap();
        let dir = root.join("sec");
        let before = std::fs::read_to_string(dir.join("domain.toml")).unwrap();

        // 缺文件 → 报错且不动盘
        let bad = serde_json::json!({ "domain.toml": "x" });
        assert!(apply_generated(&dir, &bad).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("domain.toml")).unwrap(), before);

        // 生成的 toml 语法坏了 → 回滚到原内容
        let broken = serde_json::json!({
            "domain.toml": "[domain\nid = ",
            "workflow.md": "# x",
            "knowledge.md": "## a\nb",
            "constraints.toml": "",
        });
        assert!(apply_generated(&dir, &broken).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("domain.toml")).unwrap(), before, "必须回滚");

        // 合法生成 → 写入成功并留有 .bak
        let ok = serde_json::json!({
            "domain.toml": "[domain]\nid = \"sec\"\nname = \"安全\"\nkind = \"pack\"\nsummary = \"新概要\"\ngroups = [\"file\"]\nworkflow = \"workflow.md\"\nknowledge = \"knowledge.md\"\nconstraints = \"constraints.toml\"\n\n[params]\nallow_high_risk = true\nmax_tool_rounds = 0\n",
            "workflow.md": "---\ninject: always\nbudget_chars: 800\n---\n# 步骤\n1. 收集\n",
            "knowledge.md": "## 常识\n内容\n",
            "constraints.toml": "[[constraint]]\nid = \"no-delete\"\nrule = \"不要删用户数据\"\nstrength = \"hard\"\nwhen = \"always\"\n",
        });
        let files = apply_generated(&dir, &ok).unwrap();
        assert_eq!(files.len(), 4);
        assert!(dir.join("domain.toml.bak").is_file(), "原文件必须留 .bak");
        assert_eq!(load(&dir).unwrap().manifest.summary, "新概要");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_object_is_extracted_from_fenced_reply() {
        let raw = "好的，结果如下：\n```json\n{\"domain.toml\":\"a\",\"workflow.md\":\"b\"}\n```\n希望有帮助";
        let v = extract_json_object(raw).unwrap();
        assert_eq!(v["domain.toml"], "a");
        assert!(extract_json_object("完全没有 JSON").is_err());
    }

    // ---- 领域私有外部工具（`[[external]]`） ----

    fn tmp_domain(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wpa_dom_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn exe_decl() -> ExternalDecl {
        ExternalDecl {
            kind: "exe".into(),
            path: "bin/tool.exe".into(),
            name: "示例工具".into(),
            usage: "把目标传进去".into(),
            args: vec!["scan".into(), "{{target}}".into()],
            params: [("target".to_string(), "目标地址".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn set_external_roundtrips_and_keeps_other_sections() {
        let dir = tmp_domain("ext");
        let file = dir.join("domain.toml");
        // 一份带注释与 groups 的清单：写 external 不能把别的内容毁掉
        std::fs::write(
            &file,
            "[domain]\nid = \"d\"\nname = \"D\"\n\n# 组引用\ngroups = [\n    \"file\",\n]\n\n[params]\nmax_tool_rounds = 8\n",
        )
        .unwrap();

        set_external(&file, &[exe_decl()]).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# 组引用"), "注释要留着");
        assert!(text.contains("max_tool_rounds = 8"), "别的段要留着");

        let f: DomainFile = toml::from_str(&text).unwrap();
        assert_eq!(f.external.len(), 1);
        let e = &f.external[0];
        assert_eq!(e.kind_norm(), "exe");
        assert_eq!(e.path, "bin/tool.exe");
        assert_eq!(e.usage, "把目标传进去");
        assert_eq!(e.args, vec!["scan", "{{target}}"]);
        assert_eq!(e.params.get("target").map(String::as_str), Some("目标地址"));
        assert_eq!(f.domain.groups, vec!["file"], "groups 不受影响");

        // 覆写：再写一遍不能累加出第二条
        set_external(&file, &[exe_decl()]).unwrap();
        let f2: DomainFile = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(f2.external.len(), 1, "重复写入应整段替换而不是追加");

        // 清空
        set_external(&file, &[]).unwrap();
        let f3: DomainFile = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(f3.external.is_empty());
        assert_eq!(f3.domain.id, "d", "清空 external 不影响领域本体");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_rows_report_missing_targets() {
        let dir = tmp_domain("extrows");
        let mut bad = exe_decl();
        bad.path = "nothere.exe".into();
        let rows = external_rows(&dir, &[exe_decl(), bad]);
        assert_eq!(rows.len(), 2);
        // 第一个：文件也不在（临时目录没建 bin/tool.exe）
        assert!(!rows[0].exists);
        assert!(rows[0].error.contains("找不到程序"), "{}", rows[0].error);
        // 但工具名仍然算出来了（排查时要知道"本来会叫什么名"）
        assert_eq!(rows[0].tools, vec!["ext.tool".to_string()]);
        assert_eq!(rows[1].tools, vec!["ext.nothere".to_string()]);

        // 造出文件后应变成可用
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/tool.exe"), b"x").unwrap();
        let rows = external_rows(&dir, &[exe_decl()]);
        assert!(rows[0].exists, "{}", rows[0].error);
        assert!(rows[0].error.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_tools_builds_exe_tool_with_named_params() {
        let dir = tmp_domain("extbuild");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/tool.exe"), b"x").unwrap();

        let (tools, problems) = external_tools(&dir, &[exe_decl()], "d", None);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(tools.len(), 1);
        let t = &tools[0];
        assert_eq!(t.name(), "ext.tool");
        assert!(t.description().contains("示例工具"));
        // 参数进了 schema
        let schema = t.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["target"]["type"], "string");
        // 外部程序默认 fail-closed：高风险
        assert_eq!(format!("{:?}", t.risk()).to_ascii_lowercase(), "high");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_tools_skips_broken_declarations_without_failing_others() {
        let dir = tmp_domain("extmixed");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/tool.exe"), b"x").unwrap();
        let mut ghost = exe_decl();
        ghost.path = "ghost.exe".into();

        let (tools, problems) = external_tools(&dir, &[exe_decl(), ghost], "d", None);
        assert_eq!(tools.len(), 1, "好的那条要照常产出");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("ghost.exe"), "{problems:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decl_namespace_and_action_are_derived_safely() {
        let mut d = ExternalDecl::default();
        d.path = "bin/My-Tool v2.exe".into();
        assert_eq!(d.action_name(), "my_tool_v2");
        assert_eq!(d.namespace_norm(), "ext");
        d.namespace = "acme".into();
        assert_eq!(d.display_name(), "My-Tool v2.exe", "没填 name 时用文件名");
        let mut weird = ExternalDecl::default();
        weird.path = "工具.exe".into();
        // 纯中文文件名派生不出 ASCII：退化成 tool，而不是空名
        assert_eq!(weird.action_name(), "tool");
    }
}
