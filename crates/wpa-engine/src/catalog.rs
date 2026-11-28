//! 基座能力目录：**工具 → 能力组** 的唯一真相表。
//!
//! 为什么需要它：基座工具（`toolkit-core` / `shell-core` / `net-core` / …）是
//! 由装配层直接 `kern.register_tool()` 注册的，内核只记名字，不记"它属于哪一层能力"。
//! 而两件事都需要这个归属：
//!
//! 1. **领域的工具面**——`domain.toml` 写的是"我要 core/file/net 这些组"，
//!    没有归属就没法把组展开成工具集合，领域声明就只是一段注释。
//! 2. **工具清单导出**（`registry export`）——生成的 `toolkit.toml` 必须写组名，
//!    否则清单无法按能力审计。
//!
//! 刻意做成静态表而不是给 `Tool` trait 加方法：`Tool` 契约在 `toolkit-core`，
//! 加字段会波及所有实现（含插件桥接），而这里是**装配层**的编排知识。
//!
//! 维护约定：新增基座工具必须在这里登记（能力组或 [`AGENT_TOOLS`]），
//! 不允许出现"注册了但没人知道它属于哪一层"的工具——那会让领域声明的工具面
//! 与实际下发的工具面悄悄分叉。

/// 一个能力组。
#[derive(Debug, Clone, Copy)]
pub struct CapGroup {
    /// 组 id（`domain.toml` 的 `groups` 用它引用）。
    pub id: &'static str,
    /// 界面展示名。
    pub label: &'static str,
    /// 实现 crate（工具清单导出用）。
    pub source: &'static str,
}

/// 基座内置能力组（顺序即工具清单里的分组顺序）。
pub const CAP_GROUPS: &[CapGroup] = &[
    CapGroup { id: "core", label: "内核", source: "wpa-engine" },
    CapGroup { id: "file", label: "文件", source: "toolkit-core" },
    CapGroup { id: "shell", label: "命令与作业", source: "shell-core" },
    CapGroup { id: "sandbox", label: "沙箱执行", source: "toolkit-core" },
    CapGroup { id: "net", label: "网络", source: "net-core" },
    CapGroup { id: "text", label: "文本", source: "text-core" },
    CapGroup { id: "kb", label: "知识读写", source: "data-core" },
    CapGroup { id: "space", label: "工作空间", source: "workspace-core" },
    CapGroup { id: "memory", label: "记忆检索", source: "base-store" },
];

/// **基础组**：系统自带、与领域无关的能力。
///
/// 它们不参与"装配设置"（领域组引用、空间工具白名单），永远是所有领域的
/// 工具面子集。理由：这些是基座的运行前提——把 `core` 漏掉就没有知识检索，
/// 把 `memory`/`space` 漏掉记忆与工作区直接失效，而用户无法从界面上理解
/// "少勾一个 chip 就把系统搞坏了"。所以它们**只由代码决定，不进任何配置**。
///
/// `file` / `net` / `text` / `kb` 之外的专业组与插件组才是可配置部分。
pub const BASE_GROUPS: &[&str] = &["core", "shell", "sandbox", "space", "memory", "kb"];

/// 是否基础组（装配时无条件保留，且不出现在任何勾选界面里）。
pub fn is_base_group(id: &str) -> bool {
    BASE_GROUPS.contains(&id)
}

/// 工具 → 能力组。
///
/// **不在这里的工具**（`plan.*` / `subagent.spawn`）属于智能体层，不随领域收敛：
/// 它们由 `[agent].plan_mode` / `subagent_enabled` 控制，写进领域组反而会让
/// "领域 A 不小心漏写一个组"变成"规划层消失"。
pub const TOOL_GROUP: &[(&str, &str)] = &[
    ("kb.search", "core"),
    ("util.echo", "core"),
    ("plan.phase", "core"),
    ("file.read", "file"),
    ("file.write", "file"),
    ("shell.run", "shell"),
    ("shell.jobs", "shell"),
    ("shell.sessions", "shell"),
    ("os.exec", "sandbox"),
    ("sandbox.python", "sandbox"),
    ("sandbox.pip", "sandbox"),
    ("net.reach", "net"),
    ("net.scan", "net"),
    ("net.banner", "net"),
    ("net.dns", "net"),
    ("net.http", "net"),
    ("text.codec", "text"),
    ("text.hash", "text"),
    ("text.regex", "text"),
    ("text.extract", "text"),
    ("text.transform", "text"),
    // 知识读写（sqlite / 向量库）：基础组，所有领域默认可用
    ("kb.sqlite.create", "kb"),
    ("kb.sqlite.query", "kb"),
    ("kb.sqlite.insert", "kb"),
    ("kb.sqlite.update", "kb"),
    ("kb.sqlite.delete", "kb"),
    ("kb.vec.create", "kb"),
    ("kb.vec.query", "kb"),
    ("kb.vec.insert", "kb"),
    ("kb.vec.update", "kb"),
    ("kb.vec.delete", "kb"),
    ("ws.create", "space"),
    ("ws.list", "space"),
    ("ws.switch", "space"),
    ("ws.remove", "space"),
    ("ws.info", "space"),
    ("ws.record", "space"),
    ("ws.memory.append", "space"),
    ("ws.memory.read", "space"),
    ("session.search", "memory"),
];

