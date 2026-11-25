//! 上下文预算器：把有限窗口切成五段，**按固定顺序裁剪**。
//!
//! ```text
//! system ≤15% | 知识 ≤10% | 经验 ≤10% | 工具 schema ≤10% | 对话+观察 ≥55%
//! ```
//!
//! 裁剪顺序铁律（先便宜后昂贵）：
//! 1. 先裁**可重建的观察**（大结果转存留摘要）——信息不丢，只是换了存放位置；
//! 2. 再裁**较早对话**（滚动摘要接管）；
//! 3. 最后才动 system prompt（且只裁"注入段"，基座骨架与 `Must` 约束永不裁）。
//!
//! 本模块只做**装订前的预算核算与截断**，真正的历史压缩仍在 `agent-core` 内部；
//! 两者职责不同：这里防止"一开始就超载"，那里处理"跑着跑着超载"。

use agent_core::estimate_text_tokens;

/// 五段预算比例（和为 1）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub system: f32,
    pub knowledge: f32,
    pub experience: f32,
    pub tools: f32,
    pub dialog: f32,
}

impl Default for Budget {
    fn default() -> Self {
        Self { system: 0.15, knowledge: 0.10, experience: 0.10, tools: 0.10, dialog: 0.55 }
    }
}

impl Budget {
    /// 比例归一化（配置写歪了也不能让总和≠1）。
    pub fn normalized(&self) -> Self {
        let vals = [self.system, self.knowledge, self.experience, self.tools, self.dialog];
        if vals.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Self::default();
        }
        let sum: f32 = vals.iter().sum();
        if sum <= 0.0 {
            return Self::default();
        }
        Self {
            system: self.system / sum,
            knowledge: self.knowledge / sum,
            experience: self.experience / sum,
            tools: self.tools / sum,
            dialog: self.dialog / sum,
        }
    }

    pub fn sum(&self) -> f32 {
        self.system + self.knowledge + self.experience + self.tools + self.dialog
    }
}

/// 一段的核算结果。
#[derive(Debug, Clone)]
pub struct Slice {
    pub name: &'static str,
    /// 允许的 token 上限。
    pub cap: u64,
    /// 实际估算占用。
    pub used: u64,
    /// 是否被截断。
    pub trimmed: bool,
}

impl Slice {
    pub fn over(&self) -> bool {
        self.used > self.cap
    }
}

/// 一次装订的核算结果。
#[derive(Debug, Clone, Default)]
pub struct ContextPlan {
    pub window: u64,
    pub slices: Vec<Slice>,
    pub warnings: Vec<String>,
}

impl ContextPlan {
    pub fn slice(&self, name: &str) -> Option<&Slice> {
        self.slices.iter().find(|s| s.name == name)
    }

    /// 总占用（估算）。
    pub fn used_total(&self) -> u64 {
        self.slices.iter().map(|s| s.used).sum()
    }

    pub fn has_room(&self) -> bool {
        self.used_total() <= self.window
    }
}

/// 各段当前的"未截断"占用（由调用方估算后传入）。
#[derive(Debug, Clone, Default)]
pub struct Load {
    pub system: u64,
    pub knowledge: u64,
    pub experience: u64,
    pub tools: u64,
    pub dialog: u64,
}

/// 核算：给出每段上限与实际占用，并标出超预算项。
pub fn plan(window: u64, budget: &Budget, load: &Load) -> ContextPlan {
    let b = budget.normalized();
    let window = window.max(1024);
    let mk = |name: &'static str, ratio: f32, used: u64| Slice {
        name,
        cap: (window as f32 * ratio) as u64,
        used,
        trimmed: false,
    };
    let mut slices = vec![
        mk("system", b.system, load.system),
        mk("knowledge", b.knowledge, load.knowledge),
        mk("experience", b.experience, load.experience),
        mk("tools", b.tools, load.tools),
        mk("dialog", b.dialog, load.dialog),
    ];
    let mut warnings = Vec::new();
    for s in &mut slices {
        if s.over() {
            // 工具 schema 是**不能裁**的：裁掉 schema 但留着工具，只会让模型看见调不通的工具。
            // 超预算时给出可操作的缩小手段（切阶段 / 停树包），由用户决定。
            s.trimmed = match s.name {
                "tools" => {
                    warnings.push(format!(
                        "tools 段超预算：估算 {} tokens > 上限 {}（工具 schema 不可截断；可切换阶段或停用多余树包来缩小工具面）",
                        s.used, s.cap
                    ));
                    false
                }
                "dialog" => {
                    warnings.push(format!(
                        "dialog 段超预算：估算 {} tokens > 上限 {}（早期轮次将由滚动摘要接管）",
                        s.used, s.cap
                    ));
                    true
                }
                _ => {
                    warnings.push(format!(
                        "{} 段超预算：估算 {} tokens > 上限 {} tokens（已按预算截断）",
                        s.name, s.used, s.cap
                    ));
                    true
                }
            };
        }
    }
    ContextPlan { window, slices, warnings }
}

