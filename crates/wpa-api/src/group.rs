//! 四类群组插件契约与它们之间的数据模型。
//!
//! 设计要点：
//! - 群组**只向注册表登记能力**，不直接触碰基座内部执行链；
//! - 工具入参/出参用 `serde_json::Value` 与纯数据结构，避免树依赖基座类型；
//! - 共享资源（网络/存储/审计）通过 [`GroupRuntime`] 句柄注入，树不自己造资源。

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{GroupManifest, RiskTier};
use crate::registry::GroupRegistry;
use crate::runtime::GroupRuntime;

// ---------------------------------------------------------------------------
// 工具（业务工具群组）
// ---------------------------------------------------------------------------

/// 树侧工具契约：与基座 `Tool` 同语义，但**不依赖基座 crate**。
#[async_trait]
pub trait ApiTool: Send + Sync + 'static {
    /// 全局唯一工具名，格式 `命名空间.动作`（如 `net.scan`）。
    ///
    /// **工具名只在两个地方出现**：注册表条目里（声明）、调用现场（数据）。
    /// 任何"要做什么"的描述——规划器、领域配置、约束文件、workflow——
    /// 都不得写工具名，只能写[能力标签](crate::registry::caps)。
    /// 理由：工具可安装、可卸载、可被同样能力的另一个实现替换，
    /// 名字一旦被写死就会静默失效（见 `kern_loop::StaticPlanner` 的旧实现）。
    fn name(&self) -> &str;
    fn description(&self) -> String;
    /// JSON Schema（OpenAI function-calling 规范）。
    fn input_schema(&self) -> Value;
    /// 该工具能提供的能力标签（受控词表见 [`crate::registry::caps`]）。
    ///
    /// 规划器只声明能力需求，装配期由
    /// [`GroupRegistry::resolve_needs`](crate::registry::GroupRegistry::resolve_needs)
    /// 翻译成具体工具名。未声明 = 该工具不会被能力解析命中
    /// （仍可由 LLM 按 schema 自主选择），**不做前缀猜测**。
    fn capabilities(&self) -> Vec<String> {
        Vec::new()
    }
    /// 静态典型风险档位（展示用）。
    fn risk(&self) -> RiskTier {
        RiskTier::Low
    }
    /// 参数级风险（审批判定真正依据；未知一律 High 由实现方保证 fail-closed）。
    fn risk_for(&self, _args: &Value) -> RiskTier {
        self.risk()
    }
    /// 是否可与其他工具并行执行（只读、无共享副作用者覆写为 true）。
    fn concurrency_safe(&self, _args: &Value) -> bool {
        false
    }
    async fn call(&self, req: ToolRequest) -> ToolReply;
}

/// 一次工具调用的上下文（由基座装配后传入）。
pub struct ToolRequest {
    pub session_id: String,
    /// 授权范围（目标白名单）；空表示未声明，按 fail-closed 拒绝。
    pub authorized_scope: Vec<String>,
    /// 拒绝清单（「除这些之外」），优先级高于授权范围。
    pub denylist: Vec<String>,
    /// 调用参数。
    pub args: Value,
    /// 共享运行时句柄（HTTP / 存储 / 事件）。
    pub runtime: Arc<dyn GroupRuntime>,
}

/// 工具执行产出：精简回显与结构化明细分道。
#[derive(Debug, Clone, Default)]
pub struct ToolReply {
    pub ok: bool,
    /// 给 LLM 的回显（实现方负责截断）。
    pub summary: String,
    /// 结构化明细（供报告/审计；可空）。
    pub detail: Option<Value>,
    pub duration_ms: u64,
}

impl ToolReply {
    pub fn ok(summary: impl Into<String>, duration_ms: u64) -> Self {
        Self { ok: true, summary: summary.into(), detail: None, duration_ms }
    }

    pub fn json(summary: impl Into<String>, detail: Value, duration_ms: u64) -> Self {
        Self { ok: true, summary: summary.into(), detail: Some(detail), duration_ms }
    }

    pub fn err(summary: impl Into<String>, duration_ms: u64) -> Self {
        Self { ok: false, summary: summary.into(), detail: None, duration_ms }
    }
}

// ---------------------------------------------------------------------------
// 知识（知识群组）
// ---------------------------------------------------------------------------

/// 知识检索命中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeHit {
    pub id: String,
    pub title: String,
    pub body: String,
    /// 相关度（越大越相关；排序由提供方负责）。
    #[serde(default)]
    pub score: f32,
    /// 来源标识（如 `cve:CVE-2021-44228`）。
    #[serde(default)]
    pub source: String,
}