/// 智能体层工具（不属于任何能力组，不随领域收敛）。
pub const AGENT_TOOLS: &[&str] = &[
    "plan.create",
    "plan.update",
    "plan.view",
    "plan.exit",
    "subagent.spawn",
    "task.todo",
];

/// 查询工具所属能力组（未知 ⇒ `None`，调用方应按"保留"处理）。
pub fn group_of(tool: &str) -> Option<&'static str> {
    TOOL_GROUP.iter().find(|(t, _)| *t == tool).map(|(_, g)| *g)
}

/// 是否智能体层工具。
pub fn is_agent_tool(tool: &str) -> bool {
    AGENT_TOOLS.contains(&tool)
}

/// 组是否存在（用于领域配置自检）。
pub fn is_known_group(id: &str) -> bool {
    CAP_GROUPS.iter().any(|g| g.id == id)
}

/// 组的展示名（未知组原样返回）。
pub fn label_of(id: &str) -> &str {
    CAP_GROUPS.iter().find(|g| g.id == id).map(|g| g.label).unwrap_or(id)
}

/// 组的实现来源 crate（未知 ⇒ `None`）。
pub fn source_of(id: &str) -> Option<&'static str> {
    CAP_GROUPS.iter().find(|g| g.id == id).map(|g| g.source)
}


/// 一个内置工具的规格（注册表首次生成时用它落盘）。
///
/// 为什么要有这张表：P0-1 要求 `data/registry.toml` 是唯一真相源、内置工具也进注册表。
/// 而内置工具是编译期构造的（`register_base_tools` 里 `Arc::new(X)`），没有清单文件，
/// 所以"它叫什么、属于哪个组、风险多少、能提供什么能力"必须有静态来源——就是这里。
///
/// `risk` 只是**展示与初始值**：真正审批用的是工具自己的 `risk_for(args)`，
/// 改这里不会放宽任何一次调用的审批。
#[derive(Debug, Clone, Copy)]
pub struct BuiltinSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub risk: &'static str,
    /// 能力标签（受控词表见 `wpa_api::registry::caps`）；空 = 不参与能力解析。
    pub caps: &'static [&'static str],
}

