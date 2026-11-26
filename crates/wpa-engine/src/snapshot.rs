//! UI 数据契约（UiSnapshot）：引擎 → 界面 的**只读**视图。
//!
//! 界面只消费这些结构，不直接触碰注册表/存储，保证 UI 与引擎解耦：
//! 以后换 UI 技术栈或加管理端点都只改这一层。

use serde::Serialize;

/// 单个工具行（工具页 / 树包页）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolRow {
    pub name: String,
    /// 归属组 id；`base` = 基座通用工具。
    pub group: String,
    /// 风险档位（展示用字符串，如 `High`）。
    pub risk: String,
    pub description: String,
}

/// 单个空间行（空间页）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SpaceRow {
    pub name: String,
    pub active: bool,
    /// 授权目标条数
    pub scope: usize,
    pub cwd: String,
    pub trees: Vec<String>,
    /// 授权目标明细（空间页可编辑）
    pub authorized: Vec<String>,
    /// 工具白名单明细（组 id / 工具名）
    pub whitelist: Vec<String>,
    /// 审批三态：global / auto / ask
    pub approval: String,
    /// 沙箱三态：global / strict / relaxed
    pub sandbox: String,
    /// 工具白名单条数（0 = 全部）
    pub plugins: usize,
    pub skills: usize,
    pub note: String,
}

/// 单个模型档案行（模型页）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelRow {
    pub name: String,
    pub model: String,
    pub base_url: String,
    /// 上下文窗口（tokens）
    pub context_window: u32,
    /// 是否支持 function calling
    pub tools_ok: bool,
    pub vision: bool,
    /// 是否当前生效档案
    pub active: bool,
    pub enabled: bool,
    /// 是否在 fallback 链上
    pub fallback: bool,
}

/// 单个技能行（技能页：名字 + 触发场景 + 所属领域包）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SkillRow {
    pub name: String,
    pub trigger: String,
    /// 所属包 id（`domain`；空 = 未知）
    pub domain: String,
}

/// 单个领域行（领域页：列表 + 组引用 + 工具面预览）。
///
/// 与 `domain::DomainRow` 字段一致，但这里**只保留 UI 需要的**：`error` 变成
/// 可直接显示的一句话，组引用已展开成名字列表（UI 不做 `groups` 计数推测）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DomainCardRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub summary: String,
    /// 该领域声明的组 id（领域页的组引用 chip）
    pub groups: Vec<String>,
    /// 该领域最终装配出来的工具数（0 且 groups 非空 = 组名写错了）
    pub tools: usize,
    pub knowledge_entries: usize,
    pub has_workflow: bool,
    pub active: bool,
    /// 装载失败原因（空 = 正常）
    pub error: String,
}

/// 按领域声明的组，从工具面里数出"该领域会装配出多少工具"。
///
/// 与 `build_session` 的组展开规则一致（组 → `catalog::TOOL_GROUP` 里的工具）。
/// 抽成自由函数是为了让领域页在**不重建会话**的前提下预览工具面。
pub fn tool_surface(tools: &[ToolRow], group_ids: &[String]) -> usize {
    tools
        .iter()
        .filter(|t| group_ids.iter().any(|g| g == &t.group))
        .count()
}

/// 能力页左栏的一行：一个**基座能力组**（不是插件契约组）。
///
/// 为什么要单独一个结构：`GroupRow` 来自契约群组（插件装出来的），
/// 纯基座模式下它是空的；而能力页要展示的是 `catalog::CAP_GROUPS` 那 8 组
/// ——它们永远存在，只是可能有 0 个工具。"组存在"和"组里有东西"是两件事，
/// 混成一个列表会让用户以为"没有组"。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CapGroupRow {
    pub id: String,
    pub label: String,
    /// 实现 crate（`domain.toml` 引用的是 id，source 只用于"这是谁提供的"）
    pub source: String,
    /// 该组下**当前工具面里**的工具数
    pub tools: usize,
    /// 当前领域是否声明了这个组（false = 被领域收敛掉了，工具不会下发）
    pub declared: bool,
}


