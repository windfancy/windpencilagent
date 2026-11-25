//! 知识约束 → 可执行的 guard 谓词。
//!
//! 知识群组产出的 `Must` 约束不能只是一句给模型看的话，必须变成**能拦人**的红线：
//!
//! ```text
//! 工具调用 → ScopeGuard → denylist → 【约束钩子】Must 违反 → 拒绝并回灌理由
//!          → 风险判定 → 审批 → 执行
//! ```
//!
//! 本模块只做**纯规则判定**（不依赖基座钩子类型），由 engine 包成 `ToolHook`：
//! 契约层不 import `kern-core` / `toolkit-core` 的老规矩不破。

use wpa_api::{Constraint, ConstraintSeverity, RiskTier};

use crate::pnc::parse_risk;

/// 默认编译器：从约束自带的执行元数据生成规则。
///
/// 三条元数据全缺的 `Must` 返回 `None` —— 宁可明确"降级为提示"，也不假装拦住了。
pub fn compile_default(c: &Constraint) -> Option<GuardRule> {
    if c.tools.is_empty() && c.min_risk.is_none() && c.target_contains.is_empty() {
        return None;
    }
    let mut rule = GuardRule::new(c.id.clone(), c.text.clone());
    if !c.tools.is_empty() {
        let pats: Vec<&str> = c.tools.iter().map(|s| s.as_str()).collect();
        rule = rule.for_tools(&pats);
    }
    if let Some(m) = &c.min_risk {
        rule = rule.min_risk(parse_risk(Some(m)));
    }
    if !c.target_contains.is_empty() {
        let pats: Vec<&str> = c.target_contains.iter().map(|s| s.as_str()).collect();
        rule = rule.target_contains(&pats);
    }
    Some(rule)
}

/// 一条可执行约束（由自然语言约束 + 适用面编译而来）。
#[derive(Debug, Clone)]
pub struct GuardRule {
    /// 约束 id（审计与回灌理由用）。
    pub id: String,
    /// 约束原文（拒绝时回灌给 LLM，便于自我纠正）。
    pub text: String,
    /// 适用工具：精确名或 `前缀.*`；空 = 全部工具。
    pub tools: Vec<String>,
    /// 风险门槛：达到该档才拦；`None` = 不看风险。
    pub min_risk: Option<RiskTier>,
    /// 目标匹配（子串）：命中当前调用参数里的目标才拦；空 = 不看目标。
    pub target_contains: Vec<String>,
}

impl GuardRule {
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            tools: vec![],
            min_risk: None,
            target_contains: vec![],
        }
    }

    pub fn for_tools(mut self, tools: &[&str]) -> Self {
        self.tools = tools.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn min_risk(mut self, risk: RiskTier) -> Self {
        self.min_risk = Some(risk);
        self
    }

    pub fn target_contains(mut self, pats: &[&str]) -> Self {
        self.target_contains = pats.iter().map(|s| s.to_string()).collect();
        self
    }

    /// 工具名匹配：精确或 `前缀.*`。
    fn tool_matches(&self, tool: &str) -> bool {
        if self.tools.is_empty() {
            return true;
        }
        self.tools.iter().any(|p| {
            if let Some(prefix) = p.strip_suffix(".*") {
                tool.starts_with(prefix)
            } else {
                p == tool
            }
        })
    }

    /// 判定：命中即返回拒绝理由。
    pub fn check(&self, tool: &str, risk: RiskTier, args_summary: &str) -> Option<String> {
        if !self.tool_matches(tool) {
            return None;
        }
        if let Some(min) = self.min_risk {
            if risk < min {
                return None;
            }
        }
        if !self.target_contains.is_empty() {
            let hit = self
                .target_contains
                .iter()
                .any(|p| !p.is_empty() && args_summary.contains(p));
            if !hit {
                return None;
            }
        }
        Some(format!("违反知识约束 [{}]：{}", self.id, self.text))
    }
}

/// 约束集合：`Must` 编译为规则，`Should`/`Info` 只进 prompt（不拦人）。
#[derive(Debug, Clone, Default)]
pub struct ConstraintSet {
    rules: Vec<GuardRule>,
    /// 非强制约束（注入 prompt 用）。
    advisory: Vec<Constraint>,
}

