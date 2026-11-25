//! 阶段机：业务决策"分阶段、每阶段可用手段不同"的落地。
//!
//! 基座对阶段**无语义认知**——它只认 `Phase { id, label, allowed_groups, ... }` 这层结构，
//! 做三件事：
//! 1. 把"当前阶段 + 进入/退出条件"写进 system prompt；
//! 2. 按 `allowed_groups` 收敛工具面（阶段外的工具不进 schema，模型根本看不见）；
//! 3. 切换时产出可审计的迁移记录。
//!
//! 阶段推进由 LLM 显式声明（基座提供 `plan.phase` 工具），**不做隐式推断**：
//! 隐式推断会在长会话里静默改变能力面，难以复盘。

use std::sync::{Arc, Mutex};

use wpa_api::Phase;

/// 阶段集合 + 当前位置。
#[derive(Debug, Clone, Default)]
pub struct PhaseSet {
    phases: Vec<Phase>,
    /// 当前阶段索引；`None` = 尚未进入任何阶段（等价于"全阶段可见"）。
    current: Option<usize>,
}

/// 一次阶段迁移（审计事件载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseTransition {
    pub from: Option<String>,
    pub to: String,
}

impl PhaseSet {
    pub fn new(phases: Vec<Phase>) -> Self {
        Self { phases, current: None }
    }

    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    pub fn is_empty(&self) -> bool {
        self.phases.is_empty()
    }

    pub fn current(&self) -> Option<&Phase> {
        self.current.and_then(|i| self.phases.get(i))
    }

    pub fn current_id(&self) -> Option<&str> {
        self.current().map(|p| p.id.as_str())
    }

    /// 进入指定阶段（`None` = 阶段清单的第一个）。
    ///
    /// 未定义阶段 id 一律拒绝（fail-closed：不许进入不存在的阶段而绕过收敛）。
    pub fn advance(&mut self, to: Option<&str>) -> Result<PhaseTransition, String> {
        if self.phases.is_empty() {
            return Err("本树未定义阶段，无需推进".into());
        }
        let from = self.current().map(|p| p.id.clone());
        let idx = match to {
            Some(id) => self
                .phases
                .iter()
                .position(|p| p.id == id)
                .ok_or_else(|| {
                    let known: Vec<&str> = self.phases.iter().map(|p| p.id.as_str()).collect();
                    format!("未知阶段 {id}（已定义：{}）", known.join(" / "))
                })?,
            None => match self.current {
                // 尚未进入任何阶段 → 从第一个阶段开始（不是第二个）。
                None => 0,
                Some(i) => (i + 1).min(self.phases.len() - 1),
            },
        };
        self.current = Some(idx);
        Ok(PhaseTransition {
            from,
            to: self.phases[idx].id.clone(),
        })
    }

    /// 当前阶段是否允许某个工具组。
    ///
    /// 三种情形放行：无阶段定义、未进入阶段、当前阶段未声明 allowed_groups。
    /// 保留字 `none` 表示"本阶段不开放任何业务工具组"（如受理阶段只对话不收工具）——
    /// 空列表已被"= 全部"占用，只能用显式保留字表达"一个都不给"。
    pub fn allows_group(&self, group: &str) -> bool {
        match self.current() {
            None => true,
            Some(p) if p.allowed_groups.iter().any(|g| g == "none") => false,
            Some(p) if p.allowed_groups.is_empty() => true,
            Some(p) => p.allowed_groups.iter().any(|g| g == group),
        }
    }

    /// 过滤工具面：给定 `(工具名, 归属组)`，返回当前阶段可见的工具名。
    pub fn visible_tools<'a, I>(&self, tools: I) -> Vec<String>
    where
        I: IntoIterator<Item = (&'a str, Option<&'a str>)>,
    {
        tools
            .into_iter()
            .filter(|(_, group)| match group {
                Some(g) => self.allows_group(g),
                // 基座工具（无归属组）不参与阶段收敛：它们是基础设施，
                // 收掉会让模型连文本编码都做不了。
                None => true,
            })
            .map(|(name, _)| name.to_string())
            .collect()
    }

