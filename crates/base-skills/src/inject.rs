//! 技能检索注入：在规划期把候选技能按需注入上下文。

use crate::skill::{Skill, SkillIndex};

/// 注入深度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectLevel {
    /// 只有触发条件与名称摘要
    Head,
    /// header + body 全文
    Full,
}

/// 针对待识别目标文本，召回候选技能并生成注入文本。
pub struct SkillInjector<'a> {
    index: &'a SkillIndex,
    max_candidates: usize,
}

impl<'a> SkillInjector<'a> {
    pub fn new(index: &'a SkillIndex) -> Self {
        Self {
            index,
            max_candidates: 3,
        }
    }

    pub fn with_max(mut self, n: usize) -> Self {
        self.max_candidates = n.max(1);
        self
    }

    /// 基于目标使用场景文本做关键词召回。
    pub fn recall(&self, intent: &str) -> Vec<&Skill> {
        let intent_lower = intent.to_ascii_lowercase();
        let mut scored: Vec<(usize, &Skill)> = self
            .index
            .skills
            .iter()
            .map(|s| {
                let text = format!(
                    "{} {} {}",
                    s.manifest.name,
                    s.manifest.trigger,
                    s.manifest.deps.join(" ")
                )
                .to_ascii_lowercase();
                let score = count_score(&text, &intent_lower);
                (score, s)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        scored
            .into_iter()
            .take(self.max_candidates)
            .map(|(_, s)| s)
            .collect()
    }

    /// 生成注入文本。
    pub fn render(&self, skills: &[&Skill], level: InjectLevel) -> String {
        let mut buf = String::from("# 可用的技能（按需启用）\n");
        for s in skills {
            buf.push_str(&format!(
                "## {} (v{})\n- 触发: {}\n",
                s.manifest.name, s.manifest.version, s.manifest.trigger
            ));
            if level == InjectLevel::Full && !s.manifest.body.is_empty() {
                buf.push('\n');
                buf.push_str(&s.manifest.body);
                buf.push('\n');
            }
        }
        buf
    }
}

fn count_score(hay: &str, needle: &str) -> usize {
    needle
        .split_whitespace()
        .map(|tok| {
            let t = tok.trim_matches(|c: char| !c.is_alphanumeric());
            if t.len() < 2 {
                0
            } else if hay.contains(t) {
                t.len()
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_matches_trigger() {
        let mut idx = SkillIndex::new();
        idx.skills.push(demo_skill("footprint", "web 应用指纹识别"));
        idx.skills.push(demo_skill("portscan", "端口扫描探活"));
        let inj = SkillInjector::new(&idx);
        let hit: Vec<&str> = inj
            .recall("对 web 应用做指纹识别")
            .iter()
            .map(|s| s.manifest.name.as_str())
            .collect();
        assert!(hit.contains(&"footprint"));
    }

    fn demo_skill(name: &str, trigger: &str) -> Skill {
        Skill {
            manifest: crate::skill::SkillManifest {
                name: name.into(),
                version: "1".into(),
                trigger: trigger.into(),
                deps: vec![],
                min_app_version: None,
                body: format!("# {name}\nbody"),
                path: std::path::PathBuf::new(),
            },
            assets: vec![],
        }
    }
}
