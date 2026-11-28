//! 工具注册表（`data/registry.toml`）——**能力的唯一真相源**（P0-1）。
//!
//! 为什么必须有它：工具来自三类地方（内置 crate / `plugins/` 下的 dll / 声明式 `PnC.toml`），
//! 如果"谁在磁盘上就注册谁"，会出三个问题：
//!
//! 1. 用户没法"禁用一个工具"——删了文件才算禁用，而且删了它就彻底消失了，
//!    下次想恢复得重新编译/重新下载；
//! 2. 插件目录里放一个半成品 dll，它立刻出现在工具面里（模型会去调它）；
//! 3. 装错文件（id 与目录名不符）时没有"记录"可比对，只能靠加载时报错。
//!
//! 所以规则是：**扫描只产出"候选"，注册表才是"装了什么、开没开"的账本。**
//!
//! 状态机（条目级）：
//!
//! ```text
//!   discovered ──install──▶ installed(ok) ──disable──▶ installed(disabled)
//!        │                        │
//!        │                        └──文件被删──▶ broken（条目保留，不消失）
//!        └──uninstall──▶ 移除条目
//! ```
//!
//! `broken` 刻意保留而不是删除：用户删了 dll 要能看见"它曾经装过、现在缺文件"，
//! 而不是工具悄悄消失（那是排查噩梦）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::catalog::{self, BuiltinSpec};

/// 注册表格式版本。加字段不升版（`serde(default)` 兜底）；语义变更才升。
pub const REGISTRY_VERSION: u32 = 1;

/// 「未分配」组 id：存放所有不属于任何用户组的条目（移出组、卸载后的落点）。
///
/// 这是保留组：不能新建、改名、删除，也不能直接安装其中的条目——
/// 必须先由用户分配到具体组。它始终存在，且固定在列表末尾。
///
/// ## 为什么 id 不带下划线
///
/// 它同时是磁盘上的目录名（`plugins/unassigned/`）。`plugin_host::scan` 会跳过
/// `.`／`_` 开头的目录（那是"用户自己放的共享库/素材"的约定俗成），所以历史上
/// `__unassigned__` 这个 id 永远对不上一个真目录：往里面放 dll 会被静默忽略。
/// 改成一个普通名字后，id、目录名、`PnC.toml` 三者一致，扫描链路才闭合。
///
/// 旧数据由 [`ToolRegistry::migrate_ids`] 一次性改名。
pub const UNASSIGNED_GROUP: &str = "unassigned";

/// 未分配的旧 id（`migrate_ids` 用）。
const LEGACY_UNASSIGNED: &str = "__unassigned__";

/// 从用户输入的名称派生一个**可当目录名**的组 id。
///
/// 用户只该输入组名（「我的抓包工具」），不该被要求手敲 id——那违反易用性铁律。
/// 中文名派生不出有意义的 ASCII，退化成 `grp`（再由 [`unique_group_id`] 加序号）。
pub fn slug_group_id(name: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_sep = false;
        } else if !prev_sep && !out.is_empty() {
            out.push('_');
            prev_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    // 目录名太长在 Windows 上会撞 260 上限（插件目录本身还可能嵌套），截短
    if out.len() > 24 {
        out.truncate(24);
        while out.ends_with('_') {
            out.pop();
        }
    }
    if out.is_empty() {
        out.push_str("grp");
    }
    // 纯数字开头容易和"编号目录"混淆，加前缀让它一眼看出是个组
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "grp_");
    }
    out
}

/// 在已有组与基础组之外求一个不冲突的组 id。
pub fn unique_group_id(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let free = |cand: &str| {
        !taken(cand) && !catalog::is_base_group(cand) && cand != UNASSIGNED_GROUP
    };
    if free(base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let cand = format!("{base}_{n}");
        if free(&cand) {
            return cand;
        }
    }
    format!("{base}_x")
}

/// 条目状态。
pub mod status {
    /// 清单与文件都在，已安装，可用。
    pub const OK: &str = "ok";
    /// 已安装但文件/清单不见（用户删了 dll 或改坏了 PnC.toml）。
    pub const BROKEN: &str = "broken";
    /// 扫描到了但没安装（候选）。
    pub const DISCOVERED: &str = "discovered";
}

/// 条目来源。
pub mod source {
    pub const BUILTIN: &str = "builtin";
    pub const PLUGIN: &str = "plugin";
    pub const DECLARATIVE: &str = "declarative";
}

/// 注册表根。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolRegistry {
    pub version: u32,
    /// TOML 里是 `[[group]]`。
    #[serde(rename = "group")]
    pub groups: Vec<GroupRecord>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self { version: REGISTRY_VERSION, groups: Vec::new() }
    }
}

/// 一个能力组（`[[group]]`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupRecord {
    pub id: String,
    pub name: String,
    /// 用户可编辑的组描述（管理 UI 的副标题）。
    pub description: String,
    /// tools / knowledge / logic / skills
    pub kind: String,
    /// builtin / plugin / declarative
    pub source: String,
    /// 组级开关（关掉后组内条目一律不进工具面，但条目自己的 enabled 保留）。
    pub enabled: bool,
    /// TOML 里是 `[[group.entry]]`。
    #[serde(rename = "entry")]
    pub entries: Vec<EntryRecord>,
}

/// 一个条目（一个工具 / 一条知识 / 一个技能）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EntryRecord {
    /// 工具名（`file.read`），注册表与工具面的主键。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// builtin / plugin / declarative
    pub source: String,
    /// dll 相对路径或声明式目录（内置条目为空）。
    pub path: String,
    /// 展示用风险档（真正审批看工具自己的 `risk_for`）。
    pub risk: String,
    /// 能力标签（`wpa_api::registry::caps` 受控词表）。
    pub capabilities: Vec<String>,
    /// 是否已安装（false = 只是候选）。
    pub installed: bool,
    /// 是否启用（组关掉时这一位保留原值）。
    pub enabled: bool,
    /// `ok` / `broken` / `discovered`
    pub status: String,
    /// 备注（缺 dll、清单坏掉的原因）。
    pub note: String,
}

impl EntryRecord {
    /// 当前是否应当进工具面：装好、开着、健康、且所在组也开着（组开关由调用方传入）。
    pub fn active(&self, group_enabled: bool) -> bool {
        group_enabled && self.installed && self.enabled && self.status == status::OK
    }
}

/// 扫描快照（由装配层从 `plugin_host::ScanReport` 转换而来）。
///
/// 用中间结构而不是直接吃 `plugin_host` 的类型，是为了让注册表不依赖插件宿主——
/// 将来加"声明式工具目录扫描"时也复用同一个入口。
#[derive(Debug, Clone, Default)]
pub struct ScanSnapshot {
    pub groups: Vec<ScannedGroup>,
}

