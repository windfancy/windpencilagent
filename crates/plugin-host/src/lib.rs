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

pub mod ctx;
pub mod load;
pub mod manifest;
pub mod scan;
pub mod tool;

/// 工具 + 其所属组 id（组用于 UI 分组与卸载定位）。
pub type ToolWithGroup = (String, Arc<dyn ApiTool>);

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plugin_abi::HostApi;
use wpa_api::ApiTool;

pub use ctx::CallContext;
pub use load::{CacheConfig, LoadedPlugin, Loader};
pub use manifest::{DllManifest, GroupManifest, PluginManifest};
pub use scan::{GroupEntry, ScanReport};
pub use tool::{max_risk, parse_risk, PluginTool};

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

/// 状态文件（`state/plugins.toml`）。
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct PersistedState {
    #[serde(default = "one")]
    version: u32,
    #[serde(default)]
    enabled: Vec<String>,
}

fn one() -> u32 {
    1
}

/// 插件宿主。
pub struct PluginHost {
    root: PathBuf,
    state_path: PathBuf,
    factory: HostFactory,
    report: ScanReport,
    loader: Loader,
    enabled: BTreeSet<String>,
    /// 状态文件读写失败等（不致命，但要能看见）。
    problems: Vec<String>,
}

impl PluginHost {
    /// 打开插件目录并加载状态。
    ///
    /// `state_path` 通常是 `<数据目录>/plugins.toml`；不存在时按各组的 `default_enabled` 初始化。
    pub fn open(root: impl Into<PathBuf>, state_path: impl Into<PathBuf>, factory: HostFactory) -> Self {
        let root = root.into();
        let state_path = state_path.into();
        let report = scan::scan(&root);
        let mut host = Self {
            root,
            state_path,
            factory,
            report,
            loader: Loader::new(CacheConfig::default()),
            enabled: BTreeSet::new(),
            problems: Vec::new(),
        };
        host.load_state();
        host
    }

