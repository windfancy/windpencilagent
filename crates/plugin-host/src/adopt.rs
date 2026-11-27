//! 目录自举：把「用户随手拷进来的东西」变成「扫描能看懂的组」。
//!
//! 三件事，都是为了**零手写**：
//!
//! 1. [`ensure_group_dir`]：新建组时顺手建目录 + 写 `PnC.toml`，用户不需要知道组还有目录。
//! 2. [`ensure_unassigned`]：保证 `plugins/unassigned/` 存在且带清单，作为裸 dll 的落点。
//! 3. [`adopt_loose`]：把 `plugins/` 根目录下的裸 dll / 放错层的插件目录收进 `unassigned/`。
//!
//! 第 3 步必须**真的搬文件**而不是"扫描时虚拟挂载"：`scan` 是只读的，每次启动都
//! 重新算一遍虚拟归属，结果移动组、改白名单时找不到源文件（`move_entry` 要移动 dll）。
//! 让磁盘结构 == 注册表结构，后续所有操作才有唯一事实。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::manifest::DllManifest;
use crate::probe::{probe_cached, ProbeCache};

/// 裸 dll 的落点目录名。必须等于 `wpa_engine::registry::UNASSIGNED_GROUP`
/// （那边有测试钉住一致性），且**不带下划线**——扫描会跳过 `_` 开头的目录。
pub const UNASSIGNED_DIR: &str = "unassigned";

/// 自举结果。
#[derive(Debug, Clone, Default)]
pub struct AdoptReport {
    /// 收编成功的插件 id（或目录名）。
    pub adopted: Vec<String>,
    /// 收不进来的原因（供启动告警与 UI 提示）。
    pub problems: Vec<String>,
}

impl AdoptReport {
    pub fn is_empty(&self) -> bool {
        self.adopted.is_empty() && self.problems.is_empty()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.adopted.is_empty() {
            parts.push(format!("收编 {} 个裸插件：{}", self.adopted.len(), self.adopted.join(", ")));
        }
        if !self.problems.is_empty() {
            parts.push(format!("{} 个未能收编", self.problems.len()));
        }
        if parts.is_empty() {
            "无需自举".to_string()
        } else {
            parts.join("；")
        }
    }
}

/// TOML 字符串转义（只处理值，不处理键）。
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

/// 生成组清单。
pub fn group_pnc(id: &str, name: &str, description: &str, priority: i32) -> String {
    format!(
        "# 由宿主自动生成；可手工编辑（改 name/description/priority 不会被覆盖）\n\
         [group]\n\
         id = \"{}\"\n\
         name = \"{}\"\n\
         version = \"0.1.0\"\n\
         kind = \"tools\"\n\
         priority = {}\n\
         description = \"{}\"\n\
         plugins = []\n",
        esc(id),
        esc(name),
        priority,
        esc(description)
    )
}