#[derive(Debug, Clone, Default)]
pub struct ScannedGroup {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub source: String,
    pub entries: Vec<ScannedEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct ScannedEntry {
    pub id: String,
    pub name: String,
    pub risk: String,
    pub capabilities: Vec<String>,
    pub path: String,
    /// 文件是否齐备（dll 在 / 声明式目录可读）。
    pub has_file: bool,
    pub note: String,
}

impl ToolRegistry {
    /// 从磁盘读取；文件不存在时按内置目录生成一份并落盘。
    ///
    /// 返回 `(registry, created)`：`created = true` 表示这次是新生成的
    /// （UI 可以提示"已按内置工具初始化注册表"）。
    pub fn load_or_init(path: &Path) -> Result<(Self, bool), String> {
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("读取注册表失败 {}：{e}", path.display()))?;
            if text.trim().is_empty() {
                let reg = Self::from_catalog();
                reg.save(path)?;
                return Ok((reg, true));
            }
            let mut reg: Self = toml::from_str(&text)
                .map_err(|e| format!("注册表解析失败 {}：{e}", path.display()))?;
            // 内置条目先归位（内置工具不允许换组），再补齐版本升级新增的。
            // 顺序不能反：先 rehome 能把用户挪走的副本（连同 enabled 设置）收回来，
            // 若先 ensure 会在原组补一份默认值，用户的开关就丢了。
            let mut changed = reg.migrate_ids();
            changed |= reg.rehome_builtins();
            changed |= reg.ensure_builtins();
            // 旧版扫描会把候选（installed=false）写进账本；新语义下账本只记已安装，
            // 候选由扫描快照重建。这里把残留剔掉，否则能力页会出现"幽灵条目"。
            if reg.drop_uninstalled() > 0 {
                changed = true;
            }
            if changed {
                reg.save(path)?;
            }
            return Ok((reg, false));
        }
        let reg = Self::from_catalog();
        reg.save(path)?;
        Ok((reg, true))
    }

    /// 旧 id 改名：`__unassigned__` → [`UNASSIGNED_GROUP`]。返回是否有改动。
    ///
    /// 只在语义变更时用一次。它必须跑在 `ensure_builtins` 之前——否则后者会
    /// 看到"没有未分配组"而补一个新的，旧组连同里面的条目原地变成孤儿。
    pub fn migrate_ids(&mut self) -> bool {
        let mut changed = false;
        for g in &mut self.groups {
            if g.id == LEGACY_UNASSIGNED {
                g.id = UNASSIGNED_GROUP.to_string();
                changed = true;
            }
        }
        changed
    }

    /// 按 `catalog::CAP_GROUPS` + `BUILTIN_SPECS` 生成初始注册表。
    ///
    /// 内置条目一律 `installed = true, enabled = true, status = ok` —— 基座装好就该能用，
    /// 让用户先手动"安装"自己的内置工具没有意义。
    pub fn from_catalog() -> Self {
        let mut groups: Vec<GroupRecord> = catalog::CAP_GROUPS
            .iter()
            .map(|g| GroupRecord {
                id: g.id.to_string(),
                name: g.label.to_string(),
                description: String::new(),
                kind: "tools".to_string(),
                source: source::BUILTIN.to_string(),
                enabled: true,
                entries: Vec::new(),
            })
            .collect();
        for spec in catalog::BUILTIN_SPECS {
            let idx = match groups.iter().position(|g| g.id == spec.group) {
                Some(i) => i,
                None => continue, // 规格指向未知组：catalog 的测试会拦住
            };
            groups[idx].entries.push(entry_from_spec(spec));
        }
        // 未分配组固定在末尾，作为所有「无归属」条目的落点。
        groups.push(make_unassigned_group());
        Self { version: REGISTRY_VERSION, groups }
    }

    /// 补齐缺失的内置条目/组。返回是否有改动。
    pub fn ensure_builtins(&mut self) -> bool {
        let mut changed = false;
        for spec in catalog::BUILTIN_SPECS {
            let gi = match self.groups.iter().position(|g| g.id == spec.group) {
                Some(i) => i,
                None => {
                    let label = catalog::label_of(spec.group).to_string();
                    self.groups.push(GroupRecord {
                        id: spec.group.to_string(),
                        name: label,
                        description: String::new(),
                        kind: "tools".to_string(),
                        source: source::BUILTIN.to_string(),
                        enabled: true,
                        entries: Vec::new(),
                    });
                    changed = true;
                    self.groups.len() - 1
                }
            };
            if !self.groups[gi].entries.iter().any(|e| e.id == spec.name) {
                self.groups[gi].entries.push(entry_from_spec(spec));
                changed = true;
            }
        }
        // 旧注册表升级时补齐保留组（新生成的已由 from_catalog 建好）。
        if !self.groups.iter().any(|g| g.id == UNASSIGNED_GROUP) {
            self.groups.push(make_unassigned_group());
            changed = true;
        }
        changed
    }

    /// 把被移出原组的内置条目归位。返回是否有改动。
    ///
    /// 内置工具**不允许换组**：它的所属组由 `catalog::BUILTIN_SPECS` 决定，是代码契约，
    /// 不是用户资产。而 `ensure_builtins` 是按 `spec.group` 补缺的——一旦用户把它挪到
    /// 别的组，加载时原组会再补一份，同一个 id 就出现在两个组（能力页两行、装配口径
    /// 按组名取第一个，全部对不上）。所以加载时必须先收回复本再补缺。
    ///
    /// 收回时**保留条目本身的 `enabled`**（用户禁用了某个内置工具，归位后仍然禁用）。
    pub fn rehome_builtins(&mut self) -> bool {
        let mut changed = false;
        for spec in catalog::BUILTIN_SPECS {
            // 收走所有"不在目标组"的同名内置副本（通常只会有 1 份，多份时留第一份）
            let mut carried: Option<EntryRecord> = None;
            for gi in 0..self.groups.len() {
                if self.groups[gi].id == spec.group {
                    continue;
                }
                let mut ei = 0;
                while ei < self.groups[gi].entries.len() {
                    let hit = self.groups[gi].entries[ei].id == spec.name
                        && self.groups[gi].entries[ei].source == source::BUILTIN;
                    if hit {
                        let e = self.groups[gi].entries.remove(ei);
                        if carried.is_none() {
                            carried = Some(e);
                        }
                        changed = true;
                    } else {
                        ei += 1;
                    }
                }
            }
            let Some(c) = carried else { continue };
            match self.groups.iter_mut().find(|g| g.id == spec.group) {
                Some(g) => {
                    // 目标组里已有同名条目：说明是重复副本，直接丢弃（已在上面移除来源）
                    if !g.entries.iter().any(|e| e.id == spec.name) {
                        g.entries.push(c);
                    }
                }
                None => {
                    // 目标组整个被删掉了：重建组并把条目放回
                    self.groups.push(GroupRecord {
                        id: spec.group.to_string(),
                        name: catalog::label_of(spec.group).to_string(),
                        description: String::new(),
                        kind: "tools".to_string(),
                        source: source::BUILTIN.to_string(),
                        enabled: true,
                        entries: vec![c],
                    });
                }
            }
        }
        changed
    }

    /// 剔除 `installed == false` 的非内置条目（旧版残留的候选）。返回剔除数。
    pub fn drop_uninstalled(&mut self) -> usize {
        let mut n = 0;
        for g in &mut self.groups {
            let before = g.entries.len();
            g.entries.retain(|e| e.installed || e.source == source::BUILTIN);
            n += before - g.entries.len();
        }
        n
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建注册表目录失败 {}：{e}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| format!("注册表序列化失败：{e}"))?;
        // 先写临时文件再改名：写到一半崩溃不会留下半个注册表
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("写注册表失败 {}：{e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换注册表失败 {}：{e}", path.display()))?;
        Ok(())
    }

    // ---------------------------------------------------------------- 查询

    pub fn group(&self, id: &str) -> Option<&GroupRecord> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn group_mut(&mut self, id: &str) -> Option<&mut GroupRecord> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    pub fn entry(&self, tool: &str) -> Option<(&GroupRecord, &EntryRecord)> {
        self.groups.iter().find_map(|g| {
            g.entries.iter().find(|e| e.id == tool).map(|e| (g, e))
        })
    }

    /// 工具是否应当进工具面（装好 + 开着 + 健康 + 组开着）。
    pub fn is_active(&self, tool: &str) -> bool {
        match self.entry(tool) {
            Some((g, e)) => e.active(g.enabled),
            // 注册表里没有记录的工具（运行时才注册的 session.search / plan.*）
            // 按"默认放行"处理：注册表管的是"可管理的条目"，不是拦截器。
            None => true,
        }
    }

    /// 组是否启用。
    pub fn group_enabled(&self, id: &str) -> bool {
        self.group(id).map(|g| g.enabled).unwrap_or(true)
    }

    /// 当前生效的工具名集合（仅注册表里有记录的）。
    pub fn active_tools(&self) -> Vec<String> {
        self.groups
            .iter()
            .filter(|g| g.enabled)
            .flat_map(|g| g.entries.iter().filter(|e| e.active(true)).map(|e| e.id.clone()))
            .collect()
    }

    // ---------------------------------------------------------------- 操作

    /// 新建一个用户组。失败时给出中文原因。
    pub fn add_group(&mut self, id: &str, name: &str, description: &str) -> Result<(), String> {
        if id.trim().is_empty() {
            return Err("组 id 不能为空".to_string());
        }
        if name.trim().is_empty() {
            return Err("组名称不能为空".to_string());
        }
        if id == UNASSIGNED_GROUP {
            return Err("「未分配」是保留组，不能新建".to_string());
        }
        if self.groups.iter().any(|g| g.id == id) {
            return Err(format!("组 id「{id}」已存在"));
        }
        self.groups.push(GroupRecord {
            id: id.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            kind: "tools".to_string(),
            source: source::PLUGIN.to_string(),
            enabled: true,
            entries: Vec::new(),
        });
        Ok(())
    }

    /// 重命名用户组（保留的「未分配」组不可改名）。
    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<(), String> {
        if id == UNASSIGNED_GROUP {
            return Err("「未分配」是保留组，不能改名".to_string());
        }
        if name.trim().is_empty() {
            return Err("组名称不能为空".to_string());
        }
        let g = self.group_mut(id).ok_or_else(|| format!("组「{id}」不存在"))?;
        g.name = name.to_string();
        Ok(())
    }

    /// 设置组描述（管理 UI 的副标题）。
    pub fn set_group_description(&mut self, id: &str, description: &str) -> Result<(), String> {
        let g = self.group_mut(id).ok_or_else(|| format!("组「{id}」不存在"))?;
        g.description = description.to_string();
        Ok(())
    }

    /// 删除用户组：组内条目全部移入保留的「未分配」组，组本身删除。
    ///
    /// 返回实际移动的条目数。条目状态按 `move_entry` 规则保留，不重置开关。
    pub fn remove_group(&mut self, id: &str) -> Result<usize, String> {
        if id == UNASSIGNED_GROUP {
            return Err("「未分配」是保留组，不能删除".to_string());
        }
        // 内置组（core / file / net / …）由 catalog 定义，删了下次 load 会被
        // `ensure_builtins` 重建，而组内内置条目会先被搬进「未分配」——
        // 结果是"组回来了、工具却留在隔离区"。直接拒绝，别留这个坑。
        if self.group(id).map(|g| g.source == source::BUILTIN).unwrap_or(false) {
            return Err(format!(
                "组「{id}」是内置组（由基座提供），不能删除；不需要时在能力页关掉它的启用开关"
            ));
        }
        let tools: Vec<String> = self
            .group(id)
            .ok_or_else(|| format!("组「{id}」不存在"))?
            .entries
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let n = tools.len();

        if self.group(UNASSIGNED_GROUP).is_none() {
            self.groups.push(make_unassigned_group());
        }
        for tool in &tools {
            self.move_entry(tool, UNASSIGNED_GROUP);
        }
        self.groups.retain(|g| g.id != id);
        Ok(n)
    }

    /// 组级启停。返回是否真的变了。
    pub fn set_group_enabled(&mut self, id: &str, on: bool) -> bool {
        match self.group_mut(id) {
            Some(g) if g.enabled != on => {
                g.enabled = on;
                true
            }
            _ => false,
        }
    }

    /// 条目级启停。返回是否真的变了。
    ///
    /// 「未分配」组的条目不允许启用：`on = true` 一律拒绝并返回 false。
    pub fn set_entry_enabled(&mut self, tool: &str, on: bool) -> bool {
        for g in &mut self.groups {
            let is_unassigned = g.id == UNASSIGNED_GROUP;
            if let Some(e) = g.entries.iter_mut().find(|e| e.id == tool) {
                if is_unassigned && on {
                    return false;
                }
                if e.enabled != on {
                    e.enabled = on;
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// 安装单个条目（`discovered` → `ok`）。
    ///
    /// - 条目必须存在；已安装时幂等返回 Ok。
    /// - `broken`（缺文件）给「文件缺失，无法安装」。
    /// - 位于保留的「未分配」组时给「请先分配到组再安装」。
    pub fn install_entry(&mut self, tool: &str) -> Result<(), String> {
        let (gi, ei) = self
            .groups
            .iter()
            .enumerate()
            .find_map(|(gi, g)| {
                g.entries.iter().position(|e| e.id == tool).map(|ei| (gi, ei))
            })
            .ok_or_else(|| format!("条目「{tool}」不存在"))?;

        // 幂等优先：重复点击安装不应报错。
        if self.groups[gi].entries[ei].installed {
            return Ok(());
        }
        if self.groups[gi].id == UNASSIGNED_GROUP {
            return Err("请先分配到组再安装".to_string());
        }
        if self.groups[gi].entries[ei].status == status::BROKEN {
            return Err("文件缺失，无法安装".to_string());
        }

        let enabled = self.groups[gi].id != UNASSIGNED_GROUP;
        let e = &mut self.groups[gi].entries[ei];
        e.installed = true;
        e.status = status::OK.to_string();
        e.enabled = enabled;
        Ok(())
    }

    /// 卸载单个条目：**从注册表删除**（注册表只记已安装）。
    ///
    /// 删掉之后它会重新出现在"扫描候选"里（UI 中栏仍可见，状态"未安装"），
    /// 所以用户不会觉得"东西丢了"，但账本里确实没有它了。
    ///
    /// 内置条目不能卸载（由代码提供）：不需要时用 `set_entry_enabled(false)`。
    pub fn uninstall_entry(&mut self, tool: &str) -> Result<(), String> {
        for gi in 0..self.groups.len() {
            let Some(ei) = self.groups[gi].entries.iter().position(|e| e.id == tool) else {
                continue;
            };
            if self.groups[gi].entries[ei].source == source::BUILTIN {
                return Err(format!(
                    "「{tool}」是内置工具，不能卸载；不需要时改成禁用"
                ));
            }
            self.groups[gi].entries.remove(ei);
            return Ok(());
        }
        Err(format!("条目「{tool}」不存在"))
    }

    /// 卸载一个组：删除组内所有**非内置**条目（内置工具固定属于内置组）。
    ///
    /// 组本身保留（可能还有内置条目，或用户想重新装回同一个组）。
    /// 返回删除的条目数。
    pub fn uninstall_group(&mut self, id: &str) -> usize {
        let Some(g) = self.group_mut(id) else { return 0 };
        let before = g.entries.len();
        g.entries.retain(|e| e.source == source::BUILTIN);
        let n = before - g.entries.len();
        if n > 0 {
            g.enabled = false;
        }
        n
    }

    /// 注册表里有没有「已安装」的记录。
    ///
    /// 插件装配的**硬闸门**：`is_active` 对"查不到"的条目默认放行（为 `session.search`
    /// / `plan.*` 这类运行时工具留的口子），未安装的插件同样查不到，所以不能只靠它。
    pub fn is_installed(&self, tool: &str) -> bool {
        self.entry(tool).map(|(_, e)| e.installed).unwrap_or(false)
    }

    /// 与扫描结果对账：**只更新已安装条目的事实状态，从不新增、从不删除**。
    ///
    /// 这是「注册表只记已安装」的核心。扫描到的候选不落库——它们留在内存快照里
    /// （`snapshot` 把候选合并进 UI 行），用户点「安装」时才由
    /// [`install_candidate`](Self::install_candidate) 落成正式条目。
    ///
    /// - 已安装 + 扫到 + 文件在 ⇒ `ok`
    /// - 已安装 + 扫到 + 文件缺 ⇒ `broken`
    /// - 已安装 + 没扫到 ⇒ `broken`（条目保留，留证据）
    /// - 内置条目：跳过（由 catalog 管，不归扫描）
    pub fn reconcile(&mut self, snap: &ScanSnapshot) -> MergeReport {
        let mut report = MergeReport::default();
        // 只看条目 id（不分组）：用户可能已经把条目移到别的组，
        // 按"扫描组 + 条目"配对会把移动结果覆盖掉。
        let mut seen: Vec<String> = Vec::new();

        for sg in &snap.groups {
            for se in &sg.entries {
                seen.push(se.id.clone());

                // 全局查找（跨组）：找到就更新事实位，**不改组归属**
                let found = self.groups.iter().enumerate().find_map(|(gi, g)| {
                    g.entries
                        .iter()
                        .position(|e| e.id == se.id)
                        .map(|ei| (gi, ei))
                });
                // 没找到 = 候选：不落库，等用户安装
                let Some((gi2, ei2)) = found else { continue };
                let e = &mut self.groups[gi2].entries[ei2];
                if e.source == source::BUILTIN {
                    continue;
                }
                e.name = se.name.clone();
                e.risk = se.risk.clone();
                e.path = se.path.clone();
                if !se.capabilities.is_empty() {
                    e.capabilities = se.capabilities.clone();
                }
                if se.has_file {
                    if e.status == status::BROKEN {
                        report.recovered += 1;
                        e.note.clear();
                    }
                    e.status = status::OK.to_string();
                } else {
                    e.status = status::BROKEN.to_string();
                    e.note = if se.note.is_empty() {
                        "文件缺失".to_string()
                    } else {
                        se.note.clone()
                    };
                    report.broken += 1;
                }
            }
        }

        // 注册表里有、这次没扫到的非内置条目 → broken（不删，留证据）
        for g in &mut self.groups {
            for e in &mut g.entries {
                if e.source == source::BUILTIN {
                    continue;
                }
                if !seen.contains(&e.id) && e.status != status::BROKEN {
                    e.status = status::BROKEN.to_string();
                    e.note = "未扫描到（目录或清单被移除）".to_string();
                    report.broken += 1;
                }
            }
        }

        report
    }

    /// 把扫描候选落成正式条目。
    ///
    /// 注册表里没有候选（只记已安装），所以候选数据必须由调用方从扫描快照传进来。
    /// `group` 是目标组；`source_kind` 取 `plugin` / `declarative`。
    ///
    /// 装进「未分配」时落成"已安装但未启用"——它可以被移动，但不能直接跑。
    pub fn install_candidate(
        &mut self,
        group: &str,
        source_kind: &str,
        se: &ScannedEntry,
    ) -> Result<(), String> {
        if let Some((g, _)) = self.entry(&se.id) {
            return Err(format!(
                "「{}」已经装在组「{}」里了（同一工具只能属于一个组）",
                se.id, g.id
            ));
        }
        if !se.has_file {
            return Err(if se.note.is_empty() {
                "文件缺失，无法安装".to_string()
            } else {
                format!("文件缺失，无法安装：{}", se.note)
            });
        }
        let src = if source_kind.is_empty() {
            source::PLUGIN.to_string()
        } else {
            source_kind.to_string()
        };
        let gi = match self.groups.iter().position(|g| g.id == group) {
            Some(i) => i,
            None => {
                self.groups.push(GroupRecord {
                    id: group.to_string(),
                    name: group.to_string(),
                    description: String::new(),
                    kind: "tools".to_string(),
                    source: src.clone(),
                    enabled: true,
                    entries: Vec::new(),
                });
                self.groups.len() - 1
            }
        };
        let enabled = self.groups[gi].id != UNASSIGNED_GROUP;
        self.groups[gi].entries.push(EntryRecord {
            id: se.id.clone(),
            name: se.name.clone(),
            source: src,
            path: se.path.clone(),
            risk: se.risk.clone(),
            capabilities: se.capabilities.clone(),
            installed: true,
            enabled,
            status: status::OK.to_string(),
            note: String::new(),
        });
        Ok(())
    }

    /// 批量安装某组里扫描到的候选（跳过已装的）。返回实际安装数。
    pub fn install_group_from(&mut self, id: &str, snap: &ScanSnapshot) -> usize {
        let Some(sg) = snap.groups.iter().find(|g| g.id == id) else {
            return 0;
        };
        let mut n = 0;
        for se in &sg.entries {
            if self.entry(&se.id).is_some() {
                continue;
            }
            if self.install_candidate(id, &sg.source, se).is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            if let Some(g) = self.group_mut(id) {
                g.enabled = true;
            }
        }
        n
    }

    /// 把条目移到另一个组（组↔条目绑定）。返回 `(from, to)`。
    ///
    /// 目标组不存在时新建（source 跟随条目来源）。移动**不改**条目的
    /// installed/enabled 状态——换归属不该重置用户的开关。
    pub fn move_entry(&mut self, tool: &str, to_group: &str) -> Option<(String, String)> {
        let (from_idx, ent_idx) = self.groups.iter().enumerate().find_map(|(gi, g)| {
            g.entries.iter().position(|e| e.id == tool).map(|ei| (gi, ei))
        })?;
        let from = self.groups[from_idx].id.clone();
        // 内置工具属于固定组：既不能换组，也不能被移进「未分配」。
        // 同组 passthrough 放行，避免调用方把"原地不动"当成错误。
        if from != to_group && self.groups[from_idx].entries[ent_idx].source == source::BUILTIN {
            return None;
        }
        let entry = self.groups[from_idx].entries.remove(ent_idx);
        // 同一工具只允许存在于一个组：除当前这条外，把别处的同名条目一并清掉
        for g in &mut self.groups {
            g.entries.retain(|e| e.id != tool);
        }
        if from == to_group {
            self.groups[from_idx].entries.push(entry);
            return Some((from.clone(), from));
        }
        let to_idx = match self.groups.iter().position(|g| g.id == to_group) {
            Some(i) => i,
            None => {
                self.groups.push(GroupRecord {
                    id: to_group.to_string(),
                    name: to_group.to_string(),
                    description: String::new(),
                    kind: "tools".to_string(),
                    source: entry.source.clone(),
                    enabled: true,
                    entries: Vec::new(),
                });
                self.groups.len() - 1
            }
        };
        let mut entry = entry;
        entry.note = if entry.note.is_empty() {
            String::new()
        } else {
            format!("{}（由 {from} 移入）", entry.note)
        };
        self.groups[to_idx].entries.push(entry);
        Some((from, to_group.to_string()))
    }

    /// 把条目分配到目标组。目标组必须已存在（不会隐式新建）。
    ///
    /// 复用 `move_entry` 完成迁移；已安装的条目迁移后自动启用，
    /// 目标为保留的「未分配」组时一律关闭。
    pub fn assign_entry(&mut self, tool: &str, group: &str) -> Result<(), String> {
        if self.group(group).is_none() {
            return Err(format!("目标组「{group}」不存在"));
        }
        // 先判来源：内置工具换组要给"不能换"的明确原因，而不是笼统地报"条目不存在"
        let (from_id, is_builtin, installed) = {
            let (g, e) = self
                .entry(tool)
                .ok_or_else(|| format!("条目「{tool}」不存在"))?;
            (g.id.clone(), e.source == source::BUILTIN, e.installed)
        };
        if is_builtin && from_id != group {
            return Err(format!(
                "「{tool}」是内置工具，固定属于组「{from_id}」，不能换组"
            ));
        }

        self.move_entry(tool, group)
            .ok_or_else(|| format!("条目「{tool}」不存在"))?;

        for g in &mut self.groups {
            if g.id == group {
                if let Some(e) = g.entries.iter_mut().find(|e| e.id == tool) {
                    if group == UNASSIGNED_GROUP {
                        e.enabled = false;
                    } else if installed {
                        e.enabled = true;
                    }
                }
                break;
            }
        }
        Ok(())
    }

    /// 统计（UI 顶部一行摘要）。
    pub fn stats(&self) -> Stats {
        let mut s = Stats::default();
        for g in &self.groups {
            s.groups += 1;
            if g.enabled {
                s.groups_enabled += 1;
            }
            for e in &g.entries {
                s.entries += 1;
                match e.status.as_str() {
                    status::OK => s.ok += 1,
                    status::BROKEN => s.broken += 1,
                    status::DISCOVERED => s.discovered += 1,
                    _ => {}
                }
                if e.installed && e.enabled && g.enabled && e.status == status::OK {
                    s.active += 1;
                }
            }
        }
        s
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub groups: usize,
    pub groups_enabled: usize,
    pub entries: usize,
    pub ok: usize,
    pub broken: usize,
    pub discovered: usize,
    pub active: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MergeReport {
    pub new_groups: usize,
    pub new_entries: usize,
    pub broken: usize,
    pub recovered: usize,
}

impl MergeReport {
    pub fn is_empty(&self) -> bool {
        self.new_groups == 0 && self.new_entries == 0 && self.broken == 0 && self.recovered == 0
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.new_groups > 0 {
            parts.push(format!("新组 {} 个", self.new_groups));
        }
        if self.new_entries > 0 {
            parts.push(format!("新条目 {} 个", self.new_entries));
        }
        if self.broken > 0 {
            parts.push(format!("缺文件 {} 个", self.broken));
        }
        if self.recovered > 0 {
            parts.push(format!("恢复 {} 个", self.recovered));
        }
        if parts.is_empty() {
            "注册表无变化".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

fn entry_from_spec(spec: &BuiltinSpec) -> EntryRecord {
    EntryRecord {
        id: spec.name.to_string(),
        name: spec.label.to_string(),
        source: source::BUILTIN.to_string(),
        path: String::new(),
        risk: spec.risk.to_string(),
        capabilities: spec.caps.iter().map(|c| c.to_string()).collect(),
        installed: true,
        enabled: true,
        status: status::OK.to_string(),
        note: String::new(),
    }
}

/// 构造保留的「未分配」组（三处初始化共用，避免字段漏填）。
fn make_unassigned_group() -> GroupRecord {
    GroupRecord {
        id: UNASSIGNED_GROUP.to_string(),
        name: "未分配".to_string(),
        description: String::new(),
        kind: "tools".to_string(),
        source: source::BUILTIN.to_string(),
        enabled: true,
        entries: Vec::new(),
    }
}


/// 扫描插件目录并转成快照（`registry list` / 装配 / CLI 共用）。
///
/// 只读操作：不加载 dll、不改注册表；合并由调用方决定。
pub fn scan_plugins(root: &std::path::Path) -> (ScanSnapshot, Vec<String>) {
    if !root.is_dir() {
        return (ScanSnapshot::default(), Vec::new());
    }
    let report = plugin_host::scan::scan(root);
    (snapshot_from_groups(&report.groups), report.problems)
}

/// 把已扫描到的组转成快照（装配路径用：宿主已经 scan 过了，不重复扫盘）。
pub fn snapshot_from_groups(groups: &[plugin_host::GroupEntry]) -> ScanSnapshot {
    let mut out = ScanSnapshot::default();
    for g in groups {
        out.groups.push(ScannedGroup {
            id: g.manifest.id.clone(),
            name: g.manifest.name.clone(),
            kind: if g.manifest.kind.is_empty() {
                "tools".to_string()
            } else {
                g.manifest.kind.clone()
            },
            source: source::PLUGIN.to_string(),
            entries: g
                .members
                .iter()
                .map(|m| {
                    let has_file = m.has_binary();
                    ScannedEntry {
                        id: m.id.clone(),
                        name: m.name.clone(),
                        risk: m.risk.clone(),
                        capabilities: if m.capabilities.is_empty() {
                            m.tags.clone()
                        } else {
                            m.capabilities.clone()
                        },
                        path: m
                            .dll_path()
                            .strip_prefix(&m.dir)
                            .unwrap_or(&m.dll_path())
                            .to_string_lossy()
                            .to_string(),
                        has_file,
                        note: if has_file {
                            String::new()
                        } else {
                            format!("缺 dll：{}", m.dll_path().display())
                        },
                    }
                })
                .collect(),
        });
    }
    out
}

/// 一次「主动扫描发现」的结果（GUI 的「扫描发现」按钮与启动日志共用）。
#[derive(Debug, Clone, Default)]
pub struct DiscoverReport {
    /// 裸 dll 收编 + 挂起搬迁补做的结果。
    pub adopt: plugin_host::AdoptReport,
    /// 注册表对账结果（缺文件 / 恢复计数）。
    pub merge: MergeReport,
    /// 扫描期问题（清单解析失败、目录名与 id 不符等）。
    pub problems: Vec<String>,
    /// 本次扫到的组数（磁盘事实，不是账本）。
    pub groups: usize,
    /// 本次扫到的条目数（磁盘事实，不是账本）。
    pub entries: usize,
    /// 磁盘上有、账本里没有的组 id（**只报告不落库**——装不装是人的决定）。
    pub new_groups: Vec<String>,
    /// 磁盘上有、账本里没有的条目数。
    pub new_entries: usize,
    /// 根目录下「放进去却扫不到」的目录名（缺 `PnC.toml` 或清单坏了）。
    /// 扫描对这类目录是静默跳过的，用户看到的是"点了没反应"——这里点名。
    pub skipped: Vec<String>,
    /// `[plugins] auto_probe = false`：裸 dll 没被收编（用户配置优先，不偷偷动文件）。
    pub probe_disabled: bool,
}

impl DiscoverReport {
    /// 一句话摘要（toast / 卡片里的「上次扫描」都用它）。
    pub fn describe(&self) -> String {
        let mut parts = vec![format!("扫到 {} 组 / {} 条", self.groups, self.entries)];
        if self.probe_disabled {
            parts.push("裸 dll 未收编（[plugins] auto_probe = false）".to_string());
        } else if !self.adopt.is_empty() {
            parts.push(self.adopt.describe());
        }
        if !self.new_groups.is_empty() {
            parts.push(format!(
                "新组 {}：{}",
                self.new_groups.len(),
                self.new_groups.join("、")
            ));
        }
        if self.new_entries > 0 {
            parts.push(format!("新条目 {} 个（待安装）", self.new_entries));
        }
        if self.merge.broken > 0 {
            parts.push(format!("缺文件 {} 个", self.merge.broken));
        }
        if self.merge.recovered > 0 {
            parts.push(format!("恢复 {} 个", self.merge.recovered));
        }
        if !self.problems.is_empty() {
            parts.push(format!("{} 个清单问题", self.problems.len()));
        }
        if !self.skipped.is_empty() {
            parts.push(format!(
                "{} 个目录被跳过（缺 PnC.toml）：{}",
                self.skipped.len(),
                self.skipped.join("、")
            ));
        }
        if parts.len() == 1 && self.new_groups.is_empty() && self.new_entries == 0 {
            parts.push("账本无变化".to_string());
        }
        parts.join(" · ")
    }

    /// 给用户看的详细版：摘要 + 每个问题一行（toast 只放摘要，详情进日志/卡片）。
    pub fn detail(&self) -> String {
        let mut s = self.describe();
        for p in &self.problems {
            s.push_str("\n· ");
            s.push_str(p);
        }
        for p in &self.adopt.problems {
            s.push_str("\n· ");
            s.push_str(p);
        }
        s
    }
}

/// **主动扫描发现**：自举 → 扫描 → 对账，三步一条链（GUI 一个按钮，不用 CLI/重启）。
///
/// 顺序不可换：
/// 1. `bootstrap` 先跑 —— 扫描只认目录，裸 dll 躺在根目录下永远成不了候选；
///    同时补做上次因 dll 被占用而挂起的换组搬迁，避免"账上说在 B、文件还在 A"。
/// 2. `scan_plugins` 只读扫盘，产出快照。
/// 3. `reconcile` 把已装条目的健康位刷新（缺文件/恢复），候选**不落库**。
///
/// 调用方负责 `save()` 与重新装配：本函数不写注册表文件，也不加载任何 dll。
pub fn discover(root: &Path, auto_probe: bool, reg: &mut ToolRegistry) -> DiscoverReport {
    let mut rep = DiscoverReport::default();
    if root.is_dir() {
        rep.probe_disabled = !auto_probe;
        rep.adopt = plugin_host::bootstrap(root, auto_probe);
        rep.skipped = skipped_dirs(root);
    }
    let (snap, problems) = scan_plugins(root);
    rep.problems = problems;
    rep.groups = snap.groups.len();
    rep.entries = snap.groups.iter().map(|g| g.entries.len()).sum();
    rep.new_groups = snap
        .groups
        .iter()
        .filter(|g| reg.group(&g.id).is_none())
        .map(|g| g.id.clone())
        .collect();
    rep.new_entries = snap
        .groups
        .iter()
        .flat_map(|g| g.entries.iter())
        .filter(|e| reg.entry(&e.id).is_none())
        .count();
    rep.merge = reg.reconcile(&snap);
    rep
}

/// 扫描器会跳过的目录名（`.`／`_` 开头是用户素材约定，`target` 是构建产物）。
fn scan_visible_dir(name: &str) -> bool {
    !name.starts_with('.') && !name.starts_with('_') && name != "target"
}

/// 根目录下**可见但读不出组清单**的目录：这些目录扫描时会静默跳过。
///
/// 最常见的踩坑：用户在 `plugins/` 里新建一个文件夹、把 dll 丢进去，却没写
/// `PnC.toml`——扫描看不到任何东西，界面显示"0 组"，用户以为功能坏了。
fn skipped_dirs(root: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| scan_visible_dir(&e.file_name().to_string_lossy()))
        .filter(|e| plugin_host::manifest::load_group(&e.path()).is_err())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// 确保 `plugins/<id>/` 与其中的 `PnC.toml` 存在（转发插件宿主，UI 只需要这一个入口）。
///
/// 新建组时调用：用户只输入组名，目录和清单都是自动的——否则"建了组却往里放不了
/// dll"（目录不存在），或者"目录在但扫描看不见"（缺清单）。
pub fn ensure_group_dir(
    root: &Path,
    id: &str,
    name: &str,
    description: &str,
) -> Result<PathBuf, String> {
    plugin_host::ensure_group_dir(root, id, name, description)
}

/// 换组的**文件侧**：把 `plugins/<旧组>/<插件>/` 搬到目标组，返回给 UI 的一句话。
///
/// 账与盘必须一起动。只改账的话，扫描仍按目录判定归属，加载器拿到的还是旧路径——
/// 表现为"换了组但工具照旧、卸载时找不到文件"。搬不动（dll 被占用）时本函数
/// 会登记 `.pending-moves.json`，下次启动由 `bootstrap` 补做，返回的说明里会写明。
pub fn relocate_plugin(root: &Path, tool: &str, to_group: &str) -> String {
    if !root.is_dir() {
        return String::new();
    }
    let report = plugin_host::scan::scan(root);
    plugin_host::relocate(root, &report, tool, to_group).note()
}

/// 注册表落点：`<data>/registry.toml`。
///
/// **唯一一份，全局共享**。注册表是「组 ⇄ 插件」的管理树，与领域无关：
/// 组和插件的装/卸/开关是全局状态，领域只**引用**组（`domain.toml` 的 `groups`）。
///
/// 历史：旧版曾按领域分账（`<data>/registries/<domain>/registry.toml`），
/// 导致"领域 A 装的插件在领域 B 看不见 / 没装却在跑"。旧账本由
/// [`consolidate`] 一次性合并进这里，之后不再读那套目录。
pub fn registry_path(data_root: &Path) -> PathBuf {
    data_root.join("registry.toml")
}

/// 账本合并结果（`consolidate` 的产物，供启动日志与 UI 提示用）。
#[derive(Debug, Clone, Default)]
pub struct ConsolidateReport {
    /// 合并了哪些旧账本（目录名）。
    pub ledgers: Vec<String>,
    /// 从旧账本里整组带进来的组 id。
    pub groups_added: Vec<String>,
    /// 与全局已有组合并的组 id。
    pub groups_merged: Vec<String>,
    /// 状态冲突（同名组在两处开关不一致）的说明。
    pub conflicts: Vec<String>,
    /// 旧目录归档到哪里（`registries.migrated-<ts>`）。
    pub archived: Option<PathBuf>,
}

impl ConsolidateReport {
    pub fn is_empty(&self) -> bool {
        self.ledgers.is_empty()
    }

    pub fn describe(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut s = format!(
            "已合并 {} 份旧领域账本 → 全局注册表（新增组 {} · 合并组 {}）",
            self.ledgers.len(),
            self.groups_added.len(),
            self.groups_merged.len()
        );
        if !self.conflicts.is_empty() {
            s.push_str(&format!("；{} 处状态冲突按「任一启用即启用」处理", self.conflicts.len()));
        }
        s
    }
}

/// 把旧版「每领域一份」的注册表合并进全局账本，并归档旧目录。
///
/// 幂等：`<data>/registries/` 不存在就什么都不做（合并后被改名，不会重复跑）。
///
/// 冲突裁决（宁可多启用一次，也不静默卸载）：
/// - 组 `enabled`：任一处启用即启用
/// - 条目 `installed` / `enabled`：任一为真即为真
/// - 条目 `status`：取健康度最高的一档（ok > discovered > broken）
pub fn consolidate(data_root: &Path) -> Result<ConsolidateReport, String> {
    let mut rep = ConsolidateReport::default();
    let reg_dir = data_root.join("registries");
    if !reg_dir.is_dir() {
        return Ok(rep);
    }

    // 收集旧账本：<registries>/<domain>/registry.toml
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&reg_dir) {
        for ent in rd.flatten() {
            let dir = ent.path();
            if !dir.is_dir() {
                continue;
            }
            let p = dir.join("registry.toml");
            if p.is_file() {
                let name = dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                paths.push((name, p));
            }
        }
    }
    if paths.is_empty() {
        // 目录在但没有账本：也归档，避免下次再扫
        archive_registries(&reg_dir, &mut rep);
        return Ok(rep);
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0));

    let dst_path = registry_path(data_root);
    let (mut dst, _) = ToolRegistry::load_or_init(&dst_path)?;
    let before = toml::to_string_pretty(&dst).unwrap_or_default();
    if dst_path.is_file() {
        let bak = dst_path.with_extension("toml.bak");
        let _ = std::fs::copy(&dst_path, &bak);
    }

    for (name, p) in &paths {
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                rep.conflicts.push(format!("{name}: 读取失败（{e}）"));
                continue;
            }
        };
        let src: ToolRegistry = match toml::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                rep.conflicts.push(format!("{name}: 解析失败（{e}）"));
                continue;
            }
        };
        rep.ledgers.push(name.clone());
        for g in &src.groups {
            match dst.groups.iter_mut().find(|d| d.id == g.id) {
                Some(d) => {
                    rep.groups_merged.push(g.id.clone());
                    if d.enabled != g.enabled {
                        rep.conflicts.push(format!(
                            "组 {} 启用状态不一致（全局={} / {name}={}）",
                            g.id, d.enabled, g.enabled
                        ));
                        d.enabled = d.enabled || g.enabled;
                    }
                    for e in &g.entries {
                        match d.entries.iter_mut().find(|x| x.id == e.id) {
                            Some(te) => {
                                te.installed = te.installed || e.installed;
                                te.enabled = te.enabled || e.enabled;
                                if status_rank(&e.status) > status_rank(&te.status) {
                                    te.status = e.status.clone();
                                }
                                if te.note.is_empty() {
                                    te.note = e.note.clone();
                                }
                            }
                            None => d.entries.push(e.clone()),
                        }
                    }
                }
                None => {
                    rep.groups_added.push(g.id.clone());
                    dst.groups.push(g.clone());
                }
            }
        }
    }

    // 内置条目归位 + 保留组补齐，避免合并后同一工具出现两份
    let mut changed = dst.rehome_builtins();
    changed |= dst.ensure_builtins();
    // 未分配组始终在末尾
    if let Some(pos) = dst.groups.iter().position(|g| g.id == UNASSIGNED_GROUP) {
        if pos != dst.groups.len() - 1 {
            let g = dst.groups.remove(pos);
            dst.groups.push(g);
            changed = true;
        }
    }
    let after = toml::to_string_pretty(&dst).unwrap_or_default();
    if changed || after != before {
        dst.save(&dst_path)?;
    }

    archive_registries(&reg_dir, &mut rep);
    Ok(rep)
}

fn status_rank(s: &str) -> u8 {
    match s {
        status::OK => 3,
        status::DISCOVERED => 2,
        status::BROKEN => 1,
        _ => 0,
    }
}

/// 把旧 `registries/` 目录改名留档；失败只记冲突，不影响装配。
fn archive_registries(reg_dir: &Path, rep: &mut ConsolidateReport) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dst = reg_dir.with_file_name(format!("registries.migrated-{ts}"));
    match std::fs::rename(reg_dir, &dst) {
        Ok(()) => rep.archived = Some(dst),
        Err(e) => rep.conflicts.push(format!("旧账本目录归档失败（{e}），原目录保留")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wpa-reg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 构造只含一个扫描条目的快照，供安装/分配相关测试复用。
    fn plugin_snapshot(group_id: &str, entry_id: &str, has_file: bool) -> ScanSnapshot {
        ScanSnapshot {
            groups: vec![ScannedGroup {
                id: group_id.to_string(),
                name: group_id.to_string(),
                kind: "tools".into(),
                source: source::PLUGIN.into(),
                entries: vec![ScannedEntry {
                    id: entry_id.to_string(),
                    name: entry_id.to_string(),
                    risk: "low".into(),
                    capabilities: vec![],
                    path: format!("{entry_id}.dll"),
                    has_file,
                    note: String::new(),
                }],
            }],
        }
    }

    /// 测试辅助：把快照里的候选全部安装。
    ///
    /// 新语义下扫描**不落库**（账本只记已安装），所以"装好之后的行为"类测试
    /// 必须先对账再显式安装一遍。
    fn install_all(reg: &mut ToolRegistry, snap: &ScanSnapshot) -> usize {
        reg.reconcile(snap);
        let mut n = 0;
        for g in &snap.groups {
            for e in &g.entries {
                if reg.entry(&e.id).is_none() && reg.install_candidate(&g.id, &g.source, e).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn from_catalog_covers_all_builtins() {
        let reg = ToolRegistry::from_catalog();
        // 内置能力组 + 末尾一个保留的「未分配」组
        assert_eq!(reg.groups.len(), catalog::CAP_GROUPS.len() + 1);
        assert_eq!(reg.groups.last().unwrap().id, UNASSIGNED_GROUP);
        for spec in catalog::BUILTIN_SPECS {
            let (g, e) = reg.entry(spec.name).unwrap_or_else(|| panic!("缺 {}", spec.name));
            assert_eq!(g.id, spec.group);
            assert!(e.installed && e.enabled, "{} 初始应可用", spec.name);
            assert_eq!(e.status, status::OK);
        }
        let st = reg.stats();
        assert_eq!(st.active, catalog::BUILTIN_SPECS.len());
    }

    #[test]
    fn roundtrip_preserves_user_edits() {
        let d = tmpdir("rt");
        let p = registry_path(&d);
        let (mut reg, created) = ToolRegistry::load_or_init(&p).unwrap();
        assert!(created);
        // 用户关掉 shell.run 与 shell 组
        assert!(reg.set_entry_enabled("shell.run", false));
        assert!(reg.set_group_enabled("shell", false));
        reg.save(&p).unwrap();

        let (re, created2) = ToolRegistry::load_or_init(&p).unwrap();
        assert!(!created2);
        assert!(!re.is_active("shell.run"));
        assert!(!re.is_active("shell.jobs"), "组关了，组内条目一律不生效");
        assert!(re.is_active("file.read"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn scan_does_not_write_candidates() {
        let mut reg = ToolRegistry::from_catalog();
        let snap = ScanSnapshot {
            groups: vec![ScannedGroup {
                id: "file".into(),
                name: "文件（插件版）".into(),
                kind: "tools".into(),
                source: source::PLUGIN.into(),
                entries: vec![ScannedEntry {
                    id: "file.copy".into(),
                    name: "复制文件".into(),
                    risk: "low".into(),
                    capabilities: vec![],
                    path: "file/copy/file_copy.dll".into(),
                    has_file: true,
                    note: String::new(),
                }],
            }],
        };
        let rep = reg.reconcile(&snap);
        assert_eq!(rep.new_entries, 0, "候选不落库");
        assert_eq!(rep.new_groups, 0, "扫描到组也不自动建组");
        assert!(
            reg.entry("file.copy").is_none(),
            "扫描到不等于安装：账本里不能有它"
        );
        // 注意：不能用 is_active 判断——它对"查不到"默认放行（为运行时工具留口子），
        // 插件装配用的是 is_installed && is_active 双闸门。
        assert!(!reg.is_installed("file.copy"));

        // 安装候选：由调用方把扫描结果传进来
        let se = &snap.groups[0].entries[0];
        reg.install_candidate("file", source::PLUGIN, se).unwrap();
        assert!(reg.is_installed("file.copy"));
        assert!(reg.is_active("file.copy"), "安装后才进工具面");
    }

    #[test]
    fn vanished_entry_becomes_broken_not_deleted() {
        let mut reg = ToolRegistry::from_catalog();
        let with_file = ScanSnapshot {
            groups: vec![ScannedGroup {
                id: "file".into(),
                name: "文件".into(),
                kind: "tools".into(),
                source: source::PLUGIN.into(),
                entries: vec![ScannedEntry {
                    id: "file.copy".into(),
                    name: "复制".into(),
                    risk: "low".into(),
                    capabilities: vec![],
                    path: "file/copy.dll".into(),
                    has_file: true,
                    note: String::new(),
                }],
            }],
        };
        assert_eq!(install_all(&mut reg, &with_file), 1);
        assert!(reg.is_active("file.copy"));

        // 文件被删：扫描不到该条目
        let empty = ScanSnapshot::default();
        let rep = reg.reconcile(&empty);
        assert!(rep.broken >= 1);
        let (_, e) = reg.entry("file.copy").unwrap();
        assert_eq!(e.status, status::BROKEN, "条目必须保留并标 broken");
        assert!(e.installed, "安装标记不因文件丢失而回退");
        assert!(!reg.is_active("file.copy"), "broken 不进工具面");
    }

    #[test]
    fn uninstall_removes_entries_and_candidate_returns() {
        let mut reg = ToolRegistry::from_catalog();
        assert_eq!(install_all(&mut reg, &plugin_snapshot("file", "file.demo", true)), 1);
        assert!(reg.is_active("file.demo"));

        let n = reg.uninstall_group("file");
        assert_eq!(n, 1);
        assert!(
            reg.entry("file.demo").is_none(),
            "卸载即从账本删除（候选会从扫描结果回来）"
        );
        assert!(!reg.is_installed("file.demo"));
        // 内置条目不受卸载影响（由代码提供，只能禁用）
        assert!(reg.entry("file.read").is_some());
    }

    #[test]
    fn unknown_tool_is_not_blocked() {
        // 运行时注册的工具（plan.* / subagent.spawn）不在注册表里，不能被拦
        let reg = ToolRegistry::from_catalog();
        assert!(reg.is_active("plan.create"));
        assert!(reg.is_active("subagent.spawn"));
    }

    #[test]
    fn writes_valid_toml_with_nested_entries() {
        let d = tmpdir("toml");
        let p = registry_path(&d);
        let reg = ToolRegistry::from_catalog();
        reg.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[[group]]"), "应是 group 数组表：{text}");
        assert!(text.contains("[[group.entry]]"), "条目应是组内嵌套表：{text}");
        let back: ToolRegistry = toml::from_str(&text).unwrap();
        assert_eq!(back.stats().active, reg.stats().active);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn move_entry_changes_group_without_touching_flags() {
        let mut reg = ToolRegistry::from_catalog();
        // 用插件条目测移动：内置工具不允许换组（见 builtin_tool_cannot_change_group）
        assert_eq!(install_all(&mut reg, &plugin_snapshot("file", "file.demo", true)), 1);
        assert!(reg.set_entry_enabled("file.demo", false));
        let (from, to) = reg.move_entry("file.demo", "shell").unwrap();
        assert_eq!((from.as_str(), to.as_str()), ("file", "shell"));
        let (g, e) = reg.entry("file.demo").unwrap();
        assert_eq!(g.id, "shell");
        assert!(!e.enabled, "移动不该重置禁用状态");
        assert!(reg.group("file").unwrap().entries.iter().all(|x| x.id != "file.demo"));
        // 目标组不存在时新建
        let (_, to) = reg.move_entry("file.demo", "tmp_group").unwrap();
        assert_eq!(to, "tmp_group");
        assert!(reg.group("tmp_group").is_some());
    }

    /// 移动过的条目不能被下一轮扫描拉回原组（否则 move 功能等于没用）。
    #[test]
    fn scan_does_not_undo_move() {
        let snap = ScanSnapshot {
            groups: vec![ScannedGroup {
                id: "file".into(),
                name: "文件".into(),
                kind: "tools".into(),
                source: source::PLUGIN.into(),
                entries: vec![ScannedEntry {
                    id: "file.demo".into(),
                    name: "演示".into(),
                    risk: "low".into(),
                    capabilities: vec![],
                    path: "demo.dll".into(),
                    has_file: true,
                    note: String::new(),
                }],
            }],
        };
        let mut reg = ToolRegistry::from_catalog();
        assert_eq!(install_all(&mut reg, &snap), 1);
        assert_eq!(reg.entry("file.demo").unwrap().0.id, "file");

        reg.move_entry("file.demo", "shell").unwrap();
        assert_eq!(reg.entry("file.demo").unwrap().0.id, "shell");

        let rep = reg.reconcile(&snap);
        let (g, e) = reg.entry("file.demo").unwrap();
        assert_eq!(g.id, "shell", "扫描不得把条目拉回原组");
        assert_eq!(e.status, status::OK, "文件还在，状态应保持 ok");
        assert_eq!(rep.new_entries, 0, "不该在 file 组里再建一份");
    }

    // ---------------------------------------------------------------- 契约 A

    #[test]
    fn unassigned_group_is_initialized_last_and_empty() {
        let reg = ToolRegistry::from_catalog();
        let g = reg.groups.last().unwrap();
        assert_eq!(g.id, UNASSIGNED_GROUP);
        assert_eq!(g.name, "未分配");
        assert_eq!(g.kind, "tools");
        assert_eq!(g.source, source::BUILTIN);
        assert!(g.enabled);
        assert!(g.entries.is_empty());

        // 旧注册表（没有该组）升级时补齐，且不会重复添加
        let mut old = ToolRegistry::default();
        assert!(old.ensure_builtins());
        assert_eq!(old.groups.last().unwrap().id, UNASSIGNED_GROUP);
        assert!(!old.ensure_builtins(), "补齐后二次调用应无改动");
        assert_eq!(old.groups.iter().filter(|g| g.id == UNASSIGNED_GROUP).count(), 1);
    }

    #[test]
    fn new_scanned_entries_stay_as_candidates() {
        let snap = plugin_snapshot("newplug", "newplug.hello", true);
        let mut reg = ToolRegistry::from_catalog();
        let rep = reg.reconcile(&snap);
        assert_eq!(rep.new_groups, 0, "扫描不自动建组");
        assert_eq!(rep.new_entries, 0, "扫描不落库");
        assert!(reg.entry("newplug.hello").is_none(), "候选不在注册表里");
        assert!(reg.group("newplug").is_none(), "组要等安装时才创建");

        // 显式安装：落成正式条目，组按需建出来
        let se = &snap.groups[0].entries[0];
        reg.install_candidate("newplug", source::PLUGIN, se).unwrap();
        let (g, e) = reg.entry("newplug.hello").unwrap();
        assert_eq!(g.id, "newplug");
        assert_eq!(e.status, status::OK);
        assert!(e.installed && e.enabled);
    }

    // ---------------------------------------------------------------- 契约 B

    #[test]
    fn group_crud_add_rename_describe_and_remove() {
        let mut reg = ToolRegistry::from_catalog();
        reg.add_group("tools", "我的工具", "原始描述").unwrap();
        {
            let g = reg.group("tools").unwrap();
            assert_eq!(g.name, "我的工具");
            assert_eq!(g.description, "原始描述");
            assert_eq!(g.source, source::PLUGIN);
        }

        reg.rename_group("tools", "改名后").unwrap();
        assert_eq!(reg.group("tools").unwrap().name, "改名后");
        reg.set_group_description("tools", "新描述").unwrap();
        assert_eq!(reg.group("tools").unwrap().description, "新描述");

        // 删除组：组内条目全部落到未分配组（用插件条目：内置工具不可换组）
        assert_eq!(install_all(&mut reg, &plugin_snapshot("plug", "plug.c", true)), 1);
        reg.move_entry("plug.c", "tools").unwrap();
        let n = reg.remove_group("tools").unwrap();
        assert_eq!(n, 1);
        assert!(reg.group("tools").is_none());
        let (g, _) = reg.entry("plug.c").unwrap();
        assert_eq!(g.id, UNASSIGNED_GROUP);
    }

    #[test]
    fn group_crud_rejects_invalid_args_and_reserved_group() {
        let mut reg = ToolRegistry::from_catalog();
        assert!(reg.add_group("", "名字", "").unwrap_err().contains("不能为空"));
        assert!(reg.add_group("id", "", "").unwrap_err().contains("不能为空"));
        assert!(reg.add_group(UNASSIGNED_GROUP, "未分配", "").unwrap_err().contains("保留"));
        assert!(reg.add_group("file", "重复", "").unwrap_err().contains("已存在"));

        assert!(reg.rename_group(UNASSIGNED_GROUP, "x").is_err());
        assert!(reg.rename_group("nope", "x").unwrap_err().contains("不存在"));
        assert!(reg.set_group_description("nope", "x").is_err());
        assert!(reg.remove_group(UNASSIGNED_GROUP).unwrap_err().contains("保留"));
        assert!(reg.remove_group("nope").is_err());
    }

    // ---------------------------------------------------------------- 契约 C

    #[test]
    fn install_and_uninstall_single_entry() {
        let mut reg = ToolRegistry::from_catalog();
        let snap = plugin_snapshot("plug", "plug.a", true);
        // 候选不在账本里 → 直接 install_entry 找不到它（必须先 install_candidate）
        assert!(reg.install_entry("plug.a").is_err());

        let se = &snap.groups[0].entries[0];
        reg.install_candidate("plug", source::PLUGIN, se).unwrap();
        {
            let (g, e) = reg.entry("plug.a").unwrap();
            assert_eq!(g.id, "plug");
            assert!(e.installed && e.enabled);
            assert_eq!(e.status, status::OK);
        }
        // 同一个工具只能在一个组：重复安装要明确拒绝
        let err = reg.install_candidate("plug", source::PLUGIN, se).unwrap_err();
        assert!(err.contains("已经装在"), "{err}");

        reg.uninstall_entry("plug.a").unwrap();
        assert!(reg.entry("plug.a").is_none(), "卸载 = 从账本删除");
        assert!(!reg.is_installed("plug.a"));

        assert!(reg.install_entry("nope").is_err());
        assert!(reg.uninstall_entry("nope").is_err());
    }

    #[test]
    fn install_candidate_rejects_broken_and_marks_unassigned_disabled() {
        let mut reg = ToolRegistry::from_catalog();
        let broken = plugin_snapshot("plug", "plug.broken", false);
        let err = reg
            .install_candidate("plug", source::PLUGIN, &broken.groups[0].entries[0])
            .unwrap_err();
        assert!(err.contains("文件缺失"), "{err}");

        // 装进「未分配」：可以装（之后才能移动），但不可启用
        let ok = plugin_snapshot("plug2", "plug.ok", true);
        reg.install_candidate(UNASSIGNED_GROUP, source::PLUGIN, &ok.groups[0].entries[0])
            .unwrap();
        let (g, e) = reg.entry("plug.ok").unwrap();
        assert_eq!(g.id, UNASSIGNED_GROUP);
        assert!(e.installed, "未分配可以安装");
        assert!(!e.enabled, "但不可启用");
    }

    #[test]
    fn unassigned_entry_cannot_be_enabled() {
        let mut reg = ToolRegistry::from_catalog();
        // 内置工具进不了未分配（固定组），这里用插件条目验"未分配不可启用"
        let snap = plugin_snapshot("plug", "plug.u", true);
        reg.install_candidate("plug", source::PLUGIN, &snap.groups[0].entries[0])
            .unwrap();
        reg.assign_entry("plug.u", UNASSIGNED_GROUP).unwrap();
        assert_eq!(reg.entry("plug.u").unwrap().0.id, UNASSIGNED_GROUP);
        assert!(!reg.set_entry_enabled("plug.u", true), "未分配条目不可启用");
        assert!(!reg.entry("plug.u").unwrap().1.enabled);

        // 普通组的启停不受影响
        assert!(reg.set_entry_enabled("file.write", false));
        assert!(reg.set_entry_enabled("file.write", true));
    }

    // ---------------------------------------------------------------- 契约 D

    #[test]
    fn assign_entry_moves_and_updates_enabled() {
        let mut reg = ToolRegistry::from_catalog();
        reg.add_group("tools", "我的工具", "").unwrap();

        // 已安装条目分配到普通组 → 自动启用（用插件条目：内置工具不可换组）
        let sx = plugin_snapshot("plug", "plug.x", true);
        reg.install_candidate("plug", source::PLUGIN, &sx.groups[0].entries[0])
            .unwrap();
        reg.assign_entry("plug.x", "tools").unwrap();
        let (g, e) = reg.entry("plug.x").unwrap();
        assert_eq!(g.id, "tools");
        assert!(e.installed && e.enabled);

        // 内置工具换组 → 明确报"内置工具"，且原地不动
        let err = reg.assign_entry("file.read", "tools").unwrap_err();
        assert!(err.contains("内置工具"), "{err}");
        assert_eq!(reg.entry("file.read").unwrap().0.id, "file");

        // 目标组不存在 → 中文错误，且不移动条目
        assert!(reg.assign_entry("plug.x", "nope").unwrap_err().contains("不存在"));
        assert_eq!(reg.entry("plug.x").unwrap().0.id, "tools");

        // 未安装的候选不能分配：账本里没有它，也就谈不上"归属"
        let sy = plugin_snapshot("plug", "plug.y", true);
        reg.reconcile(&sy);
        assert!(reg.assign_entry("plug.y", "tools").unwrap_err().contains("不存在"));

        // 先装进未分配（已安装、未启用），再分配到普通组 → 启用
        reg.install_candidate(UNASSIGNED_GROUP, source::PLUGIN, &sy.groups[0].entries[0])
            .unwrap();
        let (g, e) = reg.entry("plug.y").unwrap();
        assert_eq!(g.id, UNASSIGNED_GROUP);
        assert!(e.installed && !e.enabled, "未分配装得进去，但不能启用");

        reg.assign_entry("plug.y", "tools").unwrap();
        let (_, e) = reg.entry("plug.y").unwrap();
        assert!(e.enabled, "分配到普通组后自动启用");
    }

    // ------------------------------------------------- 全局账本 / 内置固定组（r3）

    #[test]
    fn builtin_tool_cannot_change_group() {
        let mut reg = ToolRegistry::from_catalog();
        // assign_entry 是 UI 入口：给出明确原因
        let err = reg.assign_entry("file.read", "net").unwrap_err();
        assert!(err.contains("内置工具"), "{err}");
        // move_entry 是底层入口：同样拒绝，不能只靠上层拦
        assert!(reg.move_entry("file.read", UNASSIGNED_GROUP).is_none());
        assert_eq!(reg.entry("file.read").unwrap().0.id, "file");
        // 同组赋值是幂等操作，放行
        assert!(reg.assign_entry("file.read", "file").is_ok());
    }

    #[test]
    fn builtin_group_cannot_be_removed() {
        let mut reg = ToolRegistry::from_catalog();
        let err = reg.remove_group("file").unwrap_err();
        assert!(err.contains("内置组"), "{err}");
        assert!(reg.group("file").is_some(), "内置组不能被删掉");
        // 用户建的组照常可删
        reg.add_group("mygrp", "我的组", "").unwrap();
        assert!(reg.remove_group("mygrp").is_ok());
    }

    /// 旧数据里被挪走的内置工具，load 时必须归位且只留一份。
    #[test]
    fn rehome_builtins_pulls_stray_copy_back() {
        let mut reg = ToolRegistry::from_catalog();
        let gi = reg.groups.iter().position(|g| g.id == "file").unwrap();
        let ei = reg.groups[gi].entries.iter().position(|e| e.id == "file.read").unwrap();
        let mut stray = reg.groups[gi].entries.remove(ei);
        stray.enabled = false; // 用户曾禁用：归位后要保留这个设置
        reg.groups
            .iter_mut()
            .find(|g| g.id == "net")
            .unwrap()
            .entries
            .push(stray);

        assert!(reg.rehome_builtins(), "有副本被挪走，应报告改动");
        let owners: Vec<&str> = reg
            .groups
            .iter()
            .filter(|g| g.entries.iter().any(|e| e.id == "file.read"))
            .map(|g| g.id.as_str())
            .collect();
        assert_eq!(owners, vec!["file"], "内置条目必须唯一且在固定组");
        assert!(!reg.entry("file.read").unwrap().1.enabled, "归位应保留禁用状态");
        // 再跑一次应当无改动（幂等）
        assert!(!reg.rehome_builtins());
    }

    /// 旧版按领域分账的账本要能合并进全局，并归档旧目录。
    #[test]
    fn consolidate_merges_old_domain_ledgers() {
        let d = tmpdir("consolidate");
        let _ = ToolRegistry::load_or_init(&registry_path(&d)).unwrap();

        // 旧领域账本：多一个自定义组 recon，且把 file 组关掉（与全局冲突）
        let mut old = ToolRegistry::from_catalog();
        old.groups.iter_mut().find(|g| g.id == "file").unwrap().enabled = false;
        old.groups.push(GroupRecord {
            id: "recon".into(),
            name: "侦察".into(),
            description: String::new(),
            kind: "tools".into(),
            source: source::PLUGIN.into(),
            enabled: true,
            entries: vec![EntryRecord {
                id: "recon.whois".into(),
                name: "WHOIS".into(),
                source: source::PLUGIN.into(),
                path: "recon/whois".into(),
                risk: "low".into(),
                capabilities: vec![],
                installed: true,
                enabled: true,
                status: status::OK.into(),
                note: String::new(),
            }],
        });
        let dir = d.join("registries").join("pentest");
        std::fs::create_dir_all(&dir).unwrap();
        old.save(&dir.join("registry.toml")).unwrap();

        let rep = consolidate(&d).unwrap();
        assert_eq!(rep.ledgers, vec!["pentest"]);
        assert!(rep.groups_added.contains(&"recon".to_string()), "{:?}", rep.groups_added);

        let (merged, _) = ToolRegistry::load_or_init(&registry_path(&d)).unwrap();
        assert!(merged.group("recon").is_some(), "旧账本独有的组应带进全局");
        assert!(merged.is_active("recon.whois"));
        assert!(
            merged.group("file").unwrap().enabled,
            "冲突按「任一启用即启用」：不能被旧账本关掉"
        );
        assert!(!d.join("registries").exists(), "旧目录应被归档改名");
        assert!(rep.archived.as_ref().map(|p| p.exists()).unwrap_or(false));

        // 幂等：再跑一次什么都不做
        assert!(consolidate(&d).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 未分配组的 id 必须就是磁盘目录名，且不能被扫描跳过。
    ///
    /// 这条断言是跨 crate 的契约：`plugin-host` 不认识注册表，但它负责按目录名
    /// 建组。历史上 id 是 `__unassigned__`（带下划线），而扫描会跳过 `_` 开头的
    /// 目录 —— 结果是"未分配目录永远扫不到，往里放的 dll 凭空消失"。
    #[test]
    fn unassigned_id_matches_plugin_dir() {
        assert_eq!(UNASSIGNED_GROUP, plugin_host::adopt::UNASSIGNED_DIR);
        assert!(
            !UNASSIGNED_GROUP.starts_with('_') && !UNASSIGNED_GROUP.starts_with('.'),
            "扫描会跳过 _ / . 开头的目录，id 不能是那个形状"
        );
        assert!(!catalog::is_base_group(UNASSIGNED_GROUP), "未分配不是内置基础组");
    }

    #[test]
    fn slug_group_id_makes_dir_safe_names() {
        assert_eq!(slug_group_id("my tools"), "my_tools");
        assert_eq!(slug_group_id("  My-Tools!!  "), "my_tools");
        assert_eq!(slug_group_id("a  b"), "a_b", "连续分隔符收成一个");
        // 纯中文派生不出 ASCII：退化成 grp（再靠 unique 加序号）
        assert_eq!(slug_group_id("我的抓包工具"), "grp");
        // 数字开头加前缀，避免和"编号目录"混淆
        assert_eq!(slug_group_id("2fa"), "grp_2fa");
        // 超长名截断到 24，且不留尾部分隔符
        let long = slug_group_id(&"x".repeat(60));
        assert_eq!(long.len(), 24);
        assert!(!long.ends_with('_'));
        // 不能以 _ 或 . 开头（扫描会跳过）
        for n in ["_hidden", ".dot", "  ", "", "!!"] {
            let s = slug_group_id(n);
            assert!(!s.starts_with('_') && !s.starts_with('.'), "{n:?} -> {s}");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn unique_group_id_avoids_taken_and_reserved() {
        let mut taken = vec!["grp".to_string(), "file".to_string()];
        let a = unique_group_id("grp", |c| taken.iter().any(|x| x == c));
        assert_eq!(a, "grp_2");
        taken.push(a);
        let b = unique_group_id("grp", |c| taken.iter().any(|x| x == c));
        assert_eq!(b, "grp_3");
        // 与内置基础组同名也算占用
        assert_ne!(unique_group_id("core", |_| false), "core");
        // 未分配是保留 id
        assert_ne!(
            unique_group_id(UNASSIGNED_GROUP, |_| false),
            UNASSIGNED_GROUP
        );
    }

    #[test]
    fn migrate_renames_legacy_unassigned_id() {
        let d = std::env::temp_dir().join(format!("wpb_reg_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = registry_path(&d);

        // 造一份"旧版账本"：未分配组还叫 __unassigned__，里面躺着一个条目
        let mut old = ToolRegistry::from_catalog();
        for g in &mut old.groups {
            if g.id == UNASSIGNED_GROUP {
                g.id = "__unassigned__".to_string();
            }
        }
        old.group_mut("__unassigned__").unwrap().entries.push(EntryRecord {
            id: "plug.a".into(),
            name: "plug.a".into(),
            source: source::PLUGIN.into(),
            path: "plug.a.dll".into(),
            risk: "low".into(),
            capabilities: vec![],
            installed: true,
            enabled: false,
            status: status::OK.into(),
            note: String::new(),
        });
        old.save(&path).unwrap();

        let (reg, _) = ToolRegistry::load_or_init(&path).unwrap();
        assert!(reg.group("__unassigned__").is_none(), "旧 id 应已改名");
        let g = reg.group(UNASSIGNED_GROUP).expect("未分配组应存在");
        assert_eq!(g.entries.len(), 1, "条目要跟着一起搬，不能变孤儿");
        assert_eq!(g.entries[0].id, "plug.a");
        assert_eq!(
            reg.groups.iter().filter(|g| g.id == UNASSIGNED_GROUP).count(),
            1,
            "不能因为改名多出一个未分配组"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // ------------------------------------------------ 主动扫描发现（GUI 按钮）

    /// 造一个真实磁盘插件组：`<root>/<组>/PnC.toml` + 两个成员（一个带 dll、一个只有清单）。
    fn write_plugin_group(root: &Path) {
        let g = root.join("demo");
        std::fs::create_dir_all(g.join("ok")).unwrap();
        std::fs::create_dir_all(g.join("miss")).unwrap();
        std::fs::write(
            g.join("PnC.toml"),
            "[group]\nid = \"demo\"\nname = \"演示组\"\nversion = \"0.1.0\"\nkind = \"tools\"\n",
        )
        .unwrap();
        std::fs::write(
            g.join("ok").join("PnC.toml"),
            "[plugin]\nid = \"demo.ok\"\nname = \"有文件\"\nrisk = \"low\"\nbinary = \"ok.dll\"\n",
        )
        .unwrap();
        // 空文件就够：扫描只查文件在不在，不解析 dll
        std::fs::write(g.join("ok").join("ok.dll"), b"").unwrap();
        std::fs::write(
            g.join("miss").join("PnC.toml"),
            "[plugin]\nid = \"demo.miss\"\nname = \"缺文件\"\nrisk = \"low\"\nbinary = \"miss.dll\"\n",
        )
        .unwrap();
    }

    #[test]
    fn discover_reports_new_group_without_installing_it() {
        let d = tmpdir("disc");
        let root = d.join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        write_plugin_group(&root);
        // 新建文件夹却没写 PnC.toml：扫描静默跳过，必须点名，否则用户以为"点了没反应"
        std::fs::create_dir_all(root.join("no_manifest")).unwrap();

        let mut reg = ToolRegistry::from_catalog();
        let rep = discover(&root, true, &mut reg);

        assert!(rep.new_groups.contains(&"demo".to_string()), "{:?}", rep.new_groups);
        assert_eq!(rep.new_entries, 2, "两个成员都是候选");
        assert_eq!(rep.skipped, vec!["no_manifest".to_string()], "{:?}", rep.skipped);
        // 双闸门：发现 ≠ 安装，候选不落库
        assert!(reg.entry("demo.ok").is_none(), "扫描不能替用户决定安装");
        assert!(reg.group("demo").is_none(), "新组也不该自动进账本");
        assert!(rep.describe().contains("demo"), "{}", rep.describe());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discover_is_idempotent_and_marks_missing_files() {
        let d = tmpdir("disc2");
        let root = d.join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        write_plugin_group(&root);

        let mut reg = ToolRegistry::from_catalog();
        let first = discover(&root, true, &mut reg);
        assert_eq!(first.new_entries, 2);

        // 用户把能装的候选装进 demo 组后，删掉 dll 再重扫
        let (snap, _) = scan_plugins(&root);
        let installed = install_all(&mut reg, &snap);
        assert_eq!(installed, 1, "缺 dll 的候选装不了，只有齐备的那个能装");
        std::fs::remove_file(root.join("demo/ok/ok.dll")).unwrap();

        let second = discover(&root, true, &mut reg);
        assert!(second.new_groups.is_empty(), "{:?}", second.new_groups);
        assert_eq!(second.new_entries, 1, "装不了的候选还在，仍算未安装");
        assert_eq!(second.merge.broken, 1, "已装条目丢文件要对账成 broken");
        assert!(second.describe().contains("缺文件 1"), "{}", second.describe());
        // 幂等：第二次不该再报"新组"（账本已认识它）
        assert!(!second.describe().contains("新组"), "{}", second.describe());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn discover_respects_auto_probe_off() {
        let d = tmpdir("disc3");
        let root = d.join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        // 裸 dll 丢在根目录：auto_probe 关掉时必须原地不动，只提示
        let loose = root.join("loose.dll");
        std::fs::write(&loose, b"not a real dll").unwrap();

        let mut reg = ToolRegistry::from_catalog();
        let rep = discover(&root, false, &mut reg);
        assert!(rep.probe_disabled);
        assert!(rep.adopt.adopted.is_empty(), "{:?}", rep.adopt.adopted);
        assert!(loose.is_file(), "用户明确不让程序动插件目录，就不能搬");
        assert!(rep.describe().contains("auto_probe"), "{}", rep.describe());
        let _ = std::fs::remove_dir_all(&d);
    }
}
