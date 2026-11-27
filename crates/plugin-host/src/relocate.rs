//! 换组的**文件侧**：把插件目录从旧组搬到新组。
//!
//! 注册表说"条目在组 B"，磁盘上却在 `plugins/A/<id>/` —— 这种不一致会一路坑下去：
//! 扫描按目录判定归属（`PluginManifest.group` 来自目录名），`Loader::ensure` 拿
//! 扫描到的那份清单去 `dlopen`，于是"账上在 B、实际从 A 加载"。所以换组必须搬目录。
//!
//! 搬不动是常态：dll 被 RunDLL/杀软/上一次构建的 rustc 持有句柄时，Windows 会
//! 直接 `os error 32`（SHARING_VIOLATION）。这种情况不能回滚注册表——用户的意图
//! 是对的，只是时机不对。于是把这次搬迁记进 `.pending-moves.json`，下次启动
//! （那时占用它的进程大概率没了）自动重试。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::scan::ScanReport;

/// 挂起的搬迁清单。放插件根目录，点开头 —— 扫描不会把它当组。
pub const PENDING_FILE: &str = ".pending-moves.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingMove {
    /// 插件 id（用于在 `problems` 里说清是谁）。
    id: String,
    /// 当前所在目录，相对插件根目录（如 `file/x`）。
    from: String,
    /// 目标组 id（目录名）。
    to_group: String,
}

/// 搬迁结果。
#[derive(Debug, Clone)]
pub enum Relocate {
    /// 同组，本来就不用动。
    Unchanged,
    /// 已经搬好。
    Moved(PathBuf),
    /// 目录被占用，已登记待下次启动重试。
    Deferred(String),
    /// 这个插件在磁盘上找不到（可能还没装）。
    NotFound,
}

impl Relocate {
    /// 给 UI toast 用的一句话。
    pub fn note(&self) -> String {
        match self {
            Relocate::Unchanged => String::new(),
            Relocate::Moved(p) => format!("已移到 {}", p.display()),
            Relocate::Deferred(why) => format!("文件被占用，已登记下次启动再移（{why}）"),
            Relocate::NotFound => "磁盘上没找到插件目录，只改了账".to_string(),
        }
    }
}

fn pending_path(root: &Path) -> PathBuf {
    root.join(PENDING_FILE)
}

