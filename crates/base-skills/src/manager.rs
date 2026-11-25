//! 技能管理器：技能包的安装 / 卸载 / 启用禁用（状态持久化）。
//!
//! - 技能库目录：exe 同级 `skills/`，每个技能一个子目录（含 SKILL.md）；
//! - 禁用状态：`skills/state.json`（`{"disabled": ["name"]}`）；
//! - 安装：从源目录复制整个技能包到库目录（目录名取 SKILL.md 的 name）；
//! - 内置技能（attack-chain）：缺失时自动物化落盘并启用。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::skill::{load_skill, Skill, SkillError};

/// 管理错误。
#[derive(Debug, Error)]
pub enum SkillManagerError {
    #[error("skill error: {0}")]
    Skill(#[from] SkillError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("skill `{0}` already installed")]
    AlreadyInstalled(String),
    #[error("skill `{0}` not found")]
    NotFound(String),
    #[error("source has no SKILL.md: {0}")]
    InvalidSource(String),
    #[error("zip error: {0}")]
    Zip(String),
    #[error("skill `{0}` requires app >= {1}")]
    VersionTooLow(String, String),
}

/// 持久化的启用状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SkillState {
    #[serde(default)]
    disabled: Vec<String>,
}

/// 技能条目（管理视图）。
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub version: String,
    pub trigger: String,
    pub enabled: bool,
    pub builtin: bool,
    pub dir: PathBuf,
}

/// 技能管理器。
pub struct SkillManager {
    root: PathBuf,
    state_path: PathBuf,
    /// 宿主版本（R3-5 preflight：min_app_version > 宿主版本的技能不注入，目录保留）
    app_version: String,
}

