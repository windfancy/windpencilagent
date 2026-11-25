//! 目录扫描与依赖拓扑。
//!
//! 扫描规则（照搬构想）：遍历 `plugins/` 一级目录，有 `PnC.toml` 的是组；
//! 组目录下有 `PnC.toml` 的子目录是成员插件；缺清单的目录跳过（避免误加载公共库）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::manifest::{load_declarative, load_group, load_plugin, GroupManifest, PluginManifest};
use crate::manifest::ManifestError;

/// 一个组及其成员。
#[derive(Debug, Clone)]
pub struct GroupEntry {
    pub manifest: GroupManifest,
    pub members: Vec<PluginManifest>,
}

/// 扫描结果。
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub groups: Vec<GroupEntry>,
    /// 非致命问题（坏清单、缺 dll、跨组引用找不到）。
    pub problems: Vec<String>,
}

impl ScanReport {
    pub fn group(&self, id: &str) -> Option<&GroupEntry> {
        self.groups.iter().find(|g| g.manifest.id == id)
    }

    /// 全局插件索引（插件名 → (组名, 清单)）。
    pub fn plugin_index(&self) -> BTreeMap<String, (String, PluginManifest)> {
        let mut m = BTreeMap::new();
        for g in &self.groups {
            for p in &g.members {
                m.insert(p.id.clone(), (g.manifest.id.clone(), p.clone()));
            }
        }
        m
    }

