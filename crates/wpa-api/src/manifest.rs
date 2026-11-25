//! 清单模型：`tree.json`（整棵树）与群组清单，含版本 preflight 与依赖拓扑校验。
//!
//! 装配是**清单驱动**的：清单是唯一真相，不做目录轮询/文件监测。
//! 任何非法清单都在装配前 fail-closed 拒绝，绝不"带着半截树启动"。

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::version::SemVer;

/// 四类群组。
///
/// 用户定义：
/// - `Knowledge`：业务知识类插件，解决专业知识存储、调取、学习，为智能体提供业务规范与约束；
/// - `Tools`：原子级工具类插件；
/// - `Logic`：业务决策、业务流程类插件，编排装配知识与工具；
/// - `Skills`：分领域的工作技能。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupKind {
    Knowledge,
    Tools,
    Logic,
    Skills,
}

impl GroupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Tools => "tools",
            Self::Logic => "logic",
            Self::Skills => "skills",
        }
    }
}

impl fmt::Display for GroupKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 风险档位（与基座执行链一一对应；`High`/`Critical` 必进人工审批）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskTier {
    #[default]
    None,
    Low,
    Medium,
    High,
    Critical,
}

impl RiskTier {
    /// 是否需要人工审批（fail-closed：只有明确的 None/Low/Medium 免审）。
    pub fn needs_approval(self) -> bool {
        matches!(self, Self::High | Self::Critical)
    }
}

/// 单个群组的自描述清单（由群组自身提供，基座据此校验与审计）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupManifest {
    /// 群组唯一 id，建议带命名空间：`tools.scan`、`knowledge.vulndb`。
    pub id: String,
    /// 展示名。
    pub name: String,
    pub version: SemVer,
    pub kind: GroupKind,
    /// 依赖的其它群组 id（拓扑启停依据）。
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 能力标签（如 `scan.web`）：供上下文收敛与 UI 展示，不含实现细节。
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 该群组适用的业务阶段（与 `Planner::phases()` 的 id 对齐；空 = 全阶段可用）。
    #[serde(default)]
    pub phases: Vec<String>,
    /// 本群组要求的最低基座版本（版本闸；`None` = 不限制）。
    #[serde(default)]
    pub min_base_version: Option<SemVer>,
    /// 群组内最高风险档位（审计展示；真实判定仍走每次调用的参数级风险）。
    #[serde(default)]
    pub risk: RiskTier,
    #[serde(default)]
    pub description: String,
}

impl GroupManifest {
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: &str, kind: GroupKind) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version: SemVer::parse(version).unwrap_or(SemVer { major: 0, minor: 0, patch: 0, pre: None }),
            kind,
            depends_on: vec![],
            capabilities: vec![],
            phases: vec![],
            min_base_version: None,
            risk: RiskTier::default(),
            description: String::new(),
        }
    }

    pub fn with_depends(mut self, deps: &[&str]) -> Self {
        self.depends_on = deps.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_capabilities(mut self, caps: &[&str]) -> Self {
        self.capabilities = caps.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_phases(mut self, phases: &[&str]) -> Self {
        self.phases = phases.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_min_version(mut self, v: &str) -> Self {
        self.min_base_version = SemVer::parse(v).ok();
        self
    }

    /// 版本闸：本群组要求的最低基座版本是否满足。
    pub fn version_ok(&self, windpencilagent: &SemVer) -> bool {
        match &self.min_base_version {
            None => true,
            Some(req) => req <= windpencilagent,
        }
    }
}

/// 清单中的群组条目（声明"有哪些群组、从哪来、是否启用"）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRef {
    pub id: String,
    pub kind: GroupKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 插件来源：内置实现名（如 `builtin:scan-tools`）或相对路径。
    /// **显式声明**，不做目录扫描。
    #[serde(default)]
    pub plugin: String,
    /// 声明式群组目录（相对树包根；与 `plugin` 二选一，`plugin` 为空时用它）。
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub version: Option<SemVer>,
    #[serde(default)]
    pub description: String,
}

impl GroupRef {
    /// 群组内容目录/实现名（优先 `path`，其次 `plugin`）。
    pub fn source(&self) -> &str {
        match &self.path {
            Some(p) if !p.trim().is_empty() => p,
            _ => &self.plugin,
        }
    }
}

fn default_true() -> bool {
    true
}

/// 整棵树：`tree.toml`（新）/ `tree.json`（兼容）的映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeManifest {
    /// 树包 id（目录名；`None` 时从文件名推断）。
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub version: SemVer,
    /// 本树要求的最低基座版本（版本 preflight，fail-closed）。
    pub min_base_version: SemVer,
    #[serde(default)]
    pub description: String,
    /// 群组表：`[[group]]`（树包惯用单数）与 `groups` 均可。
    #[serde(default, alias = "group")]
    pub groups: Vec<GroupRef>,
}