impl SkillManager {
    /// 以 `root` 为技能库目录创建管理器（目录自动创建；内置技能自动补装）。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SkillManagerError> {
        Self::with_version(root, env!("CARGO_PKG_VERSION"))
    }

    /// 同 [`open`](Self::open)，并指定宿主版本供 min_app_version preflight。
    pub fn with_version(root: impl AsRef<Path>, app_version: &str) -> Result<Self, SkillManagerError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let mgr = Self { state_path: root.join("state.json"), root, app_version: app_version.to_string() };
        mgr.ensure_builtin()?;
        Ok(mgr)
    }

    fn load_state(&self) -> SkillState {
        std::fs::read_to_string(&self.state_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn save_state(&self, state: &SkillState) -> Result<(), SkillManagerError> {
        let raw = serde_json::to_vec_pretty(state)?;
        Ok(std::fs::write(&self.state_path, raw)?)
    }

    /// 内置技能缺失时物化落盘（幂等）。
    fn ensure_builtin(&self) -> Result<(), SkillManagerError> {
        for (dir_name, content) in crate::builtin::BUILTIN_SKILLS {
            let dir = self.root.join(dir_name);
            let manifest = dir.join("SKILL.md");
            if !manifest.exists() {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&manifest, content)?;
            }
        }
        Ok(())
    }

    /// 列出全部技能（含启用状态）。
    pub fn list(&self) -> Vec<SkillEntry> {
        let state = self.load_state();
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                let dir = e.path();
                if !dir.is_dir() {
                    continue;
                }
                if let Ok(skill) = load_skill(&dir) {
                    let dir_name = dir
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let builtin = crate::builtin::BUILTIN_SKILLS
                        .iter()
                        .any(|(n, _)| *n == dir_name);
                    out.push(SkillEntry {
                        name: skill.manifest.name.clone(),
                        version: skill.manifest.version.clone(),
                        trigger: skill.manifest.trigger.clone(),
                        enabled: !state.disabled.contains(&skill.manifest.name),
                        builtin,
                        dir,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// 从源目录安装技能包（递归复制；同名已装则报错）。
    pub fn install(&self, src: impl AsRef<Path>) -> Result<Skill, SkillManagerError> {
        let src = src.as_ref();
        if !src.join("SKILL.md").exists() {
            return Err(SkillManagerError::InvalidSource(src.display().to_string()));
        }
        let skill = load_skill(src)?;
        let dest = self.root.join(&skill.manifest.name);
        if dest.join("SKILL.md").exists() {
            return Err(SkillManagerError::AlreadyInstalled(skill.manifest.name.clone()));
        }
        copy_dir_recursive(src, &dest)?;
        // 重读目标位置，保证返回的 path/资产指向库内
        Ok(load_skill(&dest)?)
    }

    /// 卸载技能（删除目录与禁用记录；内置技能仅删除目录，下次启动会补装）。
    pub fn uninstall(&self, name: &str) -> Result<(), SkillManagerError> {
        let entry = self
            .list()
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| SkillManagerError::NotFound(name.to_string()))?;
        std::fs::remove_dir_all(&entry.dir)?;
        let mut state = self.load_state();
        state.disabled.retain(|n| n != name);
        self.save_state(&state)?;
        Ok(())
    }

    /// 启用 / 禁用（禁用即从注入上下文中移除，目录保留）。
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), SkillManagerError> {
        if !self.list().iter().any(|s| s.name == name) {
            return Err(SkillManagerError::NotFound(name.to_string()));
        }
        let mut state = self.load_state();
        if enabled {
            state.disabled.retain(|n| n != name);
        } else if !state.disabled.iter().any(|n| n == name) {
            state.disabled.push(name.to_string());
        }
        self.save_state(&state)?;
        Ok(())
    }

    /// 已启用技能的完整实体（供注入上下文）。
    ///
    /// R3-5 preflight：`min_app_version` 高于宿主版本的技能视为不兼容，跳过注入。
    pub fn enabled_skills(&self) -> Vec<Skill> {
        let state = self.load_state();
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                let dir = e.path();
                if dir.is_dir() {
                    if let Ok(skill) = load_skill(&dir) {
                        if state.disabled.contains(&skill.manifest.name) {
                            continue;
                        }
                        if let Some(min) = &skill.manifest.min_app_version {
                            if version_lt(&self.app_version, min) {
                                tracing::warn!(
                                    skill = %skill.manifest.name,
                                    need = %min,
                                    have = %self.app_version,
                                    "技能要求更高宿主版本，本轮不注入"
                                );
                                continue;
                            }
                        }
                        out.push(skill);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        out
    }

    /// 从 zip 包安装技能（R3-4，兼容 Claude 格式 SKILL.md）。
    ///
    /// 包布局：zip 根目录或其唯一顶层子目录下含 `SKILL.md`，附属资源随目录解压。
    /// 同名已装报 `AlreadyInstalled`；解压在临时目录完成后整体迁入技能库。
    pub fn install_zip(&self, zip_path: impl AsRef<Path>) -> Result<Skill, SkillManagerError> {
        let zip_path = zip_path.as_ref();
        let file = std::fs::File::open(zip_path)
            .map_err(|e| SkillManagerError::Zip(format!("{}: {e}", zip_path.display())))?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| SkillManagerError::Zip(format!("{}: {e}", zip_path.display())))?;

        // 安全检查 + 解到临时目录。防 zip-slip：拒绝绝对路径与 `..` 逃逸条目。
        let staging =
            std::env::temp_dir().join(format!("wpc-skill-zip-{}-{}", std::process::id(), timestamp_ms()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let result = (|| -> Result<Skill, SkillManagerError> {
            for i in 0..archive.len() {
                let mut entry = archive
                    .by_index(i)
                    .map_err(|e| SkillManagerError::Zip(format!("entry {i}: {e}")))?;
                let raw = entry.name().replace('\\', "/");
                if raw.starts_with('/') || raw.contains("..") {
                    return Err(SkillManagerError::Zip(format!("unsafe entry path: {raw}")));
                }
                if raw.ends_with('/') {
                    continue; // 目录条目按需创建
                }
                let dest = staging.join(&raw);
                let Some(parent) = dest.parent() else {
                    return Err(SkillManagerError::Zip(format!("bad entry path: {raw}")));
                };
                std::fs::create_dir_all(parent)?;
                let mut out = std::fs::File::create(&dest)?;
                std::io::copy(&mut entry, &mut out)?;
            }
            // 定位 SKILL.md：根目录直下，否则唯一顶层目录下。
            let root_manifest = staging.join("SKILL.md");
            let src_dir = if root_manifest.exists() {
                staging.clone()
            } else {
                let mut top_dirs: Vec<PathBuf> = std::fs::read_dir(&staging)?
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect();
                if top_dirs.len() != 1 {
                    return Err(SkillManagerError::InvalidSource(zip_path.display().to_string()));
                }
                let top = top_dirs.pop().unwrap();
                if !top.join("SKILL.md").exists() {
                    return Err(SkillManagerError::InvalidSource(zip_path.display().to_string()));
                }
                top
            };
            self.install(&src_dir)
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result
    }
}

/// 毫秒时间戳（临时目录名去重）。
fn timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 语义化版本比较（三段数字，缺段按 0）：`a < b`。
fn version_lt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|s| s.split(['-', '+']).next().unwrap_or("0").parse().unwrap_or(0))
            .collect()
    }
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..3 {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x < y;
        }
    }
    false
}

/// 递归复制目录（跟随常规文件；跳过空项）。
fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let from = e.path();
        let to = dest.join(e.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::ATTACK_CHAIN_SKILL;

    fn write_skill(dir: &Path, name: &str, trigger: &str) {
        let content = format!(
            "---\nname: {name}\nversion: 1.0\ntrigger: {trigger}\n---\n# {name}\n正文内容"
        );
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn builtin_materialized_and_enabled() {
        let tmp = std::env::temp_dir().join(format!("wpn-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = SkillManager::open(&tmp).unwrap();
        // 内置技能自动落盘并启用（attack-chain + threat-intel + nuclei-template-scan + usage-guide）
        let list = mgr.list();
        assert_eq!(list.len(), 4);
        assert!(list.iter().all(|s| s.builtin && s.enabled));
        assert!(list.iter().any(|s| s.name == "attack-chain"));
        assert!(list.iter().any(|s| s.name == "threat-intel"));
        assert!(list.iter().any(|s| s.name == "nuclei-template-scan"));
        assert!(list.iter().any(|s| s.name == "usage-guide"));
        assert!(mgr.enabled_skills().iter().any(|s| s.manifest.name == "attack-chain"));
        assert!(mgr.enabled_skills().iter().any(|s| s.manifest.name == "threat-intel"));
        assert!(mgr
            .enabled_skills()
            .iter()
            .any(|s| s.manifest.name == "usage-guide"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_toggle_uninstall() {
        let tmp = std::env::temp_dir().join(format!("wpn-skills2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // 源技能目录放在库目录之外（库内目录一律视为已安装技能）
        let src = std::env::temp_dir().join(format!("wpn-skills2-src-{}", std::process::id()));
        write_skill(&src, "port-lore", "端口扫描技巧");
        let mgr = SkillManager::open(&tmp).unwrap();

        // 安装
        let skill = mgr.install(&src).unwrap();
        assert_eq!(skill.manifest.name, "port-lore");
        // 重复安装报错
        assert!(matches!(
            mgr.install(&src),
            Err(SkillManagerError::AlreadyInstalled(_))
        ));
        // 禁用 → 注入清单消失
        mgr.set_enabled("port-lore", false).unwrap();
        assert!(!mgr.enabled_skills().iter().any(|s| s.manifest.name == "port-lore"));
        assert!(!mgr.list().iter().find(|s| s.name == "port-lore").unwrap().enabled);
        // 重新启用
        mgr.set_enabled("port-lore", true).unwrap();
        assert!(mgr.enabled_skills().iter().any(|s| s.manifest.name == "port-lore"));
        // 卸载
        mgr.uninstall("port-lore").unwrap();
        assert!(!mgr.list().iter().any(|s| s.name == "port-lore"));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&src);
        let _ = ATTACK_CHAIN_SKILL;
    }

    #[test]
    fn invalid_source_rejected() {
        let tmp = std::env::temp_dir().join(format!("wpn-skills3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = SkillManager::open(&tmp).unwrap();
        let bad = tmp.join("not-a-skill");
        std::fs::create_dir_all(&bad).unwrap();
        assert!(matches!(mgr.install(&bad), Err(SkillManagerError::InvalidSource(_))));
        assert!(matches!(
            mgr.set_enabled("ghost", true),
            Err(SkillManagerError::NotFound(_))
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 生成测试 zip（根目录含技能目录）。
    fn make_skill_zip(path: &Path, name: &str, use_claude_format: bool) {
        let file = std::fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        let manifest = if use_claude_format {
            format!(
                "---\nname: {name}\ndescription: 使用 {name} 处理目标场景\n---\n# {name}\n技能正文"
            )
        } else {
            format!("---\nname: {name}\nversion: 1.2\ntrigger: 触发场景\n---\n# {name}\n正文")
        };
        z.start_file(format!("{name}/SKILL.md"), opts).unwrap();
        std::io::Write::write_all(&mut z, manifest.as_bytes()).unwrap();
        z.start_file(format!("{name}/references/notes.md"), opts).unwrap();
        std::io::Write::write_all(&mut z, b"reference doc").unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn zip_install_claude_and_native() {
        let tmp = std::env::temp_dir().join(format!("wpc-skz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = SkillManager::open(&tmp).unwrap();

        // Claude 格式：description 映射为 trigger
        let z1 = tmp.join("a.zip");
        make_skill_zip(&z1, "claude-skill", true);
        let s = mgr.install_zip(&z1).unwrap();
        assert_eq!(s.manifest.name, "claude-skill");
        assert_eq!(s.manifest.trigger, "使用 claude-skill 处理目标场景");
        assert!(s.manifest.path.parent().unwrap().join("references").exists());

        // 本地格式 + 附属资源
        let z2 = tmp.join("b.zip");
        make_skill_zip(&z2, "native-skill", false);
        let s2 = mgr.install_zip(&z2).unwrap();
        assert_eq!(s2.manifest.version, "1.2");
        assert_eq!(s2.manifest.trigger, "触发场景");
        // 重复安装报错
        assert!(matches!(mgr.install_zip(&z2), Err(SkillManagerError::AlreadyInstalled(_))));
        assert_eq!(mgr.list().len(), 4 + 2); // 4 内置 + 2 新装
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn zip_slip_rejected() {
        let tmp = std::env::temp_dir().join(format!("wpc-skz-slip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mgr = SkillManager::open(&tmp).unwrap();
        let z = tmp.join("evil.zip");
        let file = std::fs::File::create(&z).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        zw.start_file("../escape.txt", opts).unwrap();
        std::io::Write::write_all(&mut zw, b"evil").unwrap();
        zw.finish().unwrap();
        assert!(matches!(mgr.install_zip(&z), Err(SkillManagerError::Zip(_))));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn version_preflight_blocks_old_host() {
        let tmp = std::env::temp_dir().join(format!("wpc-skz-ver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // 技能要求宿主 >= 9.9.9：用低版本管理器打开则不注入
        let dir = tmp.join("needs-new");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: future-skill\nmin_app_version: 9.9.9\ntrigger: t\n---\nbody",
        )
        .unwrap();
        let mgr = SkillManager::with_version(&tmp, "1.2.3").unwrap();
        assert!(!mgr.enabled_skills().iter().any(|s| s.manifest.name == "future-skill"));
        // 高版本宿主可注入
        let mgr2 = SkillManager::with_version(&tmp, "9.9.9").unwrap();
        assert!(mgr2.enabled_skills().iter().any(|s| s.manifest.name == "future-skill"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
