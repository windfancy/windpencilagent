//! # plugin-host —— 插件宿主
//!
//! 负责把 `plugins/` 目录变成一组可执行工具，全程遵守"安装 ≠ 加载"：
//!
//! | 阶段 | 动作 | 打开 dll |
//! |---|---|---|
//! | 扫描 | 读 `PnC.toml`、建注册表、算拓扑 | 否 |
//! | 启用组 | 记状态、校验依赖 | 否 |
//! | 首次调用 | `dlopen` + `wbp_init` + 缓存实例 | 是 |
//!
//! 组是逻辑视图：它的成员只是引用，同一个插件可被多个组引用而不复制文件。

pub mod adopt;
pub mod ctx;
pub mod load;
pub mod manifest;
pub mod probe;
pub mod relocate;
pub mod scan;
pub mod tool;

/// 工具 + 其所属组 id（组用于 UI 分组与卸载定位）。
pub type ToolWithGroup = (String, Arc<dyn ApiTool>);

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plugin_abi::HostApi;
use wpa_api::ApiTool;

pub use adopt::{adopt_loose, ensure_group_dir, ensure_unassigned, AdoptReport};
pub use ctx::CallContext;
pub use load::{CacheConfig, LoadedPlugin, Loader};
pub use manifest::{DllManifest, GroupManifest, PluginManifest};
pub use probe::ProbeCache;
pub use relocate::{relocate, retry_pending, Relocate};
pub use scan::{GroupEntry, ScanReport};
pub use tool::{max_risk, parse_risk, PluginTool};

/// 启动前自举：收编 `plugins/` 根目录的裸 dll，刷新探测缓存。
///
/// 只做两件幂等的事，可以每次启动都跑：
/// 1. `unassigned/` 目录与清单就位（不存在就建）；
/// 2. 根下裸 dll / 放错层的插件目录搬进去，并按内嵌清单生成 `PnC.toml`。
///
/// `auto_probe = false` 时整段跳过 —— 有些人只想要"完全静态"的插件目录，
/// 不愿意让宿主动任何文件（也会踩到"dll 被编译器占用"的毛病）。
pub fn bootstrap(root: &std::path::Path, auto_probe: bool) -> AdoptReport {
    if !root.is_dir() {
        return AdoptReport::default();
    }
    let mut cache = ProbeCache::load(root);
    let mut rep = if auto_probe {
        adopt::adopt_loose(root, &mut cache, probe::DEFAULT_TIMEOUT)
    } else {
        // 不探测也要保证未分配目录在：否则「移出组」找不到落点
        let mut r = AdoptReport::default();
        if let Err(e) = adopt::ensure_unassigned(root) {
            r.problems.push(e);
        }
        r
    };
    if auto_probe && !cache.is_empty() {
        cache.save(root);
    }
    // 补做上次因 dll 被占用而挂起的换组搬迁。必须在这里（扫描之前）：
    // 搬完目录本次扫描就能看见新归属，注册表对账一步收敛。
    let (moved, mut problems) = relocate::retry_pending(root);
    if moved > 0 {
        rep.adopted.push(format!("{moved} 个挂起的换组已补做"));
    }
    rep.problems.append(&mut problems);
    rep
}

/// 按插件声明的权限构造能力表（未声明的字段为 `None`）。
pub type HostFactory = Arc<dyn Fn(&[String]) -> HostApi + Send + Sync>;

/// 插件运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// 已登记，未加载
    Installed,
    /// 已加载，可执行
    Active,
    /// 熔断：拒绝执行但实例保留
    Degraded,
}

/// 单个插件状态（UI/CLI 展示）。
#[derive(Debug, Clone)]
pub struct PluginStatus {
    pub id: String,
    pub name: String,
    pub risk: String,
    pub version: String,
    pub state: PluginState,
    pub calls: u64,
    /// 最近一次错误（熔断原因）。
    pub error: String,
    pub has_binary: bool,
}

/// 组状态。
#[derive(Debug, Clone)]
pub struct GroupStatus {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub priority: i32,
    pub description: String,
    pub requires: Vec<String>,
    pub enabled: bool,
    pub members: Vec<PluginStatus>,
}