/// 约束查询：按当前任务/阶段/工具，取应遵守的规范条目。
#[derive(Debug, Clone, Default)]
pub struct ConstraintQuery {
    pub task: String,
    pub stage: Option<String>,
    pub tools: Vec<String>,
}

/// 规范条目（知识群组对智能体的"业务约束"输出）。
///
/// 除自然语言外可带**执行元数据**：`Must` 约束只有自带 `tools` / `min_risk` /
/// `target_contains` 才能被编译成 guard 谓词真正拦人；否则会降级为提示并留痕。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Constraint {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub severity: ConstraintSeverity,
    /// 适用工具（精确名或 `前缀.*`）；空 = 不按工具过滤。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 风险门槛（`low`/`medium`/`high`/`critical`）：达到才拦。
    #[serde(default)]
    pub min_risk: Option<String>,
    /// 目标子串：命中调用参数里的目标才拦。
    #[serde(default)]
    pub target_contains: Vec<String>,
}

/// 约束强度：`Must` 违反即拒绝；`Should` 提示；`Info` 仅背景。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstraintSeverity {
    Must,
    #[default]
    Should,
    Info,
}

/// 可沉淀经验（学习接口入参）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Experience {
    pub objective: String,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub techniques: Vec<String>,
    #[serde(default)]
    pub dead_ends: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 知识提供者：解决"专业知识存储、调取、学习"，并为智能体提供规范与约束。
#[async_trait]
pub trait KnowledgeProvider: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// 调取：按查询召回知识片段。
    async fn search(&self, query: &str, limit: usize) -> Vec<KnowledgeHit>;

    /// 约束：返回当前上下文应遵守的规范（注入 system prompt / 审批策略）。
    async fn constraints(&self, _q: &ConstraintQuery) -> Vec<Constraint> {
        Vec::new()
    }

    /// 学习：把一次执行经验写回领域知识。
    async fn learn(&self, _exp: &Experience) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 逻辑（业务逻辑群组）
// ---------------------------------------------------------------------------

/// 规划请求。
#[derive(Debug, Clone)]
pub struct PlanRequest {
    pub objective: String,
    /// 已知上下文（前序观察摘要）。
    pub context: Vec<String>,
    /// 当前可用工具名（上下文收敛后的结果）。
    pub available_tools: Vec<String>,
}

/// 规划产出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub stop_condition: String,
}

/// 计划步骤。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub index: u32,
    pub action: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub risk: RiskTier,
}

/// 业务阶段：把"一次任务"切成若干手段不同的段落（侦察/验证/利用/后渗透…）。
///
/// 基座对阶段**无语义认知**——它只做三件事：把当前阶段与进出条件写进 system prompt、
/// 按 `allowed_groups` 收敛工具面、切换时记审计事件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Phase {
    /// 阶段 id（`recon` / `verify` / `exploit` / `post`）。
    pub id: String,
    /// 展示名（侦察 / 验证 / 利用 / 后渗透）。
    pub label: String,
    /// 进入该阶段的判断依据（给 LLM 看）。
    #[serde(default)]
    pub entry_hint: String,
    /// 该阶段允许的工具组 id；空 = 不限制（沿用全局工具面）。
    #[serde(default)]
    pub allowed_groups: Vec<String>,
    /// 何时算完成（给 LLM 看的退出条件）。
    #[serde(default)]
    pub exit_condition: String,
}

impl Phase {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), ..Default::default() }
    }

    pub fn with_allowed(mut self, groups: &[&str]) -> Self {
        self.allowed_groups = groups.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_hints(mut self, entry: impl Into<String>, exit: impl Into<String>) -> Self {
        self.entry_hint = entry.into();
        self.exit_condition = exit.into();
        self
    }
}

/// 业务决策/流程提供者：编排知识与工具，给出计划与阶段策略。
#[async_trait]
pub trait Planner: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// 生成计划（可由 LLM 驱动，也可由领域规则驱动）。
    async fn plan(&self, req: &PlanRequest) -> Result<Plan, String>;

    /// 阶段定义；空 = 无阶段概念（自由 ReAct）。
    fn phases(&self) -> Vec<Phase> {
        Vec::new()
    }

    /// 按阶段推荐工具（上下文收敛依据）；空表示不干预。
    fn suggest_tools(&self, _stage: Option<&str>) -> Vec<String> {
        Vec::new()
    }

    /// 注入的 prompt 片段（领域工作范式；由基座并入 system prompt）。
    fn prompt_section(&self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// 技能（业务技能群组）
// ---------------------------------------------------------------------------

/// 一个技能包：目录里只暴露 `name` + `trigger`，正文按需加载（渐进披露）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillPack {
    pub name: String,
    pub trigger: String,
    pub body: String,
    #[serde(default)]
    pub domain: Option<String>,
    /// 触发关键词（基座先做关键词粗筛，命中才进目录——降低无关技能的 prompt 占用）。
    #[serde(default)]
    pub keywords: Vec<String>,
    /// 依赖的工具组 id（缺依赖时目录里标灰并提示，不隐藏）。
    #[serde(default)]
    pub requires: Vec<String>,
}