    /// 重新扫描（新增插件后调用；不影响已加载实例）。
    pub fn rescan(&mut self) {
        self.report = scan::scan(&self.root);
        // 已启用的组若被删掉，从状态里剔除
        let known: BTreeSet<String> = self
            .report
            .groups
            .iter()
            .map(|g| g.manifest.id.clone())
            .collect();
        self.enabled.retain(|g| known.contains(g));
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

    pub fn enabled_groups(&self) -> Vec<String> {
        self.enabled.iter().cloned().collect()
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    /// 组成员（按组内声明顺序）。
    pub fn members_of(&self, group: &str) -> Vec<PluginManifest> {
        self.report
            .group(group)
            .map(|g| g.members.clone())
            .unwrap_or_default()
    }

    /// 全部已启用组的成员（去重，保持组优先序）。
    pub fn active_members(&self) -> Vec<(String, PluginManifest)> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for gid in self.enabled_groups_in_order() {
            if let Some(g) = self.report.group(&gid) {
                for m in &g.members {
                    if seen.insert(m.id.clone()) {
                        out.push((gid.clone(), m.clone()));
                    }
                }
            }
        }
        out
    }

    /// 已启用组，按 priority 排序。
    pub fn enabled_groups_in_order(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .report
            .groups
            .iter()
            .filter(|g| self.enabled.contains(&g.manifest.id))
            .map(|g| g.manifest.id.clone())
            .collect();
        // report 已按 priority 排序，这里保持
        if v.is_empty() {
            v = Vec::new();
        }
        v
    }

    /// 启用组（连带启用其依赖）。
    pub fn enable(&mut self, id: &str) -> Result<Vec<String>, String> {
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

        let mut newly = Vec::new();
        for g in order {
            if self.enabled.insert(g.clone()) {
                newly.push(g);
            }
        }
        self.save_state();
        Ok(newly)
    }

    /// 禁用组（有其它已启用组依赖它时拒绝）。
    pub fn disable(&mut self, id: &str) -> Result<(), String> {
        if !self.enabled.contains(id) {
            return Err(format!("组 `{id}` 未启用"));
        }
        for g in self.enabled.clone() {
            if g == id {
                continue;
            }
            if let Some(deps) = self.requires_map().get(&g) {
                if deps.iter().any(|d| d == id) {
                    return Err(format!("组 `{g}` 依赖 `{id}`，先禁用 `{g}`"));
                }
            }
        }
        self.enabled.remove(id);
        // 卸载成员实例（可能被其他组共享：还在别的已启用组里就留着）
        let still_used: BTreeSet<String> = self
            .active_members()
            .into_iter()
            .map(|(_, m)| m.id)
            .collect();
        if let Some(g) = self.report.group(id) {
            for m in &g.members {
                if !still_used.contains(&m.id) {
                    self.loader.unload(&m.id);
                }
            }
        }
        self.save_state();
        Ok(())
    }

    /// 启用/禁用切换。
    pub fn toggle(&mut self, id: &str, on: bool) -> Result<(), String> {
        if on {
            self.enable(id).map(|_| ())
        } else {
            self.disable(id)
        }
    }

    fn requires_map(&self) -> BTreeMap<String, Vec<String>> {
        self.report
            .groups
            .iter()
            .map(|g| (g.manifest.id.clone(), g.manifest.requires_groups.clone()))
            .collect()
    }

    /// 构建已启用组的全部工具（首次调用时加载对应 dll）。
    ///
    /// 返回 `(工具, 问题)`：单个插件加载失败只影响它自己。
    pub fn build_tools(&self) -> (Vec<ToolWithGroup>, Vec<String>) {
        let mut tools: Vec<ToolWithGroup> = Vec::new();
        let mut problems = Vec::new();
        for (gid, m) in self.active_members() {
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
    pub fn status(&self) -> Vec<GroupStatus> {
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
                    enabled: self.enabled.contains(&g.manifest.id),
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

    // ---- 状态持久化 ----

    fn load_state(&mut self) {
        let text = match std::fs::read_to_string(&self.state_path) {
            Ok(t) => t,
            Err(_) => {
                // 首次运行：按组的 default_enabled 初始化
                for g in &self.report.groups {
                    if g.manifest.default_enabled {
                        self.enabled.insert(g.manifest.id.clone());
                    }
                }
                return;
            }
        };
        match toml::from_str::<PersistedState>(&text) {
            Ok(st) => {
                for id in st.enabled {
                    if self.report.group(&id).is_some() {
                        self.enabled.insert(id);
                    } else {
                        self.problems
                            .push(format!("状态文件里的组 `{id}` 已不存在，已忽略"));
                    }
                }
            }
            Err(e) => {
                self.problems
                    .push(format!("状态文件解析失败（按默认启用）：{e}"));
                for g in &self.report.groups {
                    if g.manifest.default_enabled {
                        self.enabled.insert(g.manifest.id.clone());
                    }
                }
            }
        }
    }

    fn save_state(&mut self) {
        let st = PersistedState {
            version: 1,
            enabled: self.enabled.iter().cloned().collect(),
        };
        let text = match toml::to_string_pretty(&st) {
            Ok(t) => t,
            Err(e) => {
                self.problems.push(format!("状态序列化失败：{e}"));
                return;
            }
        };
        if let Some(dir) = self.state_path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                self.problems.push(format!("创建状态目录失败：{e}"));
                return;
            }
        }
        let tmp = self.state_path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            self.problems.push(format!("写状态文件失败：{e}"));
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.state_path) {
            self.problems.push(format!("替换状态文件失败：{e}"));
        }
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
        let host = PluginHost::open(&plugins, root.join("state/plugins.toml"), factory());
        (host, root)
    }

    #[test]
    fn default_enabled_from_manifest() {
        let (h, _r) = setup("default");
        assert!(h.is_enabled("a"));
        assert!(h.is_enabled("b"), "requires 不会自动启用，但 default_enabled 默认 true");
        assert!(!h.is_enabled("c"));
    }

    #[test]
    fn enable_pulls_dependencies_in_topo_order() {
        let (mut h, _r) = setup("enable");
        h.disable("b").unwrap();
        h.disable("a").unwrap();
        assert!(!h.is_enabled("a"));
        let newly = h.enable("b").unwrap();
        assert_eq!(newly, vec!["a".to_string(), "b".to_string()], "依赖先启用");
        assert!(h.is_enabled("a") && h.is_enabled("b"));
    }

    #[test]
    fn disable_blocked_by_dependent() {
        let (mut h, _r) = setup("blocked");
        h.enable("b").unwrap();
        let err = h.disable("a").unwrap_err();
        assert!(err.contains("\"b\"") || err.contains("b"), "{err}");
        h.disable("b").unwrap();
        h.disable("a").unwrap();
    }

    #[test]
    fn state_persists_across_reopen() {
        let (mut h, root) = setup("persist");
        h.disable("b").unwrap();
        h.enable("c").unwrap();
        drop(h);
        let h2 = PluginHost::open(
            root.join("plugins"),
            root.join("state/plugins.toml"),
            factory(),
        );
        assert!(!h2.is_enabled("b"));
        assert!(h2.is_enabled("c"));
        assert!(h2.is_enabled("a"));
    }

    #[test]
    fn build_tools_reports_missing_binary() {
        let (h, _r) = setup("tools");
        let (tools, problems) = h.build_tools();
        assert!(tools.is_empty(), "没有 dll 就构建不出工具");
        assert!(problems.iter().any(|p| p.contains("缺 dll")), "{problems:?}");
    }

    #[test]
    fn status_shape() {
        let (h, _r) = setup("status");
        let st = h.status();
        assert_eq!(st.len(), 3);
        let a = st.iter().find(|g| g.id == "a").unwrap();
        assert_eq!(a.members.len(), 1);
        assert!(a.members[0].state == PluginState::Installed);
        assert!(a.members[0].risk == "low");
    }
}
