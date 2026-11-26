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
pub const UNASSIGNED_GROUP: &str = "__unassigned__";

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
            // 版本升级时补齐新增的内置工具（不覆盖用户对已有条目的改动）
            let created = reg.ensure_builtins();
            if created {
                reg.save(path)?;
            }
            return Ok((reg, false));
        }
        let reg = Self::from_catalog();
        reg.save(path)?;
        Ok((reg, true))
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

    /// 卸载单个条目：回到 `discovered`，`enabled = false`，条目保留在原组。
    ///
    /// 已卸载时幂等返回 Ok；条目不存在时给中文错误。
    pub fn uninstall_entry(&mut self, tool: &str) -> Result<(), String> {
        for g in &mut self.groups {
            if let Some(e) = g.entries.iter_mut().find(|e| e.id == tool) {
                if !e.installed {
                    return Ok(());
                }
                e.installed = false;
                e.status = status::DISCOVERED.to_string();
                e.enabled = false;
                return Ok(());
            }
        }
        Err(format!("条目「{tool}」不存在"))
    }

    /// 安装一个组：把该组的候选条目全部转为已安装。
    ///
    /// 只对 `discovered` 生效——`broken` 的条目要等文件补回来（重新扫描后变 discovered/ok）。
    /// 返回实际安装的条目数。
    pub fn install_group(&mut self, id: &str) -> usize {
        let Some(g) = self.group_mut(id) else { return 0 };
        let mut n = 0;
        for e in &mut g.entries {
            if !e.installed && e.status == status::DISCOVERED {
                e.installed = true;
                // 安装即启用：用户点"安装"的期望是"能用了"，之后可单独关某条
                e.enabled = true;
                e.status = status::OK.to_string();
                n += 1;
            }
        }
        if n > 0 {
            g.enabled = true;
        }
        n
    }

    /// 卸载一个组：条目全部转为未安装（条目本身保留，便于重新安装）。
    ///
    /// **不动 `enabled`**：卸载再安装应该回到原来"哪些开着"的状态，
    /// 而不是把用户之前的选择一起抹掉。
    pub fn uninstall_group(&mut self, id: &str) -> usize {
        let Some(g) = self.group_mut(id) else { return 0 };
        let mut n = 0;
        for e in &mut g.entries {
            if e.installed {
                e.installed = false;
                if e.status == status::OK {
                    e.status = status::DISCOVERED.to_string();
                }
                n += 1;
            }
        }
        g.enabled = false;
        n
    }

    /// 与扫描结果合并：**扫描只更新状态，不改变用户的安装/启停决定**。
    ///
    /// - 扫描到 + 注册表有 + 文件在 ⇒ `ok`
    /// - 扫描到 + 注册表有 + 文件缺 ⇒ `broken`
    /// - 扫描到 + 注册表无 ⇒ 新组/新条目，`installed=false, status=discovered`
    /// - 注册表有（非内置）+ 扫描不到 ⇒ `broken`（条目保留，不消失）
    pub fn merge_scan(&mut self, snap: &ScanSnapshot) -> MergeReport {
        let mut report = MergeReport::default();
        // 只看条目 id（不分组）：用户可能已经把条目移到别的组，
        // 按"扫描组 + 条目"配对会把移动结果覆盖掉，等于移动功能失效。
        let mut seen: Vec<String> = Vec::new();

        for sg in &snap.groups {
            let gi = match self.groups.iter().position(|g| g.id == sg.id) {
                Some(i) => i,
                None => {
                    self.groups.push(GroupRecord {
                        id: sg.id.clone(),
                        name: sg.name.clone(),
                        description: String::new(),
                        kind: sg.kind.clone(),
                        source: if sg.source.is_empty() {
                            source::PLUGIN.to_string()
                        } else {
                            sg.source.clone()
                        },
                        enabled: false, // 新发现的组默认不启用，要用户显式安装
                        entries: Vec::new(),
                    });
                    report.new_groups += 1;
                    self.groups.len() - 1
                }
            };

            for se in &sg.entries {
                seen.push(se.id.clone());

                // 全局查找（跨组）：找到就更新事实位，**不改组归属**
                let found = self.groups.iter().enumerate().find_map(|(gi2, g)| {
                    g.entries
                        .iter()
                        .position(|e| e.id == se.id)
                        .map(|ei| (gi2, ei))
                });

                match found {
                    Some((gi2, ei2)) => {
                        let e = &mut self.groups[gi2].entries[ei2];
                        e.name = se.name.clone();
                        e.risk = se.risk.clone();
                        e.path = se.path.clone();
                        if !se.capabilities.is_empty() {
                            e.capabilities = se.capabilities.clone();
                        }
                        if se.has_file {
                            if e.installed && e.status == status::BROKEN {
                                report.recovered += 1;
                                e.note.clear();
                            }
                            if e.installed {
                                e.status = status::OK.to_string();
                            }
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
                    None => {
                        let has_file = se.has_file;
                        let mut e = EntryRecord {
                            id: se.id.clone(),
                            name: se.name.clone(),
                            source: source::PLUGIN.to_string(),
                            path: se.path.clone(),
                            risk: se.risk.clone(),
                            capabilities: se.capabilities.clone(),
                            installed: false,
                            enabled: false,
                            status: if has_file {
                                status::DISCOVERED.to_string()
                            } else {
                                status::BROKEN.to_string()
                            },
                            note: if has_file {
                                se.note.clone()
                            } else {
                                "文件缺失".to_string()
                            },
                        };
                        if !has_file {
                            e.note = "文件缺失".to_string();
                        }
                        self.groups[gi].entries.push(e);
                        report.new_entries += 1;
                    }
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

    /// 把条目移到另一个组（组↔条目绑定）。返回 `(from, to)`。
    ///
    /// 目标组不存在时新建（source 跟随条目来源）。移动**不改**条目的
    /// installed/enabled 状态——换归属不该重置用户的开关。
    pub fn move_entry(&mut self, tool: &str, to_group: &str) -> Option<(String, String)> {
        let (from_idx, ent_idx) = self.groups.iter().enumerate().find_map(|(gi, g)| {
            g.entries.iter().position(|e| e.id == tool).map(|ei| (gi, ei))
        })?;
        let from = self.groups[from_idx].id.clone();
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
        let installed = self
            .entry(tool)
            .ok_or_else(|| format!("条目「{tool}」不存在"))?
            .1
            .installed;

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

/// 注册表默认落点：`<data>/registry.toml`。
pub fn registry_path(data_root: &Path) -> PathBuf {
    data_root.join("registry.toml")
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
    fn discovered_group_is_not_installed_until_install() {
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
        let rep = reg.merge_scan(&snap);
        assert_eq!(rep.new_entries, 1);
        let (_, e) = reg.entry("file.copy").unwrap();
        assert!(!e.installed, "扫描到不等于安装");
        assert_eq!(e.status, status::DISCOVERED);
        assert!(!reg.is_active("file.copy"));

        assert_eq!(reg.install_group("file"), 1);
        {
            let (g, e) = reg.entry("file.copy").unwrap();
            eprintln!(
                "DEBUG group_enabled={} installed={} enabled={} status=[{}]",
                g.enabled, e.installed, e.enabled, e.status
            );
        }
        assert!(reg.is_active("file.copy"), "安装并启用后才进工具面");
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
        reg.merge_scan(&with_file);
        reg.install_group("file");
        assert!(reg.is_active("file.copy"));

        // 文件被删：扫描不到该条目
        let empty = ScanSnapshot::default();
        let rep = reg.merge_scan(&empty);
        assert!(rep.broken >= 1);
        let (_, e) = reg.entry("file.copy").unwrap();
        assert_eq!(e.status, status::BROKEN, "条目必须保留并标 broken");
        assert!(e.installed, "安装标记不因文件丢失而回退");
        assert!(!reg.is_active("file.copy"), "broken 不进工具面");
    }

    #[test]
    fn uninstall_keeps_entries_for_reinstall() {
        let mut reg = ToolRegistry::from_catalog();
        let n = reg.uninstall_group("file");
        assert!(n >= 2);
        assert!(!reg.is_active("file.read"));
        assert!(reg.entry("file.read").is_some(), "条目本身不删");
        reg.install_group("file");
        assert!(reg.is_active("file.read"));
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
        assert!(reg.set_entry_enabled("file.write", false));
        let (from, to) = reg.move_entry("file.write", "shell").unwrap();
        assert_eq!((from.as_str(), to.as_str()), ("file", "shell"));
        let (g, e) = reg.entry("file.write").unwrap();
        assert_eq!(g.id, "shell");
        assert!(!e.enabled, "移动不该重置禁用状态");
        assert!(reg.group("file").unwrap().entries.iter().all(|x| x.id != "file.write"));
        // 目标组不存在时新建
        let (_, to) = reg.move_entry("file.read", "tmp_group").unwrap();
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
        reg.merge_scan(&snap);
        reg.install_group("file");
        assert_eq!(reg.entry("file.demo").unwrap().0.id, "file");

        reg.move_entry("file.demo", "shell").unwrap();
        assert_eq!(reg.entry("file.demo").unwrap().0.id, "shell");

        let rep = reg.merge_scan(&snap);
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
    fn new_scanned_entries_are_discovered_in_their_group() {
        let snap = plugin_snapshot("newplug", "newplug.hello", true);
        let mut reg = ToolRegistry::from_catalog();
        let rep = reg.merge_scan(&snap);
        assert_eq!(rep.new_groups, 1);
        assert_eq!(rep.new_entries, 1);

        let (g, e) = reg.entry("newplug.hello").unwrap();
        assert_eq!(g.id, "newplug", "新组的新条目仍放原扫描组");
        assert_eq!(e.status, status::DISCOVERED, "未安装的新条目应标记为 discovered");
        assert!(!e.installed);
        assert!(!e.enabled);
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

        // 删除组：组内条目全部落到未分配组
        reg.move_entry("file.read", "tools").unwrap();
        let n = reg.remove_group("tools").unwrap();
        assert_eq!(n, 1);
        assert!(reg.group("tools").is_none());
        let (g, _) = reg.entry("file.read").unwrap();
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
        reg.merge_scan(&plugin_snapshot("plug", "plug.a", true));
        assert_eq!(reg.entry("plug.a").unwrap().1.status, status::DISCOVERED);

        reg.install_entry("plug.a").unwrap();
        {
            let (g, e) = reg.entry("plug.a").unwrap();
            assert_eq!(g.id, "plug");
            assert!(e.installed);
            assert_eq!(e.status, status::OK);
            assert!(e.enabled);
        }
        reg.install_entry("plug.a").unwrap(); // 幂等

        reg.uninstall_entry("plug.a").unwrap();
        {
            let (g, e) = reg.entry("plug.a").unwrap();
            assert_eq!(g.id, "plug", "卸载后条目仍留在原组");
            assert!(!e.installed);
            assert_eq!(e.status, status::DISCOVERED);
            assert!(!e.enabled);
        }
        reg.uninstall_entry("plug.a").unwrap(); // 幂等

        assert!(reg.install_entry("nope").is_err());
        assert!(reg.uninstall_entry("nope").is_err());
    }

    #[test]
    fn install_entry_rejects_broken_and_unassigned() {
        let mut reg = ToolRegistry::from_catalog();
        reg.merge_scan(&plugin_snapshot("plug", "plug.broken", false));
        assert_eq!(reg.entry("plug.broken").unwrap().1.status, status::BROKEN);
        assert!(reg.install_entry("plug.broken").unwrap_err().contains("文件缺失，无法安装"));

        reg.merge_scan(&plugin_snapshot("plug2", "plug.ok", true));
        reg.assign_entry("plug.ok", UNASSIGNED_GROUP).unwrap();
        assert!(reg.install_entry("plug.ok").unwrap_err().contains("请先分配到组再安装"));
    }

    #[test]
    fn unassigned_entry_cannot_be_enabled() {
        let mut reg = ToolRegistry::from_catalog();
        reg.assign_entry("file.read", UNASSIGNED_GROUP).unwrap();
        assert_eq!(reg.entry("file.read").unwrap().0.id, UNASSIGNED_GROUP);
        assert!(!reg.set_entry_enabled("file.read", true), "未分配条目不可启用");
        assert!(!reg.entry("file.read").unwrap().1.enabled);

        // 普通组的启停不受影响
        assert!(reg.set_entry_enabled("file.write", false));
        assert!(reg.set_entry_enabled("file.write", true));
    }

    // ---------------------------------------------------------------- 契约 D

    #[test]
    fn assign_entry_moves_and_updates_enabled() {
        let mut reg = ToolRegistry::from_catalog();
        reg.add_group("tools", "我的工具", "").unwrap();

        // 已安装条目分配到普通组 → 自动启用
        reg.assign_entry("file.read", "tools").unwrap();
        let (g, e) = reg.entry("file.read").unwrap();
        assert_eq!(g.id, "tools");
        assert!(e.installed && e.enabled);

        // 目标组不存在 → 中文错误，且不移动条目
        assert!(reg.assign_entry("file.write", "nope").unwrap_err().contains("不存在"));
        assert_eq!(reg.entry("file.write").unwrap().0.id, "file");

        // 未安装条目分配到普通组 → 保持未启用
        reg.merge_scan(&plugin_snapshot("plug", "plug.y", true));
        reg.assign_entry("plug.y", "tools").unwrap();
        let (_, e) = reg.entry("plug.y").unwrap();
        assert!(!e.installed);
        assert!(!e.enabled);

        // 安装后分配到未分配组 → 关闭启用
        reg.install_entry("plug.y").unwrap();
        assert!(reg.entry("plug.y").unwrap().1.enabled);
        reg.assign_entry("plug.y", UNASSIGNED_GROUP).unwrap();
        let (g, e) = reg.entry("plug.y").unwrap();
        assert_eq!(g.id, UNASSIGNED_GROUP);
        assert!(!e.enabled);
    }
}