/// 按 token 预算截断文本（保留头部；返回是否发生截断）。
///
/// 用于知识注入、经验注入、prompt 注入段——**不用于**对话历史（那有专门的滚动摘要）。
pub fn fit_text(text: &str, cap_tokens: u64) -> (String, bool) {
    if cap_tokens == 0 {
        return (String::new(), !text.is_empty());
    }
    if estimate_text_tokens(text) <= cap_tokens as usize {
        return (text.to_string(), false);
    }
    // 二分找最大可容纳字符数（estimate_text_tokens 对中日韩按 1 token/字，对西文按 ~4 字符/token）
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        if estimate_text_tokens(&candidate) <= cap_tokens as usize {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    out.push_str("\n…（超出上下文预算，已截断）");
    (out, true)
}

/// 按 token 预算取前 N 条（逐条累加，超预算即止）。
pub fn fit_lines(lines: &[String], cap_tokens: u64) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut used = 0usize;
    for l in lines {
        let t = estimate_text_tokens(l);
        if used + t > cap_tokens as usize {
            return (out, true);
        }
        used += t;
        out.push(l.clone());
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_sums_to_one_and_is_dominant_in_dialog() {
        let b = Budget::default();
        assert!((b.sum() - 1.0).abs() < 1e-5);
        assert!(b.dialog > 0.5, "对话+观察必须占大头");
    }

    #[test]
    fn bad_budget_falls_back_or_normalizes() {
        let b = Budget { system: -1.0, knowledge: 0.1, experience: 0.1, tools: 0.1, dialog: 0.5 };
        assert_eq!(b.normalized(), Budget::default());
        let b = Budget { system: 1.0, knowledge: 1.0, experience: 1.0, tools: 1.0, dialog: 1.0 };
        assert!((b.normalized().sum() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn plan_marks_over_budget_slices() {
        let p = plan(
            10_000,
            &Budget::default(),
            &Load { system: 100, knowledge: 5000, experience: 100, tools: 100, dialog: 100 },
        );
        let k = p.slice("knowledge").unwrap();
        assert_eq!(k.cap, 1000);
        assert!(k.over() && k.trimmed);
        assert!(p.warnings.iter().any(|w| w.contains("knowledge")));
        assert!(!p.slice("system").unwrap().trimmed);
    }

    #[test]
    fn fit_text_keeps_short_text_and_trims_long() {
        let short = "短文本";
        let (out, trimmed) = fit_text(short, 100);
        assert_eq!(out, short);
        assert!(!trimmed);

        let long = "汉".repeat(5000);
        let (out, trimmed) = fit_text(&long, 100);
        assert!(trimmed);
        assert!(out.contains("已截断"));
        assert!(estimate_text_tokens(&out) <= 130, "截断后应贴近预算");
    }

    #[test]
    fn fit_lines_stops_at_budget() {
        let lines: Vec<String> = (0..100).map(|i| format!("第{i}条内容")).collect();
        let (keep, trimmed) = fit_lines(&lines, 20);
        assert!(trimmed);
        assert!(!keep.is_empty() && keep.len() < 100);
    }

    #[test]
    fn zero_window_is_clamped() {
        let p = plan(0, &Budget::default(), &Load::default());
        assert_eq!(p.window, 1024);
        assert!(p.has_room());
    }
}