    /// 注入 system prompt 的阶段段。
    pub fn prompt_section(&self) -> Option<String> {
        if self.phases.is_empty() {
            return None;
        }
        let mut s = String::from("## 作业阶段\n本任务分阶段推进，每个阶段可用手段不同；需要换阶段时调用 `plan.phase`。\n");
        if let Some(cur) = self.current() {
            s.push_str(&format!("**当前阶段：{}（{}）**\n", cur.label, cur.id));
            if !cur.entry_hint.is_empty() {
                s.push_str(&format!("- 进入依据：{}\n", cur.entry_hint));
            }
            if !cur.exit_condition.is_empty() {
                s.push_str(&format!("- 完成条件：{}\n", cur.exit_condition));
            }
            if !cur.allowed_groups.is_empty() {
                s.push_str(&format!("- 本阶段可用工具组：{}\n", cur.allowed_groups.join("、")));
                s.push_str("- 阶段外工具不会出现在工具面里；未完成本阶段不要跳步。\n");
            }
        } else {
            s.push_str("尚未进入任何阶段。先判断当前应处于哪个阶段，再调用 `plan.phase` 明确。\n");
            s.push_str("可用阶段：\n");
        }
        for p in &self.phases {
            s.push_str(&format!("- `{}` {}：{}", p.id, p.label, p.entry_hint));
            if !p.exit_condition.is_empty() {
                s.push_str(&format!("（完成：{}）", p.exit_condition));
            }
            s.push('\n');
        }
        Some(s)
    }
}

/// 可在线程间共享的阶段状态（引擎线程写、会话读取）。
pub type SharedPhases = Arc<Mutex<PhaseSet>>;

pub fn shared(phases: Vec<Phase>) -> SharedPhases {
    Arc::new(Mutex::new(PhaseSet::new(phases)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> PhaseSet {
        PhaseSet::new(vec![
            Phase::new("recon", "侦察").with_allowed(&["tools.scan", "knowledge.vulndb"]),
            Phase::new("verify", "验证").with_allowed(&["tools.vuln", "tools.web"]),
            Phase::new("report", "报告").with_allowed(&[]),
        ])
    }

    #[test]
    fn empty_phase_set_allows_everything() {
        let mut s = PhaseSet::new(vec![]);
        assert!(s.allows_group("anything"));
        assert!(s.prompt_section().is_none());
        assert!(s.advance(None).is_err());
    }

    #[test]
    fn advance_and_converge_tool_surface() {
        let mut s = demo();
        assert!(s.allows_group("tools.vuln"), "未进入阶段时全放行");
        let t = s.advance(Some("recon")).unwrap();
        assert_eq!(t, PhaseTransition { from: None, to: "recon".into() });
        assert!(s.allows_group("tools.scan"));
        assert!(!s.allows_group("tools.vuln"), "阶段外组必须被挡住");

        let tools = vec![
            ("scan.port", Some("tools.scan")),
            ("vuln.match", Some("tools.vuln")),
            ("text.codec", None),
        ];
        let visible = s.visible_tools(tools);
        assert_eq!(visible, vec!["scan.port", "text.codec"]);
    }

    #[test]
    fn unknown_phase_rejected() {
        let mut s = demo();
        let err = s.advance(Some("nope")).unwrap_err();
        assert!(err.contains("未知阶段"), "{err}");
        assert!(s.current_id().is_none(), "拒绝后不得改变状态");
    }

    #[test]
    fn empty_allowed_groups_means_no_restriction() {
        let mut s = demo();
        s.advance(Some("report")).unwrap();
        assert!(s.allows_group("tools.scan"), "未声明 allowed_groups 视为不限制");
    }

    #[test]
    fn prompt_mentions_current_phase() {
        let mut s = demo();
        s.advance(Some("verify")).unwrap();
        let p = s.prompt_section().unwrap();
        assert!(p.contains("当前阶段：验证"));
        assert!(p.contains("plan.phase"));
        assert!(p.contains("tools.vuln"));
    }

    #[test]
    fn explicit_none_blocks_every_group() {
        // 受理阶段只对话不收工具：allowed_groups = ["none"] 是"一个都不给"的唯一写法。
        let mut s = PhaseSet::new(vec![Phase {
            id: "intake".into(),
            label: "受理".into(),
            entry_hint: "还没有目标".into(),
            exit_condition: "范围确认".into(),
            allowed_groups: vec!["none".into()],
        }]);
        s.advance(Some("intake")).unwrap();
        assert!(!s.allows_group("tools.scan"));
        let visible = s.visible_tools(vec![("scan.port", Some("tools.scan")), ("text.codec", None)]);
        assert_eq!(visible, vec!["text.codec"], "基座工具不受阶段收敛影响");
    }

    #[test]
    fn next_phase_without_id() {
        let mut s = demo();
        s.advance(None).unwrap();
        assert_eq!(s.current_id(), Some("recon"));
        s.advance(None).unwrap();
        assert_eq!(s.current_id(), Some("verify"));
        s.advance(None).unwrap();
        assert_eq!(s.current_id(), Some("report"));
        s.advance(None).unwrap();
        assert_eq!(s.current_id(), Some("report"), "末阶段不越界");
    }
}