/// 全部内置工具（顺序即注册表里的条目顺序）。
///
/// 维护约定：`register_base_tools` 里注册一个工具，这里必须同时加一条，
/// 否则它不会出现在注册表与能力页里（测试 `every_registered_builtin_is_declared` 守门）。
pub const BUILTIN_SPECS: &[BuiltinSpec] = &[
    // core
    BuiltinSpec { name: "kb.search", label: "知识检索", group: "core", risk: "low", caps: &[] },
    BuiltinSpec { name: "util.echo", label: "回显", group: "core", risk: "low", caps: &[] },
    BuiltinSpec { name: "plan.phase", label: "阶段切换", group: "core", risk: "low", caps: &[] },
    // file
    BuiltinSpec { name: "file.read", label: "读文件", group: "file", risk: "low", caps: &[] },
    BuiltinSpec { name: "file.write", label: "写文件", group: "file", risk: "medium", caps: &[] },
    // shell
    BuiltinSpec { name: "shell.run", label: "执行命令", group: "shell", risk: "high", caps: &[] },
    BuiltinSpec { name: "shell.jobs", label: "后台作业", group: "shell", risk: "low", caps: &[] },
    BuiltinSpec { name: "shell.sessions", label: "会话管理", group: "shell", risk: "low", caps: &[] },
    // sandbox
    BuiltinSpec { name: "os.exec", label: "受限执行", group: "sandbox", risk: "high", caps: &[] },
    BuiltinSpec { name: "sandbox.python", label: "Python 沙箱", group: "sandbox", risk: "high", caps: &[] },
    BuiltinSpec { name: "sandbox.pip", label: "依赖安装", group: "sandbox", risk: "medium", caps: &[] },
    // net
    BuiltinSpec { name: "net.reach", label: "HTTP 请求", group: "net", risk: "medium", caps: &["http_fetch"] },
    BuiltinSpec { name: "net.scan", label: "端口探测", group: "net", risk: "high", caps: &["port_scan"] },
    BuiltinSpec { name: "net.banner", label: "服务指纹", group: "net", risk: "low", caps: &["service_fingerprint"] },
    BuiltinSpec { name: "net.dns", label: "DNS 解析", group: "net", risk: "low", caps: &["dns_resolve"] },
    BuiltinSpec { name: "net.http", label: "HTTP 报文", group: "net", risk: "medium", caps: &["http_fetch"] },
    // text
    BuiltinSpec { name: "text.codec", label: "编解码", group: "text", risk: "low", caps: &[] },
    BuiltinSpec { name: "text.hash", label: "哈希", group: "text", risk: "low", caps: &[] },
    BuiltinSpec { name: "text.regex", label: "正则", group: "text", risk: "low", caps: &[] },
    BuiltinSpec { name: "text.extract", label: "线索抽取", group: "text", risk: "low", caps: &[] },
    BuiltinSpec { name: "text.transform", label: "文本变换", group: "text", risk: "low", caps: &[] },
    // kb（知识读写：本地 sqlite 与向量库）
    BuiltinSpec { name: "kb.sqlite.create", label: "建库建表", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.sqlite.query", label: "SQL 查询", group: "kb", risk: "low", caps: &[] },
    BuiltinSpec { name: "kb.sqlite.insert", label: "插入行", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.sqlite.update", label: "更新行", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.sqlite.delete", label: "删除行", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.vec.create", label: "建向量库", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.vec.query", label: "向量检索", group: "kb", risk: "low", caps: &[] },
    BuiltinSpec { name: "kb.vec.insert", label: "向量写入", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.vec.update", label: "向量更新", group: "kb", risk: "medium", caps: &[] },
    BuiltinSpec { name: "kb.vec.delete", label: "向量删除", group: "kb", risk: "medium", caps: &[] },
    // space
    BuiltinSpec { name: "ws.create", label: "新建空间", group: "space", risk: "low", caps: &[] },
    BuiltinSpec { name: "ws.list", label: "列出空间", group: "space", risk: "low", caps: &[] },
    BuiltinSpec { name: "ws.switch", label: "切换空间", group: "space", risk: "low", caps: &[] },
    BuiltinSpec { name: "ws.remove", label: "删除空间", group: "space", risk: "medium", caps: &[] },
    BuiltinSpec { name: "ws.info", label: "空间详情", group: "space", risk: "low", caps: &[] },
    BuiltinSpec { name: "ws.record", label: "写入经验", group: "space", risk: "low", caps: &[] },
    BuiltinSpec { name: "ws.memory.append", label: "追加日记忆", group: "space", risk: "low", caps: &[] },
    BuiltinSpec { name: "ws.memory.read", label: "读日记忆", group: "space", risk: "low", caps: &[] },
    // memory
    BuiltinSpec { name: "session.search", label: "会话检索", group: "memory", risk: "low", caps: &[] },
];

/// 按名字查内置规格。
pub fn builtin_spec(name: &str) -> Option<&'static BuiltinSpec> {
    BUILTIN_SPECS.iter().find(|s| s.name == name)
}

/// 某组的全部内置工具名。
pub fn builtin_names_in(group: &str) -> Vec<&'static str> {
    BUILTIN_SPECS.iter().filter(|s| s.group == group).map(|s| s.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_group_exists() {
        for (tool, g) in TOOL_GROUP {
            assert!(is_known_group(g), "工具 {tool} 指向未知组 {g}");
        }
    }

    #[test]
    fn no_duplicate_tool_entries() {
        let mut names: Vec<&str> = TOOL_GROUP.iter().map(|(t, _)| *t).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "TOOL_GROUP 有重复登记");
    }

    #[test]
    fn known_tools_resolve() {
        assert_eq!(group_of("net.http"), Some("net"));
        assert_eq!(group_of("plan.create"), None, "智能体层工具不参与领域收敛");
        assert_eq!(label_of("net"), "网络");
        assert_eq!(source_of("net"), Some("net-core"));
    }

    /// 规格表必须覆盖 TOOL_GROUP 里登记的每一个内置工具（两侧不许分叉）。
    #[test]
    fn specs_cover_every_mapped_tool() {
        for (tool, group) in TOOL_GROUP {
            let spec = builtin_spec(tool).unwrap_or_else(|| panic!("{tool} 没有 BuiltinSpec"));
            assert_eq!(spec.group, *group, "{tool} 的组在两张表里不一致");
        }
    }

    /// 规格表自身不许重复、组必须存在。
    #[test]
    fn specs_are_consistent() {
        let mut names: Vec<&str> = BUILTIN_SPECS.iter().map(|s| s.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "BUILTIN_SPECS 有重复");
        for s in BUILTIN_SPECS {
            assert!(is_known_group(s.group), "{} 指向未知组 {}", s.name, s.group);
            assert!(!s.label.is_empty());
        }
    }

    /// 能力标签必须在受控词表里（写错标签等于规划器永远找不到它）。
    #[test]
    fn declared_caps_are_known() {
        for s in BUILTIN_SPECS {
            for c in s.caps {
                assert!(wpa_api::registry::caps::is_known(c), "{} 声明了未知能力 {c}", s.name);
            }
        }
    }

    /// 基础组必须都是合法组，且 `core` 之类绝不能被当成可配置组。
    #[test]
    fn base_groups_are_known_and_not_managed() {
        for g in BASE_GROUPS {
            assert!(is_known_group(g), "基础组 {g} 不在 CAP_GROUPS 里");
        }
        assert!(is_base_group("core"));
        assert!(is_base_group("kb"));
        assert!(!is_base_group("file"));
        assert!(!is_base_group("net"));
        // 基础组的工具必须真的登记在 TOOL_GROUP，否则"永远可见"只是一句空话
        for g in BASE_GROUPS {
            assert!(!builtin_names_in(g).is_empty(), "基础组 {g} 没有任何工具");
        }
    }
}