/// 插件宿主。
///
/// **不持有启用状态**：组/条目开不开由调用方（`wpa-engine` 的注册表）决定，
/// 每次 `build_tools(&wanted)` 传入要加载的条目集合。历史上这里存过一份
/// `plugins.toml` 的 `enabled`，与注册表的组开关形成两套账——UI 上关了组、
/// 这里仍以为开着，实际加载与否取决于哪份文件先写。现在只有一份账。
pub struct PluginHost {
    root: PathBuf,
    factory: HostFactory,
    report: ScanReport,
    loader: Loader,
    /// 扫描与加载过程中的非致命问题。
    problems: Vec<String>,
}

impl PluginHost {
    /// 打开插件目录（只扫描，不加载任何 dll）。
    pub fn open(root: impl Into<PathBuf>, factory: HostFactory) -> Self {
        let root = root.into();
        let report = scan::scan(&root);
        Self {
            root,
            factory,
            report,
            loader: Loader::new(CacheConfig::default()),
            problems: Vec::new(),
        }
    }

    /// 重新扫描（新增插件后调用；不影响已加载实例）。
    pub fn rescan(&mut self) {
        self.report = scan::scan(&self.root);
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn report(&self) -> &ScanReport {
        &self.report
    }

    pub fn problems(&self) -> Vec<String> {
        let mut v = self.report.problems.clone();
        v.extend(self.problems.iter().cloned());
        v
    }

    pub fn groups(&self) -> &[GroupEntry] {
        &self.report.groups
    }

    /// 组成员（按组内声明顺序）。
    pub fn members_of(&self, group: &str) -> Vec<PluginManifest> {
        self.report
            .group(group)
            .map(|g| g.members.clone())
            .unwrap_or_default()
    }

    /// 全部组的成员（去重，保持组优先序）。
    ///
    /// 不再按"已启用"过滤：加载与否由 `build_tools(&wanted)` 的 `wanted` 决定，
    /// 避免"扫描面"和"启用面"各算一遍、两边对不上。
    pub fn active_members(&self) -> Vec<(String, PluginManifest)> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for g in &self.report.groups {
            for m in &g.members {
                if seen.insert(m.id.clone()) {
                    out.push((g.manifest.id.clone(), m.clone()));
                }
            }
        }
        out
    }

    /// 扫描顺序下的组 id（`report` 已按 priority 排序）。
    pub fn group_ids_in_order(&self) -> Vec<String> {
        self.report
            .groups
            .iter()
            .map(|g| g.manifest.id.clone())
            .collect()
    }

