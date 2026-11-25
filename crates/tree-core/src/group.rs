//! 声明式群组：一个 `PnC.toml` 目录 = 一个群组插件。
//!
//! 四类插口共用同一层"从目录装配"的逻辑：
//! - `tools`：`[[tool]]` → [`DeclarativeTool`]（无需编译）；
//! - `knowledge`：知识文件 → 文件式知识提供者（分级检索 + 约束）；
//! - `logic`：`[[phase]]` + `prompt` → 阶段式规划器（只产出阶段骨架，不越权调工具）；
//! - `skills`：`[[skill]]` → 技能包（渐进披露）。
//!
//! 这是"装树不编译"的物理实现：整棵树的业务能力从数据里长出来。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use wpa_api::{
    ApiTool, GroupKind, GroupManifest, GroupPlugin, Phase, Plan, PlanRequest, PlanStep, Planner,
    RiskTier, SkillPack, GroupRegistry,
};

use crate::exec::{build_tools, PrimitiveInvoker};
use crate::pnc::{parse_risk, PnCManifest};

/// 从目录构建的群组。
pub struct DeclarativeGroup {
    dir: PathBuf,
    pnc: PnCManifest,
    manifest: GroupManifest,
    invoker: Option<Arc<dyn PrimitiveInvoker>>,
    /// 装配期告警（知识文件缺字段、缺少原语等）。
    warnings: Arc<Mutex<Vec<String>>>,
}

impl std::fmt::Debug for DeclarativeGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclarativeGroup")
            .field("id", &self.pnc.id)
            .field("kind", &self.manifest.kind)
            .field("dir", &self.dir)
            .finish()
    }
}