impl ConstraintSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从知识提供者返回的约束构建。`compile` 负责把业务约束翻译成规则；
    /// 没有对应规则的 `Must` 会被降级为 advisory 并留痕（不能假装拦住了）。
    pub fn from_constraints(
        constraints: &[Constraint],
        compile: impl Fn(&Constraint) -> Option<GuardRule>,
    ) -> (Self, Vec<String>) {
        let mut set = Self::new();
        let mut warnings = Vec::new();
        for c in constraints {
            match c.severity {
                ConstraintSeverity::Must => match compile(c) {
                    Some(rule) => set.rules.push(rule),
                    None => {
                        warnings.push(format!(
                            "Must 约束 {} 无法编译为 guard（已降级为提示，不会拦人）：{}",
                            c.id, c.text
                        ));
                        set.advisory.push(c.clone());
                    }
                },
                ConstraintSeverity::Should | ConstraintSeverity::Info => {
                    set.advisory.push(c.clone())
                }
            }
        }
        (set, warnings)
    }

    pub fn with_rule(mut self, rule: GuardRule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn rules(&self) -> &[GuardRule] {
        &self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.advisory.is_empty()
    }

    /// 判定一次调用：首个命中即拒绝。
    pub fn check(&self, tool: &str, risk: RiskTier, args_summary: &str) -> Option<String> {
        self.rules
            .iter()
            .find_map(|r| r.check(tool, risk, args_summary))
    }

    /// 注入 system prompt 的约束段（只含非强制项 + 规则摘要）。
    pub fn prompt_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut s = String::from("## 业务规范约束\n");
        if !self.rules.is_empty() {
            s.push_str("以下为强制红线，违反会被直接拒绝：\n");
            for r in &self.rules {
                s.push_str(&format!("- [{}] {}\n", r.id, r.text));
            }
        }
        if !self.advisory.is_empty() {
            s.push_str("以下为建议遵守：\n");
            for c in &self.advisory {
                s.push_str(&format!("- [{}] {}\n", c.id, c.text));
            }
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_matches_tool_prefix_and_risk() {
        let r = GuardRule::new("no-destroy", "禁止在生产网段使用破坏性工具")
            .for_tools(&["exploit.*", "scan.dos"])
            .min_risk(RiskTier::High);
        assert!(r.check("exploit.rce", RiskTier::High, "{}").is_some());
        assert!(r.check("exploit.rce", RiskTier::Medium, "{}").is_none());
        assert!(r.check("scan.port", RiskTier::Critical, "{}").is_none());
        assert!(r.check("scan.dos", RiskTier::Critical, "{}").is_some());
    }

    #[test]
    fn rule_target_filter() {
        let r = GuardRule::new("prod", "禁止对生产目标测试").target_contains(&["prod-"]);
        assert!(r.check("any.tool", RiskTier::Low, r#"{"target":"prod-db"}"#).is_some());
        assert!(r.check("any.tool", RiskTier::Low, r#"{"target":"test-db"}"#).is_none());
    }

    #[test]
    fn must_without_rule_is_downgraded_with_warning() {
        let cs = vec![
            Constraint { id: "c1".into(), text: "必须留痕".into(), severity: ConstraintSeverity::Must, ..Default::default() },
            Constraint { id: "c2".into(), text: "建议先侦察".into(), severity: ConstraintSeverity::Should, ..Default::default() },
        ];
        let (set, warns) = ConstraintSet::from_constraints(&cs, |_| None);
        assert!(set.rules().is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("降级"));
        let sec = set.prompt_section().unwrap();
        assert!(sec.contains("建议遵守"));
    }

    #[test]
    fn compiled_rules_block_and_explain() {
        let cs = vec![Constraint {
            id: "roe-1".into(),
            text: "禁止扫描授权范围外目标".into(),
            severity: ConstraintSeverity::Must,
            tools: vec!["scan.*".into()],
            ..Default::default()
        }];
        let (set, warns) = ConstraintSet::from_constraints(&cs, compile_default);
        assert!(warns.is_empty());
        let deny = set.check("scan.port", RiskTier::Low, "{}").expect("应拒绝");
        assert!(deny.contains("roe-1"));
        assert!(deny.contains("禁止扫描"));
        assert!(set.check("text.codec", RiskTier::Low, "{}").is_none());
    }

    #[test]
    fn default_compiler_uses_risk_and_target_metadata() {
        let c = Constraint {
            id: "roe-2".into(),
            text: "禁止对生产目标做高危操作".into(),
            severity: ConstraintSeverity::Must,
            min_risk: Some("high".into()),
            target_contains: vec!["prod".into()],
            ..Default::default()
        };
        let rule = compile_default(&c).expect("有元数据应可编译");
        assert!(rule.check("x.y", RiskTier::High, r#"{"host":"prod-db"}"#).is_some());
        assert!(rule.check("x.y", RiskTier::Low, r#"{"host":"prod-db"}"#).is_none());
        assert!(rule.check("x.y", RiskTier::High, r#"{"host":"test"}"#).is_none());
    }
}