fn load_pending(root: &Path) -> Vec<PendingMove> {
    std::fs::read_to_string(pending_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_pending(root: &Path, items: &[PendingMove]) {
    let p = pending_path(root);
    if items.is_empty() {
        let _ = std::fs::remove_file(p);
        return;
    }
    match serde_json::to_string_pretty(items) {
        Ok(t) => {
            if let Err(e) = std::fs::write(&p, t) {
                tracing::warn!(path = %p.display(), error = %e, "挂起搬迁清单写入失败");
            }
        }
        Err(e) => tracing::warn!(error = %e, "挂起搬迁清单序列化失败"),
    }
}

fn remember(root: &Path, mv: PendingMove) {
    let mut items = load_pending(root);
    // 同一插件只留最新意图：用户可能连点了几个组
    items.retain(|m| m.id != mv.id);
    items.push(mv);
    save_pending(root, &items);
}

/// 把 `plugins/<A>/<dir>/` 搬到 `plugins/<to_group>/<dir>/`。
///
/// `scan` 用来定位插件当前所在目录；必要时（调用方没现成扫描结果）传
/// `&crate::scan::scan(root)`。找不到就返回 [`Relocate::NotFound`]——
/// 未安装/只有账的条目不值得报错。
pub fn relocate(root: &Path, scan: &ScanReport, plugin_id: &str, to_group: &str) -> Relocate {
    let Some((from_group, member)) = scan
        .groups
        .iter()
        .find_map(|g| {
            g.members
                .iter()
                .find(|m| m.id == plugin_id)
                .map(|m| (g.manifest.id.clone(), m))
        })
    else {
        return Relocate::NotFound;
    };
    if from_group == to_group {
        return Relocate::Unchanged;
    }
    let src = member.dir.clone();
    let dir_name = match src.file_name() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return Relocate::NotFound,
    };
    let target_group_dir = root.join(to_group);
    if let Err(e) = crate::adopt::ensure_group_dir(root, to_group, to_group, "") {
        return Relocate::Deferred(format!("目标组目录建不出来：{e}"));
    }
    // 两个组目录都建好了才发现同名 —— 别覆盖，报出来让用户处理
    let dst = target_group_dir.join(&dir_name);
    if dst.exists() {
        return Relocate::Deferred(format!("{} 已存在，未覆盖", dst.display()));
    }

    match std::fs::rename(&src, &dst) {
        Ok(()) => {
            // 成功的搬迁要把残留的挂起记录清掉，否则下次启动会白跑一趟
            let mut items = load_pending(root);
            let before = items.len();
            items.retain(|m| m.id != plugin_id);
            if items.len() != before {
                save_pending(root, &items);
            }
            Relocate::Moved(dst)
        }
        Err(e) => {
            let why = match e.raw_os_error() {
                // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION
                Some(32) | Some(33) => format!("被占用，{e}"),
                _ => e.to_string(),
            };
            remember(
                root,
                PendingMove {
                    id: plugin_id.to_string(),
                    from: format!("{from_group}/{dir_name}"),
                    to_group: to_group.to_string(),
                },
            );
            Relocate::Deferred(why)
        }
    }
}

/// 重试上次没搬成的目录。返回 `(成功数, 问题列表)`。
///
/// 在 [`crate::bootstrap`] 里、扫描之前调用：搬完刚好能被这次扫描看见，
/// 注册表对账就能一次收敛。
pub fn retry_pending(root: &Path) -> (usize, Vec<String>) {
    let items = load_pending(root);
    if items.is_empty() {
        return (0, Vec::new());
    }
    let mut done = 0;
    let mut problems = Vec::new();
    let mut left: Vec<PendingMove> = Vec::new();

    for mv in items {
        let src = root.join(&mv.from);
        if !src.is_dir() {
            // 源没了：要么上次其实搬成功了（记账残留），要么用户手工删了。
            // 两种情况都不该无限重试。
            continue;
        }
        let Some(dir_name) = src.file_name().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        if let Err(e) = crate::adopt::ensure_group_dir(root, &mv.to_group, &mv.to_group, "") {
            problems.push(format!("{} 的目标组目录建不出来：{e}", mv.id));
            left.push(mv);
            continue;
        }
        let dst = root.join(&mv.to_group).join(&dir_name);
        if dst.exists() {
            problems.push(format!(
                "{}：{} 已存在，未覆盖（请手工处理后再删 .pending-moves.json）",
                mv.id,
                dst.display()
            ));
            continue;
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => {
                done += 1;
                tracing::info!(plugin = %mv.id, to = %dst.display(), "补做上次挂起的换组搬迁");
            }
            Err(e) => {
                problems.push(format!("{} 仍被占用：{e}", mv.id));
                left.push(mv);
            }
        }
    }
    save_pending(root, &left);
    (done, problems)
}

/// 当前挂起的搬迁（UI 展示"有 N 个换组等下次启动生效"）。
pub fn pending_count(root: &Path) -> usize {
    load_pending(root).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wbp_reloc_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个「组 a 里有插件 p」的最小目录结构。
    fn seed(root: &Path) {
        std::fs::create_dir_all(root.join("a/p")).unwrap();
        std::fs::write(
            root.join("a/PnC.toml"),
            "[group]\nid = \"a\"\nplugins = []\n",
        )
        .unwrap();
        std::fs::write(
            root.join("a/p/PnC.toml"),
            "[plugin]\nid = \"p\"\nrisk = \"low\"\n",
        )
        .unwrap();
        std::fs::write(root.join("a/p/p.dll"), b"x").unwrap();
    }

    #[test]
    fn relocate_moves_directory_and_clears_pending() {
        let root = tmp("move");
        seed(&root);
        let scan = crate::scan::scan(&root);
        match relocate(&root, &scan, "p", "b") {
            Relocate::Moved(p) => {
                assert!(p.ends_with("b/p"), "{}", p.display());
            }
            other => panic!("应搬成功：{other:?}"),
        }
        assert!(!root.join("a/p").exists(), "旧目录应已不在");
        assert!(root.join("b/p/p.dll").is_file(), "dll 要跟着走");
        assert!(root.join("b/PnC.toml").is_file(), "目标组目录应自动建好");
        assert_eq!(pending_count(&root), 0);

        // 扫描能按新目录认出归属
        let scan2 = crate::scan::scan(&root);
        let g = scan2.group("b").expect("组 b 应可扫描");
        assert_eq!(g.members[0].id, "p");
        assert_eq!(g.members[0].group, "b");
    }

    #[test]
    fn same_group_is_noop() {
        let root = tmp("same");
        seed(&root);
        let scan = crate::scan::scan(&root);
        assert!(matches!(
            relocate(&root, &scan, "p", "a"),
            Relocate::Unchanged
        ));
    }

    #[test]
    fn unknown_plugin_is_not_found() {
        let root = tmp("unknown");
        seed(&root);
        let scan = crate::scan::scan(&root);
        assert!(matches!(
            relocate(&root, &scan, "ghost", "b"),
            Relocate::NotFound
        ));
    }

    #[test]
    fn retry_pending_finishes_what_was_blocked() {
        let root = tmp("retry");
        seed(&root);
        // 手工造一条"上次没搬成"的记录（真实场景是 os error 32）
        std::fs::write(
            pending_path(&root),
            r#"[{"id":"p","from":"a/p","to_group":"c"}]"#,
        )
        .unwrap();
        assert_eq!(pending_count(&root), 1);

        let (done, problems) = retry_pending(&root);
        assert_eq!(done, 1, "{problems:?}");
        assert!(problems.is_empty(), "{problems:?}");
        assert!(root.join("c/p/p.dll").is_file());
        assert_eq!(pending_count(&root), 0, "成功后应清空记录");

        // 再跑一次是空操作
        let (again, _) = retry_pending(&root);
        assert_eq!(again, 0);
    }

    #[test]
    fn retry_drops_record_when_source_is_gone() {
        let root = tmp("gone");
        seed(&root);
        std::fs::write(
            pending_path(&root),
            r#"[{"id":"p","from":"a/nope","to_group":"c"}]"#,
        )
        .unwrap();
        let (done, problems) = retry_pending(&root);
        assert_eq!(done, 0);
        assert!(problems.is_empty(), "源没了不该报错，静默丢弃");
        assert_eq!(pending_count(&root), 0);
    }
}