impl TreeManifest {
    /// 从 JSON 文本解析（旧格式，向后兼容）。
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| format!("tree.json 解析失败：{e}"))
    }

    /// 从 TOML 文本解析（`tree.toml`，声明式树包格式）。
    pub fn from_toml(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("tree.toml 解析失败：{e}"))
    }

    /// 按扩展名自动选择解析器（`.toml` / 其它按 JSON）。
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取清单失败 {}：{e}", path.display()))?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "toml" {
            Self::from_toml(&text)
        } else {
            Self::from_json(&text)
        }
    }

    /// 树包 id（显式声明优先，其次目录名）。
    pub fn tree_id(&self) -> String {
        if let Some(id) = &self.id {
            if !id.trim().is_empty() {
                return id.clone();
            }
        }
        self.name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect::<String>()
            .to_lowercase()
    }

    /// 只保留启用项，按依赖拓扑排序后返回装配顺序。
    pub fn enabled_groups(&self) -> Vec<&GroupRef> {
        let order = self.topological_order().unwrap_or_default();
        let mut by_id: HashMap<&str, &GroupRef> =
            self.groups.iter().map(|g| (g.id.as_str(), g)).collect();
        order
            .iter()
            .filter_map(|id| by_id.remove(id.as_str()))
            .filter(|g| g.enabled)
            .collect()
    }

    /// 全量校验：版本 preflight、id 唯一、依赖存在、无环。
    pub fn validate(&self, base_version: &SemVer) -> ValidationReport {
        let mut report = ValidationReport::default();

        if self.min_base_version > *base_version {
            report.errors.push(format!(
                "版本不满足：本树要求 windpencilagent >= {}，当前 {}",
                self.min_base_version, base_version
            ));
        }

        let mut seen: HashSet<&str> = HashSet::new();
        for g in &self.groups {
            if g.id.trim().is_empty() {
                report.errors.push("存在空群组 id".into());
                continue;
            }
            if !seen.insert(g.id.as_str()) {
                report.errors.push(format!("群组 id 重复：{}", g.id));
            }
            if g.source().trim().is_empty() {
                report.errors.push(format!("群组 {} 未声明 plugin 或 path 来源", g.id));
            }
        }

        for g in &self.groups {
            for dep in &g.depends_on {
                if !seen.contains(dep.as_str()) {
                    report.errors.push(format!("群组 {} 依赖了不存在的群组：{dep}", g.id));
                }
                if dep == &g.id {
                    report.errors.push(format!("群组 {} 依赖自身", g.id));
                }
            }
        }

        match self.topological_order() {
            Ok(order) => report.order = order,
            Err(cycle) => report.errors.push(format!("群组依赖成环：{}", cycle.join(" → "))),
        }

        // 未启用但被启用的群组依赖 → 警告（装配后该依赖缺失）
        let enabled: HashSet<&str> =
            self.groups.iter().filter(|g| g.enabled).map(|g| g.id.as_str()).collect();
        for g in self.groups.iter().filter(|g| g.enabled) {
            for dep in &g.depends_on {
                if !enabled.contains(dep.as_str()) {
                    report.warnings.push(format!(
                        "群组 {} 已启用，但依赖 {} 未启用",
                        g.id, dep
                    ));
                }
            }
        }

        report
    }

    /// Kahn 拓扑排序；成环时返回环上节点（用于错误提示）。
    ///
    /// 稳定性：同层就绪节点按**清单声明顺序**入队，保证装配顺序可复现且符合人的预期
    /// （先声明的组先装配）。
    fn topological_order(&self) -> Result<Vec<String>, Vec<String>> {
        let n = self.groups.len();
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (i, g) in self.groups.iter().enumerate() {
            index.entry(g.id.as_str()).or_insert(i); // 重复 id 取首个（由 validate 单独报错）
        }
        let mut indeg = vec![0usize; n];
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, g) in self.groups.iter().enumerate() {
            for dep in &g.depends_on {
                if let Some(&j) = index.get(dep.as_str()) {
                    if j == i {
                        continue; // 自依赖由 validate 报错
                    }
                    edges[j].push(i);
                    indeg[i] += 1;
                }
            }
        }
        let mut ready: Vec<usize> = (0..n).filter(|i| indeg[*i] == 0).collect();
        ready.sort_unstable();
        let mut order = Vec::with_capacity(n);
        while !ready.is_empty() {
            let i = ready.remove(0);
            order.push(self.groups[i].id.clone());
            let mut unlocked: Vec<usize> = Vec::new();
            for &k in &edges[i] {
                if indeg[k] > 0 {
                    indeg[k] -= 1;
                }
                if indeg[k] == 0 {
                    unlocked.push(k);
                }
            }
            unlocked.sort_unstable();
            ready.extend(unlocked);
            ready.sort_unstable();
        }
        if order.len() == n {
            Ok(order)
        } else {
            Err((0..n)
                .filter(|i| indeg[*i] > 0)
                .map(|i| self.groups[i].id.clone())
                .collect())
        }
    }
}