    /// 校验"能否启用组"，返回需要连带启用的组（依赖闭包，已拓扑排序）。
    ///
    /// **不写任何状态**：组开关只存在于注册表（唯一账本），调用方拿到返回值后
    /// 写注册表即可。这里只做"目录里有没有这个组 / 依赖是否齐全 / 有没有环"。
    pub fn check_enable(&self, enabled: &BTreeSet<String>, id: &str) -> Result<Vec<String>, String> {
        if self.report.group(id).is_none() {
            return Err(format!("组 `{id}` 不存在（先运行一次扫描/检查目录名）"));
        }
        // 递归收集依赖闭包 + 环检测
        let requires = self.requires_map();
        let mut closure = BTreeSet::new();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if !closure.insert(cur.clone()) {
                continue;
            }
            for d in requires.get(&cur).cloned().unwrap_or_default() {
                if self.report.group(&d).is_none() {
                    return Err(format!("组 `{cur}` 依赖的 `{d}` 未安装"));
                }
                stack.push(d);
            }
        }
        // 只对闭包做拓扑校验（全图有环但与本组无关时不阻塞）
        let sub: BTreeMap<String, Vec<String>> = closure
            .iter()
            .map(|k| (k.clone(), requires.get(k).cloned().unwrap_or_default()))
            .collect();
        let order = scan::topo_order(&sub)?;
        // 已经启用的不必再报
        Ok(order.into_iter().filter(|g| !enabled.contains(g)).collect())
    }

    /// 校验"能否禁用组"：被别的已启用组依赖时拒绝。
    pub fn check_disable(&self, enabled: &BTreeSet<String>, id: &str) -> Result<(), String> {
        if !enabled.contains(id) {
            return Err(format!("组 `{id}` 未启用"));
        }
        let requires = self.requires_map();
        for g in enabled {
            if g == id {
                continue;
            }
            if requires.get(g).map(|d| d.iter().any(|x| x == id)).unwrap_or(false) {
                return Err(format!("组 `{g}` 依赖 `{id}`，先禁用 `{g}`"));
            }
        }
        Ok(())
    }

    fn requires_map(&self) -> BTreeMap<String, Vec<String>> {
        self.report
            .groups
            .iter()
            .map(|g| (g.manifest.id.clone(), g.manifest.requires_groups.clone()))
            .collect()
    }

    /// 构建 `wanted` 里列出的条目（首次调用时加载对应 dll）。
    ///
    /// `wanted` 是**条目 id 集合**（不是组）：注册表已经算好"装了 + 启用 + 组启用"，
    /// 这里只负责把对应 dll 打开。这样"未安装的 dll 永远不会被 dlopen"。
    ///
    /// 返回 `(工具, 问题)`：单个插件加载失败只影响它自己。
    pub fn build_tools(&self, wanted: &BTreeSet<String>) -> (Vec<ToolWithGroup>, Vec<String>) {
        let mut tools: Vec<ToolWithGroup> = Vec::new();
        let mut problems = Vec::new();
        for (gid, m) in self.active_members() {
            if !wanted.contains(&m.id) {
                continue;
            }
            // 声明式清单：实现就是 PnC.toml 里的 [[tool]] + exec，不走 dll 加载
            if let Some(pnc) = &m.declarative {
                for t in tree_core::exec::build_tools(&pnc.tool, &m.dir, gid.as_str(), None) {
                    tools.push((gid.clone(), t));
                }
                continue;
            }
            if !m.has_binary() {
                problems.push(format!(
                    "`{}`（组 {gid}）缺 dll：{}",
                    m.id,
                    m.dll_path().display()
                ));
                continue;
            }
            match self.loader.ensure(&m, &self.factory) {
                Ok(loaded) => {
                    if loaded.is_disabled() {
                        problems.push(format!("`{}` 处于熔断状态，已跳过", m.id));
                        continue;
                    }
                    let risk_default = self
                        .report
                        .group(&gid)
                        .and_then(|g| g.manifest.default_risk.clone());
                    tools.push((
                        gid.clone(),
                        Arc::new(PluginTool::new(
                            m.clone(),
                            loaded,
                            risk_default.as_deref(),
                            tool::DEFAULT_TIMEOUT_MS,
                        )) as Arc<dyn ApiTool>,
                    ));
                }
                Err(e) => problems.push(format!("加载 `{}` 失败：{e}", m.id)),
            }
        }
        (tools, problems)
    }

    /// 组 + 成员聚合状态（UI 树包/插件页读这个）。
    ///
    /// `enabled` 由调用方传入（注册表里的组开关）——宿主不再自己记账。
    pub fn status(&self, enabled: &BTreeSet<String>) -> Vec<GroupStatus> {
        self.report
            .groups
            .iter()
            .map(|g| {
                let members = g
                    .members
                    .iter()
                    .map(|m| {
                        let loaded = self.loader.get(&m.id);
                        let (state, calls, error) = match &loaded {
                            Some(l) => (
                                if l.is_disabled() {
                                    PluginState::Degraded
                                } else {
                                    PluginState::Active
                                },
                                l.calls.load(std::sync::atomic::Ordering::Relaxed),
                                l.last_error.lock().map(|s| s.clone()).unwrap_or_default(),
                            ),
                            None => (PluginState::Installed, 0, String::new()),
                        };
                        PluginStatus {
                            id: m.id.clone(),
                            name: m.name.clone(),
                            risk: m.risk.clone(),
                            version: m.version.clone(),
                            state,
                            calls,
                            error,
                            has_binary: m.has_binary(),
                        }
                    })
                    .collect();
                GroupStatus {
                    id: g.manifest.id.clone(),
                    name: g.manifest.name.clone(),
                    version: g.manifest.version.clone(),
                    kind: g.manifest.kind.clone(),
                    priority: g.manifest.priority,
                    description: g.manifest.description.clone(),
                    requires: g.manifest.requires_groups.clone(),
                    enabled: enabled.contains(&g.manifest.id),
                    members,
                }
            })
            .collect()
    }

    /// 卸载全部实例（进程退出前或热重载）。
    pub fn unload_all(&self) {
        self.loader.clear();
    }

    /// 回收空闲实例。
    pub fn purge_idle(&self) -> Vec<String> {
        self.loader.purge_idle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wbp_host_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, c: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }

    fn factory() -> HostFactory {
        Arc::new(|_perms: &[String]| HostApi::empty())
    }

    fn setup(name: &str) -> (PluginHost, PathBuf) {
        let root = tmp(name);
        let plugins = root.join("plugins");
        write(
            &plugins.join("a/PnC.toml"),
            "[group]\nid = \"a\"\nname = \"A\"\npriority = 10\nplugins = []\n",
        );
        write(
            &plugins.join("b/PnC.toml"),
            "[group]\nid = \"b\"\npriority = 20\nrequires_groups = [\"a\"]\nplugins = []\n",
        );
        write(
            &plugins.join("c/PnC.toml"),
            "[group]\nid = \"c\"\npriority = 30\nplugins = []\ndefault_enabled = false\n",
        );
        // 一个成员（无 dll，仅测注册与状态）
        write(
            &plugins.join("a/x/PnC.toml"),
            "[plugin]\nid = \"x\"\npermissions = [\"http\"]\n",
        );
        let host = PluginHost::open(&plugins, factory());
        (host, root)
    }

    #[test]
    fn scan_keeps_group_order() {
        let (h, _r) = setup("order");
        assert_eq!(h.group_ids_in_order(), vec!["a", "b", "c"], "按 priority 排序");
    }

    #[test]
    fn check_enable_pulls_dependencies_in_topo_order() {
        let (h, _r) = setup("enable");
        let mut on: BTreeSet<String> = BTreeSet::new();
        let needed = h.check_enable(&on, "b").unwrap();
        assert_eq!(needed, vec!["a".to_string(), "b".to_string()], "依赖先启用");
        // 全部已开时不必再返回
        on.extend(needed);
        assert!(h.check_enable(&on, "b").unwrap().is_empty());
        // 不存在的组 → 明确报错
        assert!(h.check_enable(&on, "nope").unwrap_err().contains("不存在"));
    }

    #[test]
    fn check_disable_blocked_by_dependent() {
        let (h, _r) = setup("blocked");
        let on: BTreeSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        let err = h.check_disable(&on, "a").unwrap_err();
        assert!(err.contains('b'), "{err}");
        // 没人依赖 b：可以禁
        assert!(h.check_disable(&on, "b").is_ok());
        // 没启用过的组：禁不了
        assert!(h.check_disable(&BTreeSet::new(), "c").is_err());
    }

    #[test]
    fn build_tools_only_builds_wanted_entries() {
        let (h, _r) = setup("wanted");
        // 什么都不要求 → 不去碰 dll，也就不该报"缺 dll"
        let (tools, problems) = h.build_tools(&BTreeSet::new());
        assert!(tools.is_empty());
        assert!(problems.is_empty(), "未要求的条目不该被加载：{problems:?}");

        // 要求了但没 dll → 报缺文件
        let wanted: BTreeSet<String> = ["x".to_string()].into_iter().collect();
        let (tools, problems) = h.build_tools(&wanted);
        assert!(tools.is_empty());
        assert!(problems.iter().any(|p| p.contains("缺 dll")), "{problems:?}");
    }

    #[test]
    fn status_shape() {
        let (h, _r) = setup("status");
        let on: BTreeSet<String> = ["a".to_string()].into_iter().collect();
        let st = h.status(&on);
        assert_eq!(st.len(), 3);
        let a = st.iter().find(|g| g.id == "a").unwrap();
        assert_eq!(a.members.len(), 1);
        assert!(a.members[0].state == PluginState::Installed);
        assert!(a.members[0].risk == "low");
        assert!(a.enabled, "启用状态来自调用方传入的集合");
        let b = st.iter().find(|g| g.id == "b").unwrap();
        assert!(!b.enabled);
    }
}