/// 生成插件清单（从 dll 内嵌清单反推）。
pub fn plugin_pnc(m: &DllManifest, binary: &str) -> String {
    let perms = m
        .permissions
        .iter()
        .map(|p| format!("\"{}\"", esc(p)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# 由宿主在发现裸 dll 时自动生成；可手工编辑\n\
         [plugin]\n\
         id = \"{}\"\n\
         name = \"{}\"\n\
         version = \"{}\"\n\
         risk = \"{}\"\n\
         permissions = [{}]\n\
         binary = \"{}\"\n\
         summary = \"{}\"\n",
        esc(&m.id),
        esc(&m.id),
        esc(&m.version),
        esc(&m.risk),
        perms,
        esc(binary),
        esc(&m.summary)
    )
}

/// 确保 `plugins/<id>/` 存在且带 `PnC.toml`。已存在则**不覆盖**（用户的编辑优先）。
///
/// 返回目录路径。`plugins/` 根目录不存在时一并创建。
pub fn ensure_group_dir(
    root: &Path,
    id: &str,
    name: &str,
    description: &str,
) -> Result<PathBuf, String> {
    if id.trim().is_empty() {
        return Err("组 id 为空，无法建目录".to_string());
    }
    let dir = root.join(id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建组目录失败 {}：{e}", dir.display()))?;
    let pnc = dir.join("PnC.toml");
    if !pnc.is_file() {
        std::fs::write(&pnc, group_pnc(id, name, description, 50))
            .map_err(|e| format!("写入组清单失败 {}：{e}", pnc.display()))?;
    }
    Ok(dir)
}

/// 确保 `plugins/unassigned/` 存在（裸 dll 的落点）。
pub fn ensure_unassigned(root: &Path) -> Result<PathBuf, String> {
    ensure_group_dir(root, UNASSIGNED_DIR, "未分配", "从 plugins 根目录收编的插件落在这里")
}

/// 收编 `plugins/` 根目录下的松散内容。
///
/// 认两种：
/// - **裸 dll 文件**：探测内嵌清单 → 建 `unassigned/<id>/` → 移文件 + 写清单。
/// - **放错层的插件目录**（根下直接是一个带 `[plugin]` 的目录，本该在组内）：
///   整目录移进 `unassigned/<目录名>/`。
///
/// 认不了的（非插件 dll、坏清单、被占用）只记 `problems`，不阻断启动。
pub fn adopt_loose(root: &Path, cache: &mut ProbeCache, timeout: Duration) -> AdoptReport {
    let mut rep = AdoptReport::default();
    let Ok(rd) = std::fs::read_dir(root) else {
        return rep;
    };

    // 先排序，让同一次运行的顺序稳定（测试与日志可复现）
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();

    let unassigned = match ensure_unassigned(root) {
        Ok(d) => d,
        Err(e) => {
            rep.problems.push(e);
            return rep;
        }
    };

    for path in entries {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with('.') || name.starts_with('_') || name == "target" {
            continue;
        }
        if path.is_dir() {
            adopt_misplaced_dir(&path, &unassigned, &mut rep);
            continue;
        }
        // 只认 .dll：用户可能同时拷了 .pdb/.lib/README
        let is_dll = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("dll"))
            .unwrap_or(false);
        if !is_dll {
            continue;
        }
        adopt_dll(&path, &name, &unassigned, cache, timeout, &mut rep);
    }

    rep
}

fn adopt_dll(
    dll: &Path,
    file_name: &str,
    unassigned: &Path,
    cache: &mut ProbeCache,
    timeout: Duration,
    rep: &mut AdoptReport,
) {
    let m = match probe_cached(cache, dll, timeout) {
        Ok(m) => m,
        Err(e) => {
            rep.problems.push(format!("{}：{e}", dll.display()));
            return;
        }
    };
    let target = unassigned.join(&m.id);
    let pnc = target.join("PnC.toml");
    if pnc.is_file() {
        // 同 id 已经在未分配里了。不覆盖：可能是同一插件的另一个编译版本，
        // 用户要的应该是"知道有冲突"，而不是文件被悄悄替换掉。
        rep.problems.push(format!(
            "{} 的 id `{}` 与已收编的插件重名，已跳过（如需替换请先删除 unassigned/{}/）",
            file_name, m.id, m.id
        ));
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&target) {
        rep.problems
            .push(format!("创建 {} 失败：{e}", target.display()));
        return;
    }
    let dest = target.join(file_name);
    if let Err(e) = std::fs::rename(dll, &dest) {
        rep.problems.push(format!(
            "移动 {} 失败：{e}（正在被占用？关掉占用它的程序后重启即可）",
            dll.display()
        ));
        return;
    }
    if let Err(e) = std::fs::write(&pnc, plugin_pnc(&m, file_name)) {
        rep.problems
            .push(format!("写清单失败 {}：{e}", pnc.display()));
        return;
    }
    rep.adopted.push(m.id);
}

fn adopt_misplaced_dir(dir: &Path, unassigned: &Path, rep: &mut AdoptReport) {
    // 有 [group] 段 = 正常的组目录，不动
    if crate::manifest::load_group(dir).is_ok() {
        return;
    }
    // 必须带 [plugin] 段才认；否则可能是用户自己的素材/共享库目录
    if !dir.join("PnC.toml").is_file() || crate::manifest::load_plugin(dir, "").is_err() {
        return;
    }
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let target = unassigned.join(&name);
    if target.exists() {
        rep.problems.push(format!(
            "{} 是插件目录但放错了层，且 unassigned/{name}/ 已存在，已跳过",
            dir.display()
        ));
        return;
    }
    match std::fs::rename(dir, &target) {
        Ok(()) => rep.adopted.push(name),
        Err(e) => rep.problems.push(format!(
            "移动插件目录 {} 失败：{e}（正在被占用？）",
            dir.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wbp_adopt_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ensure_group_dir_writes_manifest_once() {
        let root = tmp("ensure");
        let dir = ensure_group_dir(&root, "my_group", "我的组", "说明").unwrap();
        let pnc = dir.join("PnC.toml");
        assert!(pnc.is_file());
        let g = crate::manifest::load_group(&dir).unwrap();
        assert_eq!(g.id, "my_group");
        assert_eq!(g.name, "我的组");

        // 用户改过之后不能被覆盖
        std::fs::write(&pnc, "[group]\nid = \"my_group\"\nname = \"改名了\"\n").unwrap();
        ensure_group_dir(&root, "my_group", "我的组", "说明").unwrap();
        let g = crate::manifest::load_group(&dir).unwrap();
        assert_eq!(g.name, "改名了", "已存在的清单不该被覆盖");
    }

    #[test]
    fn unassigned_dir_is_scannable() {
        let root = tmp("unassigned");
        let d = ensure_unassigned(&root).unwrap();
        assert_eq!(d.file_name().unwrap().to_string_lossy(), UNASSIGNED_DIR);
        let g = crate::manifest::load_group(&d).unwrap();
        assert_eq!(g.id, UNASSIGNED_DIR);
        // 目录名不被扫描跳过（不带下划线）
        let rep = crate::scan::scan(&root);
        assert!(rep.group(UNASSIGNED_DIR).is_some(), "{:?}", rep.problems);
    }

    #[test]
    fn misplaced_plugin_dir_moves_into_unassigned() {
        let root = tmp("misplaced");
        let stray = root.join("loose_tool");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(
            stray.join("PnC.toml"),
            "[plugin]\nid = \"loose_tool\"\nrisk = \"low\"\n",
        )
        .unwrap();

        let mut cache = ProbeCache::default();
        let rep = adopt_loose(&root, &mut cache, Duration::from_millis(50));
        assert_eq!(rep.adopted, vec!["loose_tool".to_string()], "{:?}", rep.problems);
        assert!(!stray.exists(), "原目录应已搬走");
        assert!(root.join(UNASSIGNED_DIR).join("loose_tool/PnC.toml").is_file());
    }

    #[test]
    fn non_dll_files_are_left_alone() {
        let root = tmp("nondll");
        std::fs::write(root.join("README.md"), "hi").unwrap();
        std::fs::write(root.join("lib.pdb"), "x").unwrap();
        let mut cache = ProbeCache::default();
        let rep = adopt_loose(&root, &mut cache, Duration::from_millis(50));
        assert!(rep.adopted.is_empty());
        assert!(rep.problems.is_empty(), "{:?}", rep.problems);
        assert!(root.join("README.md").is_file());
    }
}
