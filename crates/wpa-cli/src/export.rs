//! `registry export`：把当前装配出来的工具面导出成领域工具清单（`toolkit.toml`）。
//!
//! 为什么要有这个文件：`domain.toml` 的 `groups` 写的是"我要哪些组"，但组展开后
//! 到底有哪些工具、多大 schema 体积，只能去跑一遍才知道。生成一份可 diff 的清单后：
//! - 审计：这个领域对外暴露了哪些能力，一望可知；
//! - 演进：两个版本的 diff 就是能力变更记录；
//! - CI：`registry export --check` 重新生成后比对，不一致即失败（防止清单过时）。
//!
//! **刻意不写的东西**：逐工具的 `kind`（builtin/plugin/declarative）与 `source`。
//! 内核注册表只记"工具属于哪个组"，不记它由谁实现；硬写只能靠一张猜出来的映射表。
//! 宁可少一列，也不产出"看着确定其实是猜"的审计文件。

use wpa_engine::catalog;
use wpa_engine::Base;

/// 从基座快照导出。
pub fn render_for(base: &Base, domain_id: &str, domain_name: &str, declared: &[String]) -> String {
    render(domain_id, domain_name, declared, base.snapshot().tools)
}

/// 按工具行渲染清单正文（纯函数，便于测试）。
pub fn render(
    domain_id: &str,
    domain_name: &str,
    declared: &[String],
    mut tools: Vec<wpa_engine::ToolRow>,
) -> String {
    // 组排序：基座能力组按 catalog 顺序 → agent 层 → 其余（插件/树组）按字典序。
    // 固定顺序是为了让 diff 只反映真实变化，而不是快照遍历顺序的抖动。
    fn rank(group: &str) -> (u8, usize, String) {
        if let Some(i) = catalog::CAP_GROUPS.iter().position(|g| g.id == group) {
            return (0, i, String::new());
        }
        if group == "agent" {
            return (1, 0, String::new());
        }
        (2, 0, group.to_string())
    }
    tools.sort_by(|a, b| rank(&a.group).cmp(&rank(&b.group)).then_with(|| a.name.cmp(&b.name)));
    tools.dedup_by(|a, b| a.name == b.name);

    let mut group_ids: Vec<String> = Vec::new();
    for t in &tools {
        if !group_ids.contains(&t.group) {
            group_ids.push(t.group.clone());
        }
    }

    let mut out = String::new();
    out.push_str(
        "# ============================================================================\n\
         # 领域工具清单（GENERATED —— 不要手工编辑）\n\
         # ============================================================================\n\
         #\n",
    );
    out.push_str(&format!(
        "# 生成命令：windpencilagent-cli registry export --domain {domain_id}\n\
         # 基座版本：{}\n\
         # 领域定义：domains/{domain_id}/domain.toml\n\
         #\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(
        "# 本文件不参与装配（装配读注册表），用途有三个：\n\
         #   1. 审计：这个领域到底暴露了哪些工具，一望可知\n\
         #   2. 演进：版本间 diff 即能力变更记录\n\
         #   3. 预算：tool_count 与 schema 体积可估算 prompt 开销\n\
         #\n\
         # 为什么没有 kind/source 列：注册表只记「工具属于哪个组」，不记实现来源。\n\
         # 硬写只能靠猜，本文件宁可少一列也不写不确切的字段。\n\
         # ============================================================================\n\n",
    );

    let missing: Vec<&str> = declared
        .iter()
        .filter(|g| !group_ids.iter().any(|x| x == *g))
        .map(String::as_str)
        .collect();
    out.push_str("[meta]\n");
    out.push_str(&format!("domain = {}\n", quote(domain_id)));
    out.push_str(&format!("domain_name = {}\n", quote(domain_name)));
    out.push_str("registry_version = 1\n");
    out.push_str(&format!("group_count = {}\n", group_ids.len()));
    out.push_str(&format!("tool_count = {}\n", tools.len()));
    if missing.is_empty() {
        out.push_str("declared_but_empty = []\n");
    } else {
        let list: Vec<String> = missing.iter().map(|g| quote(g)).collect();
        out.push_str(&format!(
            "declared_but_empty = [{}]   # 声明了但没有任何工具属于它们\n",
            list.join(", ")
        ));
    }
    out.push('\n');

    for g in &group_ids {
        let n = tools.iter().filter(|t| &t.group == g).count();
        out.push_str("[[group]]\n");
        out.push_str(&format!("id = {}\n", quote(g)));
        out.push_str(&format!("label = {}\n", quote(catalog::label_of(g))));
        out.push_str(&format!("tools = {n}\n\n"));
    }

    for t in &tools {
        out.push_str("[[tool]]\n");
        out.push_str(&format!("name = {}\n", quote(&t.name)));
        out.push_str(&format!("group = {}\n", quote(&t.group)));
        out.push_str(&format!("risk = {}\n", quote(&t.risk.to_ascii_lowercase())));
        out.push_str(&format!("summary = {}\n\n", quote(&t.description)));
    }

    out
}

/// TOML 字符串：转义反斜杠/引号/换行（描述里可能出现它们）。
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, group: &str) -> wpa_engine::ToolRow {
        wpa_engine::ToolRow {
            name: name.into(),
            group: group.into(),
            risk: "Low".into(),
            description: format!("{name} 的描述"),
        }
    }

    #[test]
    fn quote_escapes_specials() {
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(quote("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn empty_surface_still_renders_valid_meta() {
        let s = render("default", "通用对话", &["core".into()], vec![]);
        assert!(s.contains("[meta]"));
        assert!(s.contains("tool_count = 0"));
        assert!(s.contains("declared_but_empty = [\"core\"]"), "{s}");
        assert!(!s.contains("[[tool]]"));
    }

    #[test]
    fn groups_follow_catalog_order_not_input_order() {
        let tools = vec![row("ws.list", "space"), row("net.http", "net"), row("kb.search", "core")];
        let s = render("default", "通用对话", &[], tools);
        let core = s.find("id = \"core\"").expect("core");
        let net = s.find("id = \"net\"").expect("net");
        let space = s.find("id = \"space\"").expect("space");
        assert!(core < net && net < space, "组顺序应按 catalog：{s}");
    }

    #[test]
    fn duplicate_tool_names_collapse() {
        let tools = vec![row("net.http", "net"), row("net.http", "net")];
        let s = render("default", "x", &[], tools);
        assert_eq!(s.matches("name = \"net.http\"").count(), 1);
    }

    #[test]
    fn tools_are_sorted_by_name_inside_group() {
        let tools = vec![row("text.regex", "text"), row("text.codec", "text"), row("text.hash", "text")];
        let s = render("default", "x", &[], tools);
        let codec = s.find("text.codec").unwrap();
        let hash = s.find("text.hash").unwrap();
        let regex = s.find("text.regex").unwrap();
        assert!(codec < hash && hash < regex);
    }
}