    /// 引用了但找不到的插件。
    pub fn missing_members(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let index = self.plugin_index();
        for g in &self.groups {
            for want in &g.manifest.plugins {
                if !index.contains_key(want) {
                    out.push((g.manifest.id.clone(), want.clone()));
                }
            }
        }
        out
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.starts_with('_') || name == "target" {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out
}

/// 扫描插件根目录。
pub fn scan(root: &Path) -> ScanReport {
    let mut report = ScanReport::default();
    if !root.is_dir() {
        report
            .problems
            .push(format!("插件目录不存在：{}", root.display()));
        return report;
    }

    // 第一遍：识别组 + 收集所有插件（不论 plugins[] 怎么写）
    struct Raw {
        manifest: GroupManifest,
        local: Vec<PluginManifest>,
    }
    let mut raws: Vec<Raw> = Vec::new();
    let mut all: BTreeMap<String, (String, PluginManifest)> = BTreeMap::new();

    for gdir in subdirs(root) {
        if !gdir.join("PnC.toml").is_file() {
            continue;
        }
        let dir_name = gdir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let manifest = match load_group(&gdir) {
            Ok(m) => m,
            Err(e) => {
                report.problems.push(format!("组清单不可用：{e}"));
                continue;
            }
        };
        if manifest.id != dir_name {
            report.problems.push(format!(
                "组目录名 `{dir_name}` 与清单 id `{}` 不一致（以清单为准）",
                manifest.id
            ));
        }
        let mut local = Vec::new();
        for pdir in subdirs(&gdir) {
            if !pdir.join("PnC.toml").is_file() {
                // 缺清单的子目录一律跳过：可能是共享库或素材目录
                continue;
            }
            // 先按 dll 插件解析；清单不是 [plugin] 段时试声明式（[[tool]] + exec）
            let loaded = match load_plugin(&pdir, &dir_name) {
                Ok(m) => Ok(m),
                Err(e) => match &e {
                    ManifestError::UnknownKind(_) => load_declarative(&pdir, &dir_name),
                    _ => Err(e),
                },
            };
            match loaded {
                Ok(p) => {
                    if let Some(prev) = all.insert(p.id.clone(), (manifest.id.clone(), p.clone())) {
                        report.problems.push(format!(
                            "插件 `{}` 重复：同时出现在组 `{}` 与 `{}`（保留后者）",
                            p.id, prev.0, manifest.id
                        ));
                    }
                    if !p.has_binary() {
                        report.problems.push(format!(
                            "插件 `{}` 缺可执行文件：{}（未编译？）",
                            p.id,
                            p.dll_path().display()
                        ));
                    }
                    local.push(p);
                }
                Err(e) => report.problems.push(format!("插件清单不可用：{e}")),
            }
        }
        raws.push(Raw { manifest, local });
    }

    // 第二遍：按 plugins[] 解析成员（空 = 用本目录全部）
    for raw in raws {
        let members = if raw.manifest.plugins.is_empty() {
            raw.local.clone()
        } else {
            let mut v = Vec::new();
            for want in &raw.manifest.plugins {
                match all.get(want) {
                    Some((_, p)) => v.push(p.clone()),
                    None => report.problems.push(format!(
                        "组 `{}` 引用了不存在的插件 `{want}`",
                        raw.manifest.id
                    )),
                }
            }
            v
        };
        report.groups.push(GroupEntry {
            manifest: raw.manifest,
            members,
        });
    }

    report
        .groups
        .sort_by(|a, b| {
            a.manifest
                .priority
                .cmp(&b.manifest.priority)
                .then(a.manifest.id.cmp(&b.manifest.id))
        });
    report
}

/// 拓扑排序（组依赖先于被依赖者）。有环时返回环路径。
pub fn topo_order(requires: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>, String> {
    let mut indeg: BTreeMap<String, usize> = BTreeMap::new();
    let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in requires.keys() {
        indeg.entry(id.clone()).or_insert(0);
    }
    for (id, deps) in requires {
        for d in deps {
            if !requires.contains_key(d) {
                return Err(format!("组 `{id}` 依赖不存在的组 `{d}`"));
            }
            *indeg.entry(id.clone()).or_insert(0) += 1;
            users.entry(d.clone()).or_default().push(id.clone());
        }
    }

    let mut ready: BTreeSet<String> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut order = Vec::new();
    while let Some(id) = ready.iter().next().cloned() {
        ready.remove(&id);
        order.push(id.clone());
        if let Some(us) = users.get(&id) {
            for u in us.clone() {
                if let Some(d) = indeg.get_mut(&u) {
                    *d -= 1;
                    if *d == 0 {
                        ready.insert(u);
                    }
                }
            }
        }
    }

    if order.len() != requires.len() {
        let stuck: Vec<String> = requires
            .keys()
            .filter(|k| !order.contains(k))
            .cloned()
            .collect();
        let path = find_cycle(requires, &stuck).unwrap_or_else(|| stuck.join(" -> "));
        return Err(format!("组依赖成环：{path}"));
    }
    Ok(order)
}

/// 从剩余节点里 DFS 找一条真实环路径。
fn find_cycle(requires: &BTreeMap<String, Vec<String>>, candidates: &[String]) -> Option<String> {
    fn dfs(
        node: &str,
        requires: &BTreeMap<String, Vec<String>>,
        stack: &mut Vec<String>,
        on_path: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> Option<String> {
        if on_path.contains(node) {
            let start = stack.iter().position(|x| x == node).unwrap_or(0);
            let mut cyc = stack[start..].to_vec();
            cyc.push(node.to_string());
            return Some(cyc.join(" -> "));
        }
        if done.contains(node) {
            return None;
        }
        on_path.insert(node.to_string());
        stack.push(node.to_string());
        if let Some(deps) = requires.get(node) {
            for d in deps {
                if let Some(c) = dfs(d, requires, stack, on_path, done) {
                    return Some(c);
                }
            }
        }
        stack.pop();
        on_path.remove(node);
        done.insert(node.to_string());
        None
    }

    let mut done = BTreeSet::new();
    for c in candidates {
        let mut stack = Vec::new();
        let mut on_path = BTreeSet::new();
        if let Some(p) = dfs(c, requires, &mut stack, &mut on_path, &mut done) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wbp_scan_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_groups_and_members() {
        let root = tmp("scan");
        write(
            &root.join("web_scan/PnC.toml"),
            "[group]\nid = \"web_scan\"\npriority = 20\nplugins = []\n",
        );
        write(
            &root.join("web_scan/http_fingerprint/PnC.toml"),
            "[plugin]\nid = \"http_fingerprint\"\npermissions = [\"http\"]\n",
        );
        // 缺清单的子目录应被跳过
        std::fs::create_dir_all(root.join("web_scan/shared_lib")).unwrap();
        write(
            &root.join("asset_discovery/PnC.toml"),
            "[group]\nid = \"asset_discovery\"\npriority = 10\n",
        );
        write(
            &root.join("asset_discovery/dns_subdomain_enum/PnC.toml"),
            "[plugin]\nid = \"dns_subdomain_enum\"\n",
        );

        let r = scan(&root);
        assert_eq!(r.groups.len(), 2);
        assert_eq!(r.groups[0].manifest.id, "asset_discovery", "按 priority 排序");
        assert_eq!(r.groups[1].members.len(), 1, "缺清单的子目录不算成员");
        assert_eq!(r.plugin_index().len(), 2);
        assert!(r.missing_members().is_empty());
    }

    #[test]
    fn cross_group_reference() {
        let root = tmp("xref");
        write(
            &root.join("a/PnC.toml"),
            "[group]\nid = \"a\"\nplugins = [\"shared_tool\"]\n",
        );
        write(
            &root.join("b/PnC.toml"),
            "[group]\nid = \"b\"\nplugins = []\n",
        );
        write(
            &root.join("b/shared_tool/PnC.toml"),
            "[plugin]\nid = \"shared_tool\"\n",
        );
        let r = scan(&root);
        let a = r.group("a").unwrap();
        assert_eq!(a.members.len(), 1);
        assert_eq!(a.members[0].id, "shared_tool");
        assert_eq!(a.members[0].group, "b", "跨组引用不改归属");
        assert!(r.missing_members().is_empty());
    }

    #[test]
    fn missing_member_reported() {
        let root = tmp("missing");
        write(
            &root.join("a/PnC.toml"),
            "[group]\nid = \"a\"\nplugins = [\"ghost\"]\n",
        );
        let r = scan(&root);
        let miss = r.missing_members();
        assert_eq!(miss.len(), 1);
        assert_eq!(miss[0].1, "ghost");
    }

    #[test]
    fn topo_and_cycle() {
        let mut m = BTreeMap::new();
        m.insert("c".to_string(), vec!["b".to_string()]);
        m.insert("b".to_string(), vec!["a".to_string()]);
        m.insert("a".to_string(), vec![]);
        assert_eq!(topo_order(&m).unwrap(), vec!["a", "b", "c"]);

        let mut cyc = BTreeMap::new();
        cyc.insert("a".to_string(), vec!["b".to_string()]);
        cyc.insert("b".to_string(), vec!["c".to_string()]);
        cyc.insert("c".to_string(), vec!["a".to_string()]);
        let err = topo_order(&cyc).unwrap_err();
        assert!(err.contains("成环"), "{err}");
        assert!(err.contains("a ->"), "{err}");
        assert!(err.contains("-> a"), "{err}");
    }

    #[test]
    fn topo_missing_dep() {
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), vec!["nope".to_string()]);
        assert!(topo_order(&m).unwrap_err().contains("不存在"));
    }
}
