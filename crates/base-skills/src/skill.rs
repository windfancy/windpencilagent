//! 技能包：SKILL.md 元信息、检索索引与被加载的技能实体。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use kern_core::{Component, ComponentKind, ComponentStatus};

/// 技能包解析错误。
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("SKILL.md not found under {0}")]
    MissingManifest(PathBuf),
    #[error("manifest `{0}` is not utf-8")]
    NotUtf8(PathBuf),
    #[error("frontmatter parse failed in {0}: {1}")]
    Frontmatter(PathBuf, String),
    #[error("missing field `{0}` in {1}")]
    MissingField(String, PathBuf),
}

/// 技能清单（SKILL.md 的 frontmatter + 正文）。
#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub version: String,
    /// 触发/适用场景描述（Claude 兼容格式下取 `description`）
    pub trigger: String,
    pub deps: Vec<String>,
    /// 版本 preflight（R3-5）：要求的最低宿主版本（None=不校验）
    pub min_app_version: Option<String>,
    /// SKILL.md 的正文导入部分
    pub body: String,
    pub path: PathBuf,
}

/// 技能包（保留目录内的附属资源路径）。
#[derive(Debug, Clone)]
pub struct Skill {
    pub manifest: SkillManifest,
    /// 附属资源（references / assets / procedures）
    pub assets: Vec<PathBuf>,
}

impl Component for Skill {
    fn id(&self) -> &str {
        &self.manifest.name
    }
    fn kind(&self) -> ComponentKind {
        ComponentKind::Skill
    }
    fn version(&self) -> &str {
        &self.manifest.version
    }
    fn description(&self) -> String {
        self.manifest.trigger.clone()
    }
    fn status(&self) -> ComponentStatus {
        ComponentStatus::Loaded
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// 解析一个技能目录（其中须有 SKILL.md）。
///
/// 支持极简 frontmatter：`---\nname: ...\nversion: ...\ntrigger: ...\ndeps: [...]\n---\nbody`
pub fn load_skill(dir: impl AsRef<Path>) -> Result<Skill, SkillError> {
    let dir = dir.as_ref();
    let manifest_path = dir.join("SKILL.md");
    if !manifest_path.exists() {
        return Err(SkillError::MissingManifest(dir.to_path_buf()));
    }
    let raw = std::fs::read_to_string(&manifest_path).map_err(|_| SkillError::NotUtf8(manifest_path.clone()))?;

    let (meta, body) = split_frontmatter(&raw)
        .ok_or_else(|| SkillError::Frontmatter(dir.to_path_buf(), "no frontmatter".into()))?;

    let mut map = parse_frontmatter(meta)
        .ok_or_else(|| SkillError::Frontmatter(dir.to_path_buf(), "syntax error".into()))?;

    let name = map
        .remove("name")
        .ok_or_else(|| SkillError::MissingField("name".into(), manifest_path.clone()))?;
    let version = map.remove("version").unwrap_or_else(|| "0.0.0".to_string());
    // R3-4 Claude 兼容：frontmatter 用 `description` 描述适用场景；本地格式用 `trigger`。
    // 两者皆可，`trigger` 优先。
    let trigger = match map.remove("trigger") {
        t if t.as_deref().map(str::trim).map(str::is_empty) == Some(false) => t.unwrap(),
        _ => map.remove("description").unwrap_or_default(),
    };
    let deps = map
        .remove("deps")
        .and_then(|d| {
            let s = d.trim();
            if s.is_empty() {
                return None;
            }
            Some(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        })
        .unwrap_or_default();
    let min_app_version = map.remove("min_app_version").filter(|s| !s.trim().is_empty());

    // 收集附属资源
    let mut assets = Vec::new();
    for sub in ["references", "assets", "procedures"] {
        let p = dir.join(sub);
        if p.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                for e in entries.flatten() {
                    assets.push(e.path());
                }
            }
        }
    }

    Ok(Skill {
        manifest: SkillManifest {
            name,
            version,
            trigger,
            deps,
            min_app_version,
            body: body.to_string(),
            path: manifest_path,
        },
        assets,
    })
}

fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let s = trimmed.trim_start_matches('\n').trim_start();
    if !s.starts_with("---") {
        return None;
    }
    let rest = &s[3..];
    let end = rest.find("---")?;
    let meta = &rest[..end];
    let body = &rest[end + 3..];
    Some((meta, body.trim_start_matches('\n')))
}

fn parse_frontmatter(meta: &str) -> Option<HashMap<String, String>> {
    let mut m = HashMap::new();
    for line in meta.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            m.insert(
                k.trim().to_string(),
                v.trim().trim_matches('"').trim_matches('\'').to_string(),
            );
        } else {
            return None;
        }
    }
    Some(m)
}

/// 技能目录索引：扫描目录，返回全部技能及名称到实体的映射。
#[derive(Default)]
pub struct SkillIndex {
    pub skills: Vec<Skill>,
    by_name: HashMap<String, usize>,
}

impl SkillIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 扫描一个或多级技能目录。
    pub fn scan(&mut self, root: impl AsRef<Path>) -> Result<usize, SkillError> {
        let root = root.as_ref();
        let mut loaded = 0usize;
        load_from_dir(root, &mut loaded, self)?;
        Ok(loaded)
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name).map(|i| &self.skills[*i])
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// 递归收集 SKILL.md 所在目录并加载（嵌套技能库支持）。
fn load_from_dir(dir: &Path, loaded: &mut usize, index: &mut SkillIndex) -> Result<(), SkillError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).map_err(|_| SkillError::MissingManifest(dir.to_path_buf()))?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && !p.join("SKILL.md").exists() {
            // 仅当该子目录本身不含 manifest 时递归
            load_from_dir(&p, loaded, index)?;
        }
    }
    if dir.join("SKILL.md").exists() {
        if let Ok(skill) = load_skill(dir) {
            let name = skill.manifest.name.clone();
            *loaded += 1;
            let idx = index.skills.len();
            index.skills.push(skill);
            index.by_name.insert(name, idx);
        }
    }
    Ok(())
}

/// 内存示例技能，便于无文件系统时自测。
#[allow(dead_code)]
pub fn demo_skill() -> Skill {
    Skill {
        manifest: SkillManifest {
            name: "demo-skill".into(),
            version: "1.0.0".into(),
            trigger: "demo".into(),
            deps: vec![],
            min_app_version: None,
            body: "# 演示技能\n- 仅供测试链路".into(),
            path: PathBuf::new(),
        },
        assets: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parse() {
        let raw = "---\nname: demo\nversion: 2.0\ntrigger: t1\ndeps: a, b\n---\nbody content";
        let skill_body = skill_body_from(raw);
        assert_eq!(skill_body, "body content");
    }

    fn skill_body_from(raw: &str) -> String {
        let (_, body) = split_frontmatter(raw).unwrap();
        body.to_string()
    }
}