impl DeclarativeGroup {
    pub fn from_dir(dir: impl AsRef<Path>, invoker: Option<Arc<dyn PrimitiveInvoker>>) -> Result<Self, String> {
        let dir = dir.as_ref().to_path_buf();
        let pnc = PnCManifest::load(&dir)?;
        let errs = pnc.validate();
        if !errs.is_empty() {
            return Err(format!("包清单非法（{}）：\n  {}", dir.display(), errs.join("\n  ")));
        }
        let kind = pnc.group_kind()?;
        let mut manifest = GroupManifest::new(
            pnc.id.clone(),
            if pnc.name.trim().is_empty() { pnc.id.clone() } else { pnc.name.clone() },
            &pnc.version,
            kind,
        );
        manifest.depends_on = pnc.deps.clone();
        manifest.capabilities = pnc.capabilities.tags.clone();
        manifest.phases = pnc.capabilities.phases.clone();
        manifest.min_base_version = pnc.min_version();
        manifest.risk = highest_risk(&pnc);
        manifest.description = pnc.description.clone();
        Ok(Self {
            dir,
            pnc,
            manifest,
            invoker,
            warnings: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn pnc(&self) -> &PnCManifest {
        &self.pnc
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().map(|w| w.clone()).unwrap_or_default()
    }

    fn note(&self, msg: impl Into<String>) {
        if let Ok(mut w) = self.warnings.lock() {
            w.push(msg.into());
        }
    }

    /// 声明的阶段（逻辑包；其它类型返回空）。
    pub fn phases(&self) -> Vec<Phase> {
        self.pnc
            .phase
            .iter()
            .filter(|p| !p.id.trim().is_empty())
            .map(|p| Phase {
                id: p.id.clone(),
                label: if p.label.trim().is_empty() { p.id.clone() } else { p.label.clone() },
                entry_hint: p.entry_hint.clone(),
                allowed_groups: p.allowed_groups.clone(),
                exit_condition: p.exit_condition.clone(),
            })
            .collect()
    }

    fn build_prompt(&self) -> Option<String> {
        if let Some(p) = self.pnc.prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            return Some(p.to_string());
        }
        let f = self.pnc.prompt_file.as_deref()?;
        match std::fs::read_to_string(self.dir.join(f)) {
            Ok(t) if !t.trim().is_empty() => Some(t),
            Ok(_) => None,
            Err(e) => {
                self.note(format!("读取 prompt 文件 {f} 失败：{e}"));
                None
            }
        }
    }

    fn build_skills(&self) -> Vec<SkillPack> {
        self.pnc
            .skill
            .iter()
            .filter(|s| !s.name.trim().is_empty())
            .map(|s| {
                let body = match (&s.body, &s.body_file) {
                    (Some(b), _) => b.clone(),
                    (None, Some(f)) => match std::fs::read_to_string(self.dir.join(f)) {
                        Ok(t) => t,
                        Err(e) => {
                            self.note(format!("技能 {} 读取正文 {} 失败：{e}", s.name, f));
                            String::new()
                        }
                    },
                    (None, None) => String::new(),
                };
                if body.trim().is_empty() {
                    self.note(format!("技能 {} 正文为空（目录里会显示但加载后无内容）", s.name));
                }
                SkillPack {
                    name: s.name.clone(),
                    trigger: s.trigger.clone(),
                    body,
                    domain: Some(self.pnc.id.clone()),
                    keywords: s.keywords.clone(),
                    requires: s.requires.clone(),
                }
            })
            .collect()
    }
}

fn highest_risk(pnc: &PnCManifest) -> RiskTier {
    pnc.tool
        .iter()
        .map(|t| parse_risk(t.risk.as_deref()))
        .max()
        .unwrap_or(RiskTier::None)
}

#[async_trait]
impl GroupPlugin for DeclarativeGroup {
    fn manifest(&self) -> GroupManifest {
        self.manifest.clone()
    }

    async fn register(&self, reg: &mut GroupRegistry) -> Result<(), String> {
        match self.manifest.kind {
            GroupKind::Tools => {
                let tools: Vec<Arc<dyn ApiTool>> =
                    build_tools(&self.pnc.tool, &self.dir, &self.pnc.id, self.invoker.clone());
                if tools.is_empty() {
                    return Err(format!("工具包 {} 未声明任何 [[tool]]", self.pnc.id));
                }
                if let Some(inv) = &self.invoker {
                    for t in &self.pnc.tool {
                        if let crate::pnc::ExecSpec::Primitive { primitive, .. } = &t.exec {
                            if !inv.has(primitive) {
                                self.note(format!(
                                    "工具 {} 依赖的原语 {primitive} 未注册（已挂载但调用会失败）",
                                    t.name
                                ));
                            }
                        }
                    }
                }
                for tool in tools {
                    reg.register_tool_in(tool, Some(&self.pnc.id))?;
                }
                Ok(())
            }
            GroupKind::Knowledge => {
                let decl = self.pnc.knowledge.clone().unwrap_or_default();
                let provider = knowledge_core::FileKnowledgeProvider::from_dir(
                    self.pnc.id.clone(),
                    &self.dir,
                    &decl.entries,
                    decl.constraints.as_deref(),
                );
                for w in provider.warnings() {
                    self.note(w.clone());
                }
                if provider.is_empty() {
                    self.note(format!("知识包 {} 未加载到任何条目", self.pnc.id));
                }
                reg.register_knowledge(Arc::new(provider));
                if let Some(p) = &self.pnc.persona {
                    reg.set_persona(p.clone());
                }
                Ok(())
            }
            GroupKind::Logic => {
                let phases = self.phases();
                let planner = DeclarativePlanner {
                    name: self.pnc.id.clone(),
                    phases,
                    prompt: self.build_prompt(),
                    persona: self.pnc.persona.clone(),
                };
                if planner.phases.is_empty() && planner.prompt.is_none() {
                    return Err(format!(
                        "逻辑包 {} 既没有 [[phase]] 也没有 prompt，装配后不产生任何效果",
                        self.pnc.id
                    ));
                }
                if let Some(p) = &self.pnc.persona {
                    reg.set_persona(p.clone());
                }
                reg.register_planner(Arc::new(planner));
                Ok(())
            }
            GroupKind::Skills => {
                let skills = self.build_skills();
                if skills.is_empty() {
                    return Err(format!("技能包 {} 未声明任何 [[skill]]", self.pnc.id));
                }
                for s in skills {
                    reg.register_skill(s);
                }
                Ok(())
            }
        }
    }
}

/// 声明式规划器：阶段 + prompt 的提供者。
///
/// **诚实边界**：它不调 LLM、不调工具，`plan()` 只把阶段展开成骨架步骤。
/// 真正的逐步决策仍由基座主循环（ReAct）完成——规划器越权会让审批链失效。
pub struct DeclarativePlanner {
    pub name: String,
    pub phases: Vec<Phase>,
    pub prompt: Option<String>,
    pub persona: Option<String>,
}

#[async_trait]
impl Planner for DeclarativePlanner {
    fn name(&self) -> &str {
        &self.name
    }

    async fn plan(&self, req: &PlanRequest) -> Result<Plan, String> {
        if self.phases.is_empty() {
            return Err("本逻辑包未定义阶段，无法生成骨架计划".into());
        }
        let steps = self
            .phases
            .iter()
            .enumerate()
            .map(|(i, p)| PlanStep {
                index: i as u32 + 1,
                action: format!("{}：{}", p.label, p.exit_condition),
                tools: Vec::new(),
                risk: RiskTier::Low,
            })
            .collect();
        Ok(Plan {
            goal: req.objective.clone(),
            steps,
            stop_condition: self
                .phases
                .last()
                .map(|p| p.exit_condition.clone())
                .unwrap_or_default(),
        })
    }

    fn phases(&self) -> Vec<Phase> {
        self.phases.clone()
    }

    fn prompt_section(&self) -> Option<String> {
        self.prompt.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tree-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn tools_group_registers_declarative_tools() {
        let d = tmp("tools");
        write(
            &d,
            "PnC.toml",
            r#"
id = "tools.demo"
kind = "tools"
name = "演示工具"
version = "1.0.0"
[capabilities]
tags = ["demo"]
phases = ["recon"]

[[tool]]
name = "demo.ping"
description = "回显"
risk = "low"
exec = { kind = "command", program = "cmd", args = ["/C", "echo", "hi"] }
"#,
        );
        let g = DeclarativeGroup::from_dir(&d, None).expect("应加载");
        assert_eq!(g.manifest().kind, GroupKind::Tools);
        assert_eq!(g.manifest().phases, vec!["recon"]);
        assert_eq!(g.manifest().risk, RiskTier::Low);
        let mut reg = GroupRegistry::new();
        g.register(&mut reg).await.expect("应装配");
        assert_eq!(reg.tool_names(), vec!["demo.ping"]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[tokio::test]
    async fn knowledge_group_registers_provider() {
        let d = tmp("kb");
        write(
            &d,
            "PnC.toml",
            r#"
id = "knowledge.demo"
kind = "knowledge"
version = "1.0.0"
[knowledge]
entries = ["kb.jsonl"]
constraints = "rules.toml"
"#,
        );
        let mut f = std::fs::File::create(d.join("kb.jsonl")).unwrap();
        writeln!(f, r#"{{"id":"K1","title":"条目一","body":"内容"}}"#).unwrap();
        write(&d, "rules.toml", "[[constraint]]\nid=\"R1\"\ntext=\"留痕\"\nseverity=\"must\"\n");
        let g = DeclarativeGroup::from_dir(&d, None).unwrap();
        let mut reg = GroupRegistry::new();
        g.register(&mut reg).await.unwrap();
        assert_eq!(reg.knowledge().len(), 1);
        let hits = reg.knowledge()[0].search("条目", 3).await;
        assert_eq!(hits[0].id, "K1");
        std::fs::remove_dir_all(&d).ok();
    }

    #[tokio::test]
    async fn logic_group_exposes_phases_and_prompt() {
        let d = tmp("logic");
        write(
            &d,
            "PnC.toml",
            r#"
id = "logic.demo"
kind = "logic"
version = "1.0.0"
prompt = "先侦察再动手"

[[phase]]
id = "recon"
label = "侦察"
entry_hint = "未收集信息"
exit_condition = "清单完成"
allowed_groups = ["tools.demo"]
"#,
        );
        let g = DeclarativeGroup::from_dir(&d, None).unwrap();
        assert_eq!(g.phases().len(), 1);
        let mut reg = GroupRegistry::new();
        g.register(&mut reg).await.unwrap();
        assert_eq!(reg.planners().len(), 1);
        assert_eq!(reg.prompt_sections(), &["先侦察再动手".to_string()]);
        let plan = reg.planners()[0]
            .plan(&PlanRequest {
                objective: "打点".into(),
                context: vec![],
                available_tools: vec![],
            })
            .await
            .unwrap();
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn invalid_package_is_rejected_with_reason() {
        let d = tmp("bad");
        write(&d, "PnC.toml", "id = \"\"\nversion = \"x\"\nkind = \"nope\"\n");
        let err = DeclarativeGroup::from_dir(&d, None).unwrap_err();
        assert!(err.contains("包清单非法"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[tokio::test]
    async fn skills_group_registers_packs() {
        let d = tmp("skills");
        write(
            &d,
            "PnC.toml",
            r#"
id = "skills.demo"
kind = "skills"
version = "1.0.0"

[[skill]]
name = "recon-basic"
trigger = "需要做资产发现时"
body_file = "recon.md"
keywords = ["侦察", "recon"]
requires = ["tools.demo"]
"#,
        );
        write(&d, "recon.md", "# 侦察步骤\n1. 存活\n2. 端口\n");
        let g = DeclarativeGroup::from_dir(&d, None).unwrap();
        let mut reg = GroupRegistry::new();
        g.register(&mut reg).await.unwrap();
        let skills = reg.skills();
        assert_eq!(skills.len(), 1);
        assert!(skills[0].body.contains("存活"));
        assert!(skills[0].matches("做一轮侦察"));
        std::fs::remove_dir_all(&d).ok();
    }
}