impl SkillPack {
    /// 关键词预筛：空关键词 = 恒命中（无法预筛时保守全给）。
    pub fn matches(&self, query: &str) -> bool {
        if self.keywords.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        self.keywords.iter().any(|k| {
            let k = k.to_lowercase();
            !k.is_empty() && q.contains(&k)
        })
    }

    /// 依赖是否满足（`available_groups` 为当前装配成功的组 id）。
    pub fn deps_ok(&self, available_groups: &[String]) -> bool {
        self.requires.iter().all(|r| available_groups.iter().any(|a| a == r))
    }
}

// ---------------------------------------------------------------------------
// 群组插件
// ---------------------------------------------------------------------------

/// 四类群组插件的统一契约。
///
/// 一个群组 = 一组能力 + 一份自描述清单。基座只调 [`GroupPlugin::register`]，
/// 具体注册什么完全由群组决定（基座不认识业务词汇）。
#[async_trait]
pub trait GroupPlugin: Send + Sync + 'static {
    /// 自描述清单（id/版本/类型/依赖/能力标签/风险）。
    fn manifest(&self) -> GroupManifest;

    /// 装配：把本群组的能力登记到注册表。
    async fn register(&self, reg: &mut GroupRegistry) -> Result<(), String>;

    /// 卸载：释放资源（按反拓扑序调用）。
    async fn shutdown(&self) -> Result<(), String> {
        Ok(())
    }
}

/// 工具群组：声明原子工具集合。
pub trait ToolGroup: GroupPlugin {
    fn tools(&self) -> Vec<Arc<dyn ApiTool>>;
}

/// 知识群组：声明知识提供者集合。
pub trait KnowledgeGroup: GroupPlugin {
    fn providers(&self) -> Vec<Arc<dyn KnowledgeProvider>>;
}

/// 逻辑群组：声明规划器集合。
pub trait LogicGroup: GroupPlugin {
    fn planners(&self) -> Vec<Arc<dyn Planner>>;
}

/// 技能群组：声明技能包集合。
pub trait SkillGroup: GroupPlugin {
    fn skills(&self) -> Vec<SkillPack>;
}

/// 便捷登记：批量注册工具（重名由注册表 fail-closed 拒绝）。
pub fn register_tools(
    reg: &mut GroupRegistry,
    tools: Vec<Arc<dyn ApiTool>>,
) -> Result<(), String> {
    for t in tools {
        reg.register_tool(t)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_constructors() {
        let r = ToolReply::ok("done", 5);
        assert!(r.ok && r.detail.is_none());
        let r = ToolReply::json("done", serde_json::json!({"a":1}), 7);
        assert!(r.ok && r.detail.is_some());
        let r = ToolReply::err("boom", 1);
        assert!(!r.ok);
    }

    #[test]
    fn risk_gate_is_fail_closed() {
        assert!(!RiskTier::None.needs_approval());
        assert!(RiskTier::High.needs_approval());
    }

    #[test]
    fn skill_keyword_prefilter() {
        let mut s = SkillPack { name: "recon".into(), trigger: "侦察".into(), ..Default::default() };
        assert!(s.matches("随便什么"), "无关键词时不做预筛");
        s.keywords = vec!["侦察".into(), "扫描".into()];
        assert!(s.matches("先做一轮侦察"));
        assert!(!s.matches("写个报告"));
    }

    #[test]
    fn skill_requires_gate() {
        let s = SkillPack {
            name: "x".into(),
            requires: vec!["tools.scan".into()],
            ..Default::default()
        };
        assert!(!s.deps_ok(&[]));
        assert!(s.deps_ok(&["tools.scan".to_string()]));
    }

    #[test]
    fn phase_builder() {
        let p = Phase::new("recon", "侦察")
            .with_allowed(&["tools.scan", "knowledge.vulndb"])
            .with_hints("目标已授权且未收集信息", "端口与服务清单完成");
        assert_eq!(p.id, "recon");
        assert_eq!(p.allowed_groups.len(), 2);
        assert!(!p.exit_condition.is_empty());
    }
}