/// 校验结果。
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// 拓扑序（仅在校验通过时有意义）。
    pub order: Vec<String>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> SemVer {
        SemVer::parse(s).unwrap()
    }

    const OK_TREE: &str = r#"{
      "name": "demo-tree",
      "version": "1.0.0",
      "min_base_version": "0.1.0",
      "groups": [
        { "id": "knowledge.kb", "kind": "knowledge", "plugin": "builtin:kb" },
        { "id": "tools.scan",  "kind": "tools",  "plugin": "builtin:scan", "depends_on": ["knowledge.kb"] },
        { "id": "logic.plan",  "kind": "logic",  "plugin": "builtin:plan", "depends_on": ["tools.scan"] },
        { "id": "skills.web",  "kind": "skills", "plugin": "builtin:skill", "enabled": false }
      ]
    }"#;

    #[test]
    fn parses_and_validates() {
        let t = TreeManifest::from_json(OK_TREE).expect("应解析成功");
        let r = t.validate(&v("0.1.0"));
        assert!(r.is_ok(), "错误：{:?}", r.errors);
        assert_eq!(r.order, vec!["knowledge.kb", "tools.scan", "logic.plan", "skills.web"]);
        // 只装配启用组，且保持拓扑序
        let enabled: Vec<&str> = t.enabled_groups().iter().map(|g| g.id.as_str()).collect();
        assert_eq!(enabled, vec!["knowledge.kb", "tools.scan", "logic.plan"]);
    }

    #[test]
    fn rejects_old_windpencilagent() {
        let t = TreeManifest::from_json(OK_TREE).unwrap();
        let r = t.validate(&v("0.0.9"));
        assert!(!r.is_ok());
        assert!(r.errors.iter().any(|e| e.contains("版本不满足")));
    }

    #[test]
    fn rejects_missing_dependency_and_cycles() {
        let missing = r#"{"name":"t","version":"1.0.0","min_base_version":"0.1.0",
            "groups":[{"id":"a","kind":"tools","plugin":"x","depends_on":["nope"]}]}"#;
        let t = TreeManifest::from_json(missing).unwrap();
        assert!(t.validate(&v("0.1.0")).errors.iter().any(|e| e.contains("不存在")));

        let cyc = r#"{"name":"t","version":"1.0.0","min_base_version":"0.1.0",
            "groups":[
              {"id":"a","kind":"tools","plugin":"x","depends_on":["b"]},
              {"id":"b","kind":"logic","plugin":"y","depends_on":["a"]}]}"#;
        let t = TreeManifest::from_json(cyc).unwrap();
        let r = t.validate(&v("0.1.0"));
        assert!(!r.is_ok());
        assert!(r.errors.iter().any(|e| e.contains("成环")));
    }

    #[test]
    fn rejects_bad_json_and_duplicate_ids() {
        assert!(TreeManifest::from_json("{ not json }").is_err());
        let dup = r#"{"name":"t","version":"1.0.0","min_base_version":"0.1.0",
            "groups":[
              {"id":"a","kind":"tools","plugin":"x"},
              {"id":"a","kind":"logic","plugin":"y"}]}"#;
        let t = TreeManifest::from_json(dup).unwrap();
        assert!(t.validate(&v("0.1.0")).errors.iter().any(|e| e.contains("重复")));
    }

    #[test]
    fn risk_tier_gate() {
        assert!(!RiskTier::Medium.needs_approval());
        assert!(RiskTier::High.needs_approval());
        assert!(RiskTier::Critical.needs_approval());
    }

    const TREE_TOML: &str = r#"
id = "pentest-tree"
name = "网络安全渗透智能树"
version = "1.0.0"
min_base_version = "0.1.0"
description = "授权渗透测试"

[[group]]
id = "knowledge.vulndb"
kind = "knowledge"
path = "knowledge/vulndb"

[[group]]
id = "tools.scan"
kind = "tools"
path = "tools/scan"
depends_on = ["knowledge.vulndb"]
"#;

    #[test]
    fn parses_toml_tree_and_path_source() {
        let t = TreeManifest::from_toml(TREE_TOML).expect("TOML 应解析成功");
        assert_eq!(t.tree_id(), "pentest-tree");
        let r = t.validate(&v("0.1.0"));
        assert!(r.is_ok(), "{:?}", r.errors);
        assert_eq!(r.order, vec!["knowledge.vulndb", "tools.scan"]);
        assert_eq!(t.groups[1].source(), "tools/scan");
    }

    #[test]
    fn rejects_group_without_source() {
        let bad = r#"{"name":"t","version":"1.0.0","min_base_version":"0.1.0",
            "groups":[{"id":"a","kind":"tools"}]}"#;
        let t = TreeManifest::from_json(bad).unwrap();
        let r = t.validate(&v("0.1.0"));
        assert!(r.errors.iter().any(|e| e.contains("未声明 plugin 或 path")));
    }

    #[test]
    fn group_manifest_version_gate() {
        let mut g = GroupManifest::new("tools.scan", "扫描", "1.0.0", GroupKind::Tools);
        assert!(g.version_ok(&v("0.0.1")), "未声明最低版本应恒通过");
        g = g.with_min_version("0.3.0");
        assert!(!g.version_ok(&v("0.2.9")));
        assert!(g.version_ok(&v("0.3.0")));
    }
}