/// 注册表里的一个组（能力页左栏）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RegGroupRow {
    pub id: String,
    pub label: String,
    /// 用户可编辑的组描述（管理 UI 的副标题）。
    pub description: String,
    /// builtin / plugin / declarative
    pub source: String,
    pub enabled: bool,
    /// 该组条目总数
    pub total: usize,
    /// 其中当前生效的条数（装好+开着+健康）
    pub active: usize,
    /// 缺文件的条数
    pub broken: usize,
    /// 未安装的条数
    pub discovered: usize,
}

/// 该组是否进入「插件工具管理」UI。
///
/// `core` / `sandbox` / `shell` / `space` / `memory` 这 5 组是基座内置基础能力，
/// 由基座自己管理，不进插件工具管理界面；其余组（含「未分配」）都归 UI 管理。
pub fn is_managed_group(id: &str) -> bool {
    !matches!(id, "core" | "sandbox" | "shell" | "space" | "memory")
}

/// 是否为保留的「未分配」组。
pub fn is_unassigned(id: &str) -> bool {
    id == "__unassigned__"
}

/// 注册表里的一个条目（能力页中栏）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RegEntryRow {
    pub id: String,
    pub group: String,
    pub name: String,
    pub source: String,
    pub risk: String,
    /// ok / broken / discovered
    pub status: String,
    pub enabled: bool,
    pub installed: bool,
    /// 组是否启用（决定它最终生不生效）
    pub group_enabled: bool,
    pub note: String,
    pub capabilities: Vec<String>,
}

/// 基座整体快照。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub version: String,
    /// 当前生效模型（空 = 未配置）
    pub model: String,
    /// 活跃空间（空 = 全局）
    pub space: String,
    pub tool_count: usize,
    pub tools: Vec<ToolRow>,
    /// 隔离区工具（未声明组，默认不挂载）
    pub isolated: Vec<String>,
    /// 清单启用但本产物未链接的群组
    pub missing_impls: Vec<String>,
    pub spaces: Vec<SpaceRow>,
    /// 已装入的技能（技能页明细）
    pub skills: Vec<SkillRow>,
    /// 当前授权范围（设置页可直接编辑；一行一条规则）
    pub scope_rules: Vec<String>,
    pub warnings: Vec<String>,
    pub data_root: String,
    /// 激活领域 id（空 = 纯基座模式）
    pub domain: String,
    /// 领域页：`domains/` 下全部领域（含未激活）
    pub domains: Vec<DomainCardRow>,
    /// 能力页：基座能力组（8 组固定存在，`declared=false` 表示被领域收敛掉）
    pub cap_groups: Vec<CapGroupRow>,
    /// 能力页（注册表视图）：组与条目来自 `<data>/registry.toml`，含插件
    pub reg_groups: Vec<RegGroupRow>,
    pub reg_entries: Vec<RegEntryRow>,
    /// 注册表统计摘要（一行文字）
    pub reg_summary: String,
}

impl Snapshot {
    /// 状态栏一行摘要。
    pub fn status_line(&self) -> String {
        let model = if self.model.is_empty() { "未配置模型".to_string() } else { self.model.clone() };
        let space = if self.space.is_empty() { "全局".to_string() } else { self.space.clone() };
        format!("模型 {model} · 空间 {space} · 工具 {} 个", self.tool_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_handles_empty_state() {
        let snap = Snapshot::default();
        let line = snap.status_line();
        assert!(line.contains("未配置模型"));
        assert!(line.contains("全局"));
        assert!(line.contains("工具 0 个"));
    }

    #[test]
    fn managed_group_excludes_five_builtin_base_groups() {
        for id in ["core", "sandbox", "shell", "space", "memory"] {
            assert!(!is_managed_group(id), "{id} 是内置基础能力组，不应进插件管理 UI");
        }
        assert!(is_managed_group("file"));
        assert!(is_managed_group("net"));
        assert!(is_managed_group("text"));
        assert!(is_managed_group("my_plugin"));
        assert!(is_managed_group("__unassigned__"), "未分配组也归管理 UI 使用");
    }

    #[test]
    fn is_unassigned_only_matches_reserved_id() {
        assert!(is_unassigned("__unassigned__"));
        assert!(!is_unassigned("core"));
        assert!(!is_unassigned("未分配"));
        assert!(!is_unassigned(""));
    }

    #[test]
    fn reg_group_row_carries_description() {
        let mut row = RegGroupRow::default();
        row.description = "示例描述".to_string();
        assert_eq!(row.description, "示例描述");
    }
}
