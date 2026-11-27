//! 对话引擎：标准智能体 Bot 的核心循环。
//!
//! - LLM 在线：ReAct 循环 —— system(角色+授权范围+工具规范) → 模型 tool_calls
//!   → 审批门 → 工具执行 → 观察回填 → 再生成 → … → 最终答复（harness/hermes 范式）；
//! - LLM 缺席：规则降级 —— 意图关键词 + 目标提取 → 直接执行工具 → 自然语言汇总；
//! - 审批：Medium 及以上风险工具经注入的审批回调裁决（REPL 终端 y/N / serve 策略）。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use base_llm::{
    assistant_message, system_message, user_message, ChatMessage, ChatRole, Completion, LlmError,
    LlmProvider, ToolSchema,
};
use base_store::ExperienceStore;
use futures_util::StreamExt;
use kern_core::exec::ToolOutcome;
use kern_core::model::{AgentId, ExecutionContext, RiskLevel, SessionId, TaskId};
use kern_core::ToolExecutor;
use serde_json::{json, Value};

/// 审批回调：工具名、风险级别、参数摘要 → 是否放行。
pub type ApprovalCallback = Arc<dyn Fn(&str, RiskLevel, &str) -> bool + Send + Sync>;

/// 技能注入单元（来自树包提供的技能，渐进披露）。
#[derive(Debug, Clone, Default)]
pub struct SkillNote {
    pub name: String,
    pub trigger: String,
    pub body: String,
    /// 触发关键词（空 = 无法预筛，恒进目录）。
    pub keywords: Vec<String>,
    /// 依赖是否满足（工具组没装上 → 不进目录：教模型用不存在的工具只会浪费轮次）。
    pub available: bool,
}

impl SkillNote {
    pub fn new(name: impl Into<String>, trigger: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            trigger: trigger.into(),
            body: body.into(),
            keywords: vec![],
            available: true,
        }
    }

    /// 关键词预筛（大小写不敏感；空关键词视为恒命中）。
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
}

/// 技能目录条目上限：超过则只列「与当前输入相关」的前 N 个。
/// 技能是渐进披露的（目录只有名字+触发场景），但条目太多仍会稀释注意力。
const SKILL_DIR_CAP: usize = 12;

/// 工具面过滤器：返回 false 的工具不进本轮 schema（阶段收敛/空间白名单的通用挂点）。
pub type ToolFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// 一轮对话的摘要素材（窗口缓存，供每 5 轮摘要使用）。
#[derive(Debug, Clone)]
struct RecentTurn {
    input: String,
    reply: String,
    trace: Vec<String>,
}

/// R4-1 准备产物：早退观察（denylist/钩子/审批拒绝）或可执行的装载项。
enum Prepared {
    Denied(String),
    Ready {
        tool_name: String,
        args: Value,
        ctx: ExecutionContext,
        guard_note: Option<&'static str>,
    },
}

/// 一轮对话产出。
#[derive(Debug, Clone, Default)]
pub struct ChatTurn {
    /// 给用户的最终自然语言答复
    pub reply: String,
    /// 本轮工具调用轨迹（供 UI / REPL 展示）
    pub trace: Vec<String>,
    /// 是否正常完成（false = 模型/基础设施级失败，如 API 报错）
    pub ok: bool,
    /// 本轮 token 用量（ReAct 各轮累加；降级模式为零值）
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// 会话完整状态快照（R1-3 v2，dsh「模型可见即已记录」对齐）：
/// 协议历史（含 tool_calls/tool 结果）+ 滚动摘要 + 压缩归档，可整体落盘与还原。
#[derive(Debug, Clone, Default)]
pub struct ChatSnapshot {
    pub history: Vec<ChatMessage>,
    pub rolling_summary: Option<String>,
    pub archived: Vec<ChatMessage>,
}

/// 对话过程事件：供 GUI 实时呈现工具轨迹与审批请求（harness 式过程可见性）。
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// ReAct 一轮推理开始（round 从 1 计）
    RoundStarted { round: usize },
    /// 模型中间轮的思考文本（ReAct 显性化：计划/推理说明，非最终答复）
    AssistantNote { round: usize, text: String },
    /// R4-2：流式 content 增量（思考卡片逐字追加；非流式 provider 不发此事件）
    AssistantDelta { round: usize, delta: String },
    /// 工具开始执行
    ToolStarted {
        tool: String,
        args_summary: String,
    },
    /// 工具执行结束
    ToolFinished {
        tool: String,
        ok: bool,
        summary: String,
        duration_ms: u64,
        /// 完整结果明细（供 UI 复核卡片/详情展示；为空表示无额外明细）
        detail: String,
    },
    /// 高风险工具请求人工审批（审批回调可阻塞等待裁决）
    ApprovalRequested {
        tool: String,
        risk: String,
        args_summary: String,
    },
    /// 审批被拒绝
    ApprovalDenied { tool: String },
    /// 审计事件对（R2-4）之 decided：与 ApprovalRequested(asked) 配对落轨迹，可回放审计
    ApprovalDecision {
        tool: String,
        /// true=放行（once 或白名单），false=拒绝
        allowed: bool,
        /// once=一次性放行 session=本会话白名单 deny=拒绝 timeout=超时拒绝
        via: String,
    },
    /// 上下文压缩：旧历史已由 LLM 总结为滚动摘要
    MemoryCompressed {
        /// 压缩前历史条数
        compressed: usize,
        /// 压缩后保留条数
        kept: usize,
    },
    /// 本轮从对话中抽取并入库的经验条数
    ExperienceSaved { count: usize },
}

/// 事件回调：在会话线程上同步调用；允许阻塞（如等待人工审批），禁止重入会话。
pub type ChatEventCallback = Arc<dyn Fn(ChatEvent) + Send + Sync>;

/// 基座默认角色设定（通用智能体，不含任何专业领域语义）。
/// 专业智能树通过 [`ChatConfig::persona`] 注入领域角色，基座代码无需改动。
pub const DEFAULT_PERSONA: &str = "你是 windpencilagent，一个通用智能体。\
用户只给出顶层目标，不会分步指挥你：理解目标、自主拆解子任务、决定先后顺序、自主选择动作、\
根据每步观察反思调整，直到整体目标完成或判定无法完成并如实汇报。";

/// 会话配置。
pub struct ChatConfig {
    pub session_id: SessionId,
    pub authorized_scope: Vec<String>,
    /// 拒绝清单（「除 XXX 之外」）：命中即拒绝执行，优先于 authorized_scope
    pub denylist: Vec<String>,
    /// tool-calling 最大轮数（防失控）
    pub max_tool_rounds: usize,
    /// 历史窗口条数（不含 system，每轮 user+assistant 各一条）
    pub history_cap: usize,
    /// 上下文记忆：历史超过窗口时是否滚动总结（需 LLM 在线）
    pub summarize_history: bool,
    /// 对话摘要间隔（每 N 轮对最近 N 轮做一次摘要并写入经验库；0=关闭；默认 5）
    pub extract_every: usize,
    /// system prompt 注入的经验条数上限
    pub experience_inject: usize,
    /// 经验注入 token 预算（R1-4，默认 1500）：召回条目逐条累加，超预算即止
    pub experience_token_budget: usize,
    /// 上下文窗口 token 上限（用于 80% 阈值自动压缩）。0=按 history_cap 条数兜底。
    pub context_window_tokens: u64,
    /// 自动压缩触发占比（默认 0.8）
    pub compact_threshold: f64,
    /// 压缩时保留的最近原始轮数（3~5，防止工具调用链错乱）
    pub keep_recent_turns: usize,
    /// 是否处于 agent 模式（携带 tools）。chat 模式不向 LLM 传工具面。
    pub agent_mode: bool,
    /// 角色设定（persona）。None = 基座默认「通用智能体」。
    /// 专业智能树由群组注入领域角色（如某领域专家），基座自身不含任何领域措辞。
    pub persona: Option<String>,
    /// 业务群组注入的 prompt 片段（工作范式 / 领域约束），按序追加在基座骨架之后。
    pub prompt_sections: Vec<String>,
    /// 反思层：一轮结束后对失败做复盘，结论写进经验库（默认开）
    pub reflect: bool,
    /// 触发反思的最小失败次数（默认 2）
    pub reflect_min_failures: usize,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            session_id: kern_core::model::SessionId::new(),
            authorized_scope: vec![],
            denylist: vec![],
            max_tool_rounds: 8,
            history_cap: 24,
            summarize_history: true,
            extract_every: 5,
            experience_inject: 6,
            experience_token_budget: 1500,
            context_window_tokens: 32_000,
            compact_threshold: 0.8,
            keep_recent_turns: 4,
            agent_mode: true,
            persona: None,
            prompt_sections: vec![],
            reflect: true,
            reflect_min_failures: 2,
        }
    }
}

/// 聊天会话：持有 LLM / 执行器 / 工具 schema / 历史 / 记忆。
pub struct ChatSession {
    provider: Option<Arc<dyn LlmProvider>>,
    executor: Arc<dyn ToolExecutor>,
    schemas: Vec<ToolSchema>,
    risks: HashMap<String, RiskLevel>,
    /// 每轮工具面过滤器（阶段收敛用）：随阶段变化生效，不必重建会话。
    /// 返回 false 的工具**不进本轮 schema**，模型看不见也就不会调。
    tool_filter: Option<ToolFilter>,
    cfg: ChatConfig,
    history: VecDeque<ChatMessage>,
    approval: ApprovalCallback,
    on_event: Option<ChatEventCallback>,
    turn_no: u64,
    /// 已启用技能（system prompt 注入）
    skills: Vec<SkillNote>,
    /// 经验库（跨会话持久化；None=关闭）
    experience: Option<Arc<ExperienceStore>>,
    /// 滚动历史摘要（上下文压缩产物，随会话延续）
    rolling_summary: Option<String>,
    /// 最近 N 轮对话素材（按摘要间隔窗口滚动）
    recent_turns: VecDeque<RecentTurn>,
    /// 审批模式：None=逐次询问（默认），Some(true)=本会话全部放行，Some(false)=本会话全部拒绝
    approval_mode: Option<bool>,
    /// 会话工具白名单（R2-4）：用户对单工具「本会话放行」授权，重建会话后经 set_allowlist 恢复
    session_allowlist: std::collections::HashSet<String>,
    /// 结构化任务状态（程序维护，模型只读摘要）：主干计划与失败计数
    plan_text: Option<String>,
    /// 工具连续失败计数（Critic：≥3 注入调整/终止提示）
    consecutive_failures: usize,
    /// spill 全文落盘（R1-2，dsh 对齐）：None=关闭，超阈值 detail 仅截断
    spill: Option<Arc<base_store::SpillStore>>,
    /// 压缩淘汰的原始消息归档（R1-3 可逆压缩：只遮蔽不删除；上限 4000 条防膨胀）
    archived: Vec<ChatMessage>,
    /// 任务库（R2-1，dsh TodoWrite 投影：agent 清单每轮注入；None=关闭）
    tasks: Option<Arc<base_store::TaskStore>>,
    /// 目标状态机（R2-2，dsh goal 语义：完成需人类背书；None=关闭）
    goals: Option<Arc<base_store::GoalStore>>,
    /// 会话库（R5-3，/recall 旧会话召回；None=关闭）
    sessions: Option<Arc<base_store::SessionStore>>,
    /// 本会话在会话库里的记录 id（首轮写入时创建；None = 还没落过盘）
    persist_id: Option<String>,
    /// 本会话归属的空间/模型档案（落盘时用；会话列表按空间分类）
    persist_space: String,
    persist_profile: String,
    /// 工作区根（R5-4，AGENTS.md 两级发现的工作区级；None=仅全局级）
    workspace_root: Option<std::path::PathBuf>,
    /// 插件前置钩子链（R3-1，dsh guard 语义）：denylist 后、审批前裁决
    hooks: Vec<toolkit_core::hooks::DynHook>,
    /// 重复调用守卫（R2-3）：上次调用的规范化指纹 + 连续重复计数
    repeat_key: Option<u64>,
    repeat_count: usize,
    /// 结构化计划（规划层）：与 plan.* 工具共享同一份状态；None = 未启用计划层
    plan: Option<crate::plan_state::SharedPlan>,
    /// 本轮工具失败次数（反思层的触发依据；每轮开始时清零）
    turn_failed: usize,
}

impl ChatSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Option<Arc<dyn LlmProvider>>,
        executor: Arc<dyn ToolExecutor>,
        schemas: Vec<ToolSchema>,
        risks: HashMap<String, RiskLevel>,
        cfg: ChatConfig,
        approval: ApprovalCallback,
    ) -> Self {
        Self {
            provider,
            executor,
            schemas,
            risks,
            tool_filter: None,
            cfg,
            history: VecDeque::new(),
            approval,
            on_event: None,
            turn_no: 0,
            skills: Vec::new(),
            experience: None,
            rolling_summary: None,
            recent_turns: VecDeque::new(),
            approval_mode: None,
            session_allowlist: std::collections::HashSet::new(),
            plan_text: None,
            consecutive_failures: 0,
            spill: None,
            archived: Vec::new(),
            tasks: None,
            goals: None,
            sessions: None,
            persist_id: None,
            persist_space: String::new(),
            persist_profile: String::new(),
            workspace_root: None,
            hooks: Vec::new(),
            repeat_key: None,
            repeat_count: 0,
            plan: None,
            turn_failed: 0,
        }
    }

    /// 注入 spill 存储（R1-2）：detail 超阈值时全文落盘并回填 locator。
    pub fn set_spill_store(&mut self, store: Option<Arc<base_store::SpillStore>>) {
        self.spill = store;
    }

    /// 注入 spill 存储（builder 风格，与 with_memory 一致）。
    pub fn with_spill(mut self, store: Arc<base_store::SpillStore>) -> Self {
        self.spill = Some(store);
        self
    }

    /// 注入任务库（R2-1，dsh TodoWrite 语义）：agent 用 task.todo 整表维护清单，
    /// 每轮把最新清单注入上下文，看板 UI 由 ToolFinished 事件驱动刷新。
    pub fn with_tasks(mut self, store: Arc<base_store::TaskStore>) -> Self {
        self.tasks = Some(store);
        self
    }

    /// 注册插件前置钩子（R3-1）：可多次调用叠加成链；denylist 后、审批前裁决。
    pub fn with_hook(mut self, hook: toolkit_core::hooks::DynHook) -> Self {
        self.hooks.push(hook);
        self
    }

    /// 注入会话库（R5-3）：启用 /recall 旧会话召回，并开启逐轮落盘。
    pub fn with_sessions(mut self, store: Arc<base_store::SessionStore>) -> Self {
        self.sessions = Some(store);
        self
    }

    /// 设置会话归属（空间/模型档案）：落盘时写入 SessionRecord，供会话列表按空间分类。
    pub fn set_persist_identity(&mut self, space: impl Into<String>, profile: impl Into<String>) {
        self.persist_space = space.into();
        self.persist_profile = profile.into();
    }

    /// 绑定到一个已存在的会话记录（打开历史会话时）：后续轮次续写原记录。
    pub fn set_persist_id(&mut self, id: Option<String>) {
        self.persist_id = id;
    }

    /// 把一条已落盘的会话记录还原成完整协议快照（打开历史会话用）。
    pub fn session_snapshot(rec: &base_store::SessionRecord) -> ChatSnapshot {
        let conv = |msgs: &[base_store::StoredMessage]| -> Vec<ChatMessage> {
            msgs.iter()
                .map(|m| {
                    let role = match m.role {
                        1 => ChatRole::Assistant,
                        3 => ChatRole::System,
                        5 => ChatRole::Tool,
                        _ => ChatRole::User, // 0=user；2=轨迹、4=轮次分隔按用户文本展示
                    };
                    let tool_calls = m
                        .tool_calls
                        .as_deref()
                        .and_then(|s| serde_json::from_str(s).ok());
                    ChatMessage { role, content: m.text.clone(), tool_calls, tool_call_id: m.tool_call_id.clone() }
                })
                .collect()
        };
        if let Some(st) = &rec.state {
            ChatSnapshot {
                history: conv(&st.messages),
                rolling_summary: st.rolling_summary.clone(),
                archived: conv(&st.archived),
            }
        } else {
            // 旧记录：只有展示行，没有结构化 state
            ChatSnapshot { history: conv(&rec.messages), rolling_summary: None, archived: Vec::new() }
        }
    }

    /// 把本轮对话写进会话库。
    ///
    /// 为什么放在 `turn()` 而不是"退出时整段保存"：进程可能被直接关掉（窗口关、
    /// 任务管理器结束），只有逐轮落盘才保证"问过的东西一定还记得"。
    ///
    /// 失败一律降级（只记 warn）：记忆是增强能力，不能因为它写盘失败而让本轮回答失败。
    /// 同一轮不会重复建记录（`persist_id` 缓存），所以反复调用是安全的。
    fn persist_turn(&mut self, input: &str, turn: &ChatTurn) {
        let Some(store) = self.sessions.clone() else {
            return;
        };
        let id = match self.persist_id.clone() {
            Some(id) => id,
            None => match store.create(
                &base_store::SessionRecord::title_from(input),
                &self.persist_space,
                &self.persist_profile,
            ) {
                Ok(rec) => {
                    self.persist_id = Some(rec.id.clone());
                    rec.id
                }
                Err(e) => {
                    tracing::warn!(error = %e, "会话登记失败（记忆层降级为不落盘）");
                    return;
                }
            },
        };
        // role：0=用户 1=智能体（见 base_store::StoredMessage）
        let lines = [
            base_store::StoredMessage::plain(0, input.to_string()),
            base_store::StoredMessage::plain(1, turn.reply.clone()),
        ];
        if let Err(e) = store.append_messages(&id, &lines) {
            tracing::warn!(error = %e, "会话落盘失败（本轮回答不受影响）");
            return;
        }
        let _ = store.update_stats(&id, Some(self.turn_no), None, None, None);
    }

    /// 注入工作区根（R5-4）：工作区级 AGENTS.md 发现位置；None=仅全局级生效。
    pub fn with_workspace_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.workspace_root = root;
        self
    }

    /// 注入目标状态机（R2-2，dsh goal 语义）：goal.state 上报状态，
    /// 模型自报完成转为待背书并触发 HITL 审批；每轮回注目标状态。
    pub fn with_goals(mut self, store: Arc<base_store::GoalStore>) -> Self {
        self.goals = Some(store);
        self
    }

    /// 设置审批模式：None=恢复逐次询问；Some(true)=本会话全部放行；Some(false)=本会话全部拒绝。
    pub fn set_approval_mode(&mut self, mode: Option<bool>) {
        self.approval_mode = mode;
    }

    /// R2-4：会话工具白名单整表恢复（worker 重建会话后调用）。
    pub fn set_allowlist(&mut self, tools: Vec<String>) {
        self.session_allowlist = tools.into_iter().collect();
    }

    /// R2-4：把单工具加入会话白名单（「本会话放行该工具」）。
    pub fn allow_tool(&mut self, tool: &str) {
        self.session_allowlist.insert(tool.to_string());
    }

    /// 当前审批模式（UI 徽标展示用）。
    pub fn approval_mode(&self) -> Option<bool> {
        self.approval_mode
    }

    /// 挂载过程事件回调（GUI / 监听方实时呈现工具轨迹）。
    /// 接上结构化计划状态（同一份 Arc 也给 plan.* 工具，模型改的进度这里立刻可见）。
    pub fn with_plan(mut self, plan: crate::plan_state::SharedPlan) -> Self {
        self.plan = Some(plan);
        self
    }

    /// 当前计划快照（UI 展示 / 测试断言用）。
    pub fn plan_snapshot(&self) -> Option<crate::plan_state::PlanState> {
        self.plan.as_ref().and_then(|p| p.lock().ok().map(|g| g.clone()))
    }

    pub fn with_events(mut self, cb: ChatEventCallback) -> Self {
        self.on_event = Some(cb);
        self
    }

    /// 挂载经验库（上下文记忆：抽取 + 召回注入）。
    pub fn with_memory(mut self, store: Arc<ExperienceStore>) -> Self {
        self.experience = Some(store);
        self
    }

    /// 注入已启用技能（整体替换，热生效）。
    pub fn with_skills(mut self, notes: Vec<SkillNote>) -> Self {
        self.skills = notes;
        self
    }

    /// 恢复历史消息与轮次（切换会话用）：重建上下文与经验窗口。
    pub fn restore_history(&mut self, history: Vec<ChatMessage>, turn_no: u64) {
        self.history = history.into();
        self.turn_no = turn_no;
        self.recent_turns.clear();
        // 从历史中重建最近 N 轮素材（每轮 user + assistant 两条）。
        let window = self.cfg.extract_every.max(1);
        let msgs: Vec<ChatMessage> = self.history.iter().cloned().collect();
        let mut i = msgs.len();
        while i >= 2 && self.recent_turns.len() < window {
            i -= 2;
            if msgs[i].role == ChatRole::User && msgs[i + 1].role == ChatRole::Assistant {
                self.recent_turns.push_front(RecentTurn {
                    input: msgs[i].content.clone(),
                    reply: msgs[i + 1].content.clone(),
                    trace: Vec::new(),
                });
            }
        }
    }

    /// 当前输入/答复轮次（UI 会话统计用）。
    pub fn turn_no(&self) -> u64 {
        self.turn_no
    }

    /// 当前历史快照（切换模型时保留上下文）。
    pub fn history_snapshot(&self) -> Vec<ChatMessage> {
        self.history.iter().cloned().collect()
    }

    /// 完整状态快照（R1-3 v2，dsh「模型可见即已记录」）：协议历史 + 滚动摘要 + 压缩归档。
    /// worker 每轮结束后取此快照落盘 SessionState。
    pub fn state_snapshot(&self) -> ChatSnapshot {
        ChatSnapshot {
            history: self.history_snapshot(),
            rolling_summary: self.rolling_summary.clone(),
            archived: self.archived.clone(),
        }
    }

    /// 恢复完整状态（R1-3 v2）：history + rolling_summary + archived 一次还原，
    /// 使重启后的会话与压缩前行为一致（协议消息含 tool_calls 原样回填）。
    pub fn restore_full(&mut self, snap: ChatSnapshot, turn_no: u64) {
        self.restore_history(snap.history, turn_no);
        self.rolling_summary = snap.rolling_summary;
        self.archived = snap.archived;
    }

    /// 动态更新工具面（schema + 风险表）：外部工具注册/移除后热生效，
    /// 下一轮 tool-calling 即可看到新工具面，历史上下文保留。
    pub fn update_tools(
        &mut self,
        schemas: Vec<ToolSchema>,
        risks: HashMap<String, RiskLevel>,
    ) {
        self.schemas = schemas;
        self.risks = risks;
    }

    /// 动态更新技能面（技能安装/启用禁用后热生效）。
    pub fn update_skills(&mut self, notes: Vec<SkillNote>) {
        self.skills = notes;
    }

    /// 装/卸工具面过滤器（阶段收敛）：每轮调用前求值，因此阶段切换**会话内即时生效**。
    pub fn with_tool_filter(mut self, filter: ToolFilter) -> Self {
        self.tool_filter = Some(filter);
        self
    }

    /// 本赛季可见的工具面（过滤器为空 = 全量）。
    fn visible_schemas(&self) -> Vec<ToolSchema> {
        match &self.tool_filter {
            None => self.schemas.clone(),
            Some(f) => self.schemas.iter().filter(|s| f(&s.name)).cloned().collect(),
        }
    }

    /// 当前滚动摘要（观测用）。
    pub fn rolling_summary(&self) -> Option<&str> {
        self.rolling_summary.as_deref()
    }

    /// R5-3（dsh /recall 语义）：把旧会话滚动摘要 + 关键词命中的经验注入当前
    /// 会话窗口（system 消息持久入列），后续轮次均可引用。不做物理合并——
    /// 旧会话本身保留完整可再召回。
    fn recall_session(&mut self, id: &str) -> ChatTurn {
        let fail = |msg: String| ChatTurn {
            reply: msg,
            trace: Vec::new(),
            ok: false,
            prompt_tokens: 0,
            completion_tokens: 0,
        };
        let Some(store) = &self.sessions else {
            return fail("会话库未启用（/recall 不可用）".into());
        };
        let id = id.trim();
        if id.is_empty() {
            return fail("用法：/recall <会话id>（可用 /sessions 列出）".into());
        }
        let Some(rec) = store.get(id) else {
            return fail(format!("未找到会话 {id}"));
        };
        let summary = rec
            .state
            .as_ref()
            .and_then(|s| s.rolling_summary.clone())
            .unwrap_or_default();
        // 关键词召回：标题 + 摘要片段作为查询
        let query = format!(
            "{} {}",
            rec.title,
            summary.chars().take(200).collect::<String>()
        );
        let exps = self
            .experience
            .as_ref()
            .map(|e| e.recall(&query, 3))
            .unwrap_or_default();

        let mut note = format!(
            "[会话召回 /recall {id}「{}」]\n旧会话滚动摘要：\n{}\n",
            rec.title,
            if summary.is_empty() { "（旧会话过短或未触发压缩，无摘要）" } else { summary.as_str() }
        );
        if !exps.is_empty() {
            note.push_str("\n相关经验：\n");
            for e in &exps {
                note.push_str(&format!("- {}\n", e.text));
            }
        }
        self.history.push_back(system_message(note));
        ChatTurn {
            reply: format!(
                "已注入旧会话「{}」（{id}）的滚动摘要{}，可继续基于该上下文提问。",
                rec.title,
                if exps.is_empty() { String::new() } else { format!("与 {} 条相关经验", exps.len()) }
            ),
            trace: vec![format!("[OK] /recall {id} → {}「{}」", id, rec.title)],
            ok: true,
            prompt_tokens: 0,
            completion_tokens: 0,
        }
    }

    /// R4-2：LLM 调用入口——支持流式的 provider 走 SSE 增量（AssistantDelta 事件
    /// 驱动思考卡片逐字追加），失败或非流式 provider 自动降级 complete。
    async fn call_llm(
        &self,
        round: usize,
        provider: &dyn LlmProvider,
        msgs: &[ChatMessage],
    ) -> Result<Completion, LlmError> {
        // 每轮重算可见工具面：阶段收敛（plan.phase）在会话内即时生效，
        // 不必为了换一组工具而重建会话（重建会丢历史）。
        let schemas = self.visible_schemas();
        if let Some(streamer) = provider.as_streamer() {
            let on_delta = |delta: &str| {
                self.emit(ChatEvent::AssistantDelta {
                    round: round + 1,
                    delta: delta.to_string(),
                });
            };
            match streamer.stream_complete(msgs, &schemas, &on_delta).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    tracing::warn!(error = %e, "流式调用失败，降级非流式");
                }
            }
        }
        provider.complete(msgs, &schemas).await
    }

    fn emit(&self, ev: ChatEvent) {
        if let Some(cb) = &self.on_event {
            cb(ev);
        }
    }

    pub fn configured_model(&self) -> Option<String> {
        self.provider.as_ref().map(|p| p.model().to_string())
    }

    pub fn authorized_scope(&self) -> &[String] {
        &self.cfg.authorized_scope
    }

    /// 处理一条用户输入，返回本轮答复。
    pub async fn turn(&mut self, input: &str) -> ChatTurn {
        self.turn_no += 1;
        // R5-3：/recall <会话id> —— 注入旧会话滚动摘要 + 相关经验，不走 LLM 轮。
        if let Some(id) = input.trim().strip_prefix("/recall") {
            return self.recall_session(id.trim());
        }
        let turn = match &self.provider {
            Some(_) => {
                // agent/chat 模式由 provider.complete 内部按 is_agent_mode 决定是否携带 tools。
                let p = self.provider.clone().expect("checked above");
                let llm_turn = self.llm_turn(p, input).await;
                if llm_turn.ok {
                    llm_turn
                } else {
                    // 问题2：API 运行时不可用（连不上/key 错/限流/工具名被拒）→ 自动降级
                    // 为规则模式，而不是把错误直接抛给用户。下一轮仍尝试 LLM（可恢复）。
                    tracing::warn!("LLM 调用失败，本轮自动降级为规则模式");
                    let mut fallback = self.rule_turn(input).await;
                    fallback.trace.insert(
                        0,
                        "[提示] 大模型 API 不可用，已自动降级为规则模式（可用 help 查看工具命令）。".into(),
                    );
                    fallback
                }
            }
            None => self.rule_turn(input).await,
        };
        self.history.push_back(user_message(input.to_string()));
        self.history.push_back(assistant_message(turn.reply.clone()));
        // 记忆层：每轮把对话写进会话库（跨会话检索的唯一数据源）
        self.persist_turn(input, &turn);
        // R1-4：失败轮沉淀死路经验（真实任务失败才记：排除 API 不可用自动降级轮）
        if !turn.ok && !input.trim().is_empty() {
            let degraded = turn
                .trace
                .iter()
                .any(|l| l.contains("自动降级") || l.contains("API 不可用"));
            if !degraded {
                if let Some(db) = &self.experience {
                    let goal: String = input.trim().chars().take(60).collect();
                    let why: String =
                        turn.reply.chars().take(100).collect::<String>().replace('\n', " ");
                    let _ = db.add_dead_end(&format!("「{goal}」未完成：{why}"), vec!["失败".into()]);
                }
            }
        }
        while self.history.len() > self.cfg.history_cap * 2 {
            self.history.pop_front();
        }

        // ---- 上下文记忆维护 ----
        // ① 每 5 轮对窗口内对话做摘要并写入经验库（LLM 在线走模型，缺席规则降级）
        self.recent_turns.push_back(RecentTurn {
            input: input.to_string(),
            reply: turn.reply.clone(),
            trace: turn.trace.clone(),
        });
        let window = self.cfg.extract_every.max(1);
        while self.recent_turns.len() > window {
            self.recent_turns.pop_front();
        }
        self.maybe_summarize_experiences().await;
        // ② 上下文压缩：历史超窗口时滚动总结（仅 LLM 在线）
        self.maybe_compress_history().await;
        if let Some(db) = &self.experience {
            let _ = db.flush();
        }
        turn
    }

    /// 每 extract_every 轮（默认 5）把窗口内对话压缩为一条摘要经验并入库。
    ///
    /// - LLM 在线：模型摘要 + 标签，JSON 解析失败自动降级规则摘要；
    /// - LLM 缺席：取最近 N 轮用户目标/答复要点拼成规则摘要；
    /// - 窗口在触发后清空，保证每轮只摘要一次。
    async fn maybe_summarize_experiences(&mut self) {
        let Some(store) = self.experience.clone() else {
            return;
        };
        let every = self.cfg.extract_every;
        if every == 0 || self.turn_no % every as u64 != 0 || self.recent_turns.is_empty() {
            return;
        }
        let turns: Vec<RecentTurn> = self.recent_turns.drain(..).collect();
        let from = self.turn_no.saturating_sub(turns.len() as u64 - 1);
        let to = self.turn_no;
        let tags = vec!["对话摘要".to_string(), format!("轮{from}-{to}")];

        let mut transcript = String::new();
        for (i, t) in turns.iter().enumerate() {
            transcript.push_str(&format!(
                "第 {} 轮\n用户：{}\n智能体：{}\n",
                i + 1,
                t.input,
                truncate_for_summary(&t.reply, 160)
            ));
            if !t.trace.is_empty() {
                transcript.push_str(&format!("工具轨迹：{}\n", t.trace.join("；")));
            }
        }

        let mut text = String::new();
        let mut dead_ends: Vec<String> = Vec::new();
        if let Some((t, de)) = self.llm_summarize(&transcript).await {
            text = t;
            dead_ends = de;
        }
        if text.trim().is_empty() {
            text = rule_summarize(&turns, from, to);
        }
        if text.trim().is_empty() {
            return;
        }
        match store.add(&text, tags) {
            Ok(()) => {
                self.emit(ChatEvent::ExperienceSaved { count: 1 + dead_ends.len() });
                tracing::info!(turn = to, len = text.len(), "对话摘要已写入经验库");
            }
            Err(e) => tracing::warn!(error = %e, "对话摘要入库失败"),
        }
        // R1-4：死路显性沉淀（每条单独入库，关键词加权召回，无命中不注入）
        for de in &dead_ends {
            if let Err(e) = store.add_dead_end(de, vec!["死路".into(), format!("轮{to}")]) {
                tracing::warn!(error = %e, "死路经验入库失败");
            }
        }
    }

    /// LLM 摘要：要求固定字段 JSON（objective/findings/techniques/dead_ends/next_steps），
    /// 解析失败返回 None 让调用方降级。成功时返回（主摘要文本，死路列表）。
    async fn llm_summarize(&self, transcript: &str) -> Option<(String, Vec<String>)> {
        let provider = self.provider.clone()?;
        let prompt = format!(
            "把最近若干轮对话压缩为一条可复用经验（≤200 字），只输出 JSON 对象：\n\
             {{\"objective\":\"目标\",\"findings\":\"关键发现\",\
             \"techniques\":\"已验证有效的技术/命令\",\"dead_ends\":[\"失败方向，每条一句话，无则空数组\"],\
             \"next_steps\":\"待办\"}}\n\n{transcript}"
        );
        let msgs = vec![
            system_message("你是对话摘要器。只输出 JSON 对象，不输出其他文字。"),
            user_message(prompt),
        ];
        let c = provider.complete(&msgs, &[]).await.ok()?;
        let raw = c.content?;
        let v: serde_json::Value = parse_leading_json(&raw)?;
        let obj = v.as_object()?;
        let get = |k: &str| {
            obj.get(k)
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(o) = get("objective") {
            parts.push(format!("目标：{o}"));
        }
        if let Some(f) = get("findings") {
            parts.push(format!("发现：{f}"));
        }
        if let Some(t) = get("techniques") {
            parts.push(format!("有效技术：{t}"));
        }
        if let Some(n) = get("next_steps") {
            parts.push(format!("待办：{n}"));
        }
        let text = parts.join("；");
        if text.is_empty() {
            return None;
        }
        let dead_ends: Vec<String> = obj
            .get("dead_ends")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Some((text, dead_ends))
    }

    /// 上下文自动压缩（问题10）：token 估算达到窗口 80% 时触发。
    /// 保留系统提示（build_context 每轮重建）+ 最近 N 轮原始消息（含完整 tool_calls /
    /// tool 结果，防止工具调用链错乱），更早历史交给模型生成结构化摘要，
    /// 替换进 rolling_summary。上下文 = 系统提示 + 历史摘要 + 最近 N 轮原始 + 当前输入。
    async fn maybe_compress_history(&mut self) {
        if !self.cfg.summarize_history || self.provider.is_none() || self.history.is_empty() {
            return;
        }
        // 触发判定：优先 token 占比；未配置窗口（0）时用条数兜底。
        let est = self.estimate_context_tokens();
        let by_tokens = self.cfg.context_window_tokens > 0
            && (est as f64) >= (self.cfg.context_window_tokens as f64) * self.cfg.compact_threshold;
        let by_count = self.cfg.context_window_tokens == 0
            && self.history.len() > self.cfg.history_cap;
        if !by_tokens && !by_count {
            return;
        }

        // 最近 N 轮原始消息必须保留：一轮 = user + 其后的 assistant/tool 消息。
        let keep_turns = self.cfg.keep_recent_turns.clamp(3, 5);
        let keep_msgs = count_trailing_turn_msgs(&self.history, keep_turns);
        if self.history.len() <= keep_msgs {
            return; // 没有可压缩的更旧历史
        }
        let split_at = self.history.len() - keep_msgs;
        let oldest: Vec<ChatMessage> = self.history.iter().take(split_at).cloned().collect();
        let transcript = transcript_from_messages(&oldest);

        let provider = self.provider.clone().expect("checked above");
        let prev = self.rolling_summary.clone().unwrap_or_default();
        let prompt = format!(
            "你是上下文压缩器。把对话历史压缩为结构化摘要（不超过 400 字），\
必须保留以下字段并用【】标注：\n\
【目标与授权范围】用户要做什么、授权的目标边界；\n\
【关键发现】端口/服务/版本/漏洞线索/路径等已确认事实；\n\
【用户决定】用户明确的选择与偏好、已拒绝的方向；\n\
【待办】尚未完成的动作。\n\
只输出摘要正文，不输出其他文字。\n\n\
{}{}",
            if prev.is_empty() { String::new() } else { format!("已有摘要：\n{prev}\n\n") },
            transcript
        );
        let msgs = vec![
            system_message("你是上下文压缩器。只输出结构化摘要正文，不输出其他文字。"),
            user_message(prompt),
        ];
        // LLM 失败时降级为规则摘要（保证无网/API 异常也能压缩，不撑爆上下文）。
        let summary = match provider.complete(&msgs, &[]).await {
            Ok(c) => c.content.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            Err(_) => None,
        }
        .unwrap_or_else(|| rule_compress_summary(&oldest, &prev));
        if summary.trim().is_empty() {
            return;
        }
        self.rolling_summary = Some(summary);
        // R1-3 可逆压缩（dsh「压缩只遮蔽不删除」）：被淘汰的原始消息进归档，
        // 上限 4000 条防 sled 记录无限膨胀（超出丢最旧）。
        for m in self.history.drain(..split_at) {
            if self.archived.len() >= 4000 {
                let drop = self.archived.len() - 3999;
                self.archived.drain(..drop);
            }
            self.archived.push(m);
        }
        self.emit(ChatEvent::MemoryCompressed {
            compressed: split_at,
            kept: self.history.len(),
        });
    }

    /// 当前上下文占用（tokens，估算）。UI 的"上下文 12.4k/32k 39%"用它，
    /// **不要用某一轮的 prompt_tokens 当占用** —— 一轮里 5 次工具调用会把
    /// prompt_tokens 累加 5 次，占比能显示到 200%+。
    pub fn context_tokens(&self) -> usize {
        self.estimate_context_tokens()
    }

    /// 估算当前上下文 token（保守：中文按 1 字≈2 token，ASCII 按 4 字符≈1 token）。
    fn estimate_context_tokens(&self) -> usize {
        let mut n = 0usize;
        n += estimate_text_tokens(&self.system_prompt(""));
        if let Some(s) = &self.rolling_summary {
            n += estimate_text_tokens(s);
        }
        for m in &self.history {
            n += estimate_text_tokens(&m.content);
            if let Some(tcs) = &m.tool_calls {
                for tc in tcs {
                    n += estimate_text_tokens(&tc.arguments);
                }
            }
        }
        n
    }

    // ------------------------------------------------------------------
    // LLM ReAct 循环
    // ------------------------------------------------------------------

    async fn llm_turn(&mut self, provider: Arc<dyn LlmProvider>, input: &str) -> ChatTurn {
        self.turn_failed = 0;
        let mut msgs = self.build_context(input);

        let mut trace = Vec::new();
        let mut final_reply = String::new();
        let mut ok = true;
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;

        for round in 0..self.cfg.max_tool_rounds {
            self.emit(ChatEvent::RoundStarted { round: round + 1 });
            let completion = match self.call_llm(round, provider.as_ref(), &msgs).await {
                Ok(c) => c,
                Err(e) => {
                    final_reply = format!("模型调用失败（{e}）。请检查 [llm] 配置、服务可达性后重试。");
                    ok = false;
                    break;
                }
            };
            prompt_tokens += completion.usage.prompt_tokens as u64;
            completion_tokens += completion.usage.completion_tokens as u64;

            if completion.tool_calls.is_empty() {
                final_reply = completion
                    .content
                    .unwrap_or_else(|| "(模型返回空回复)".into());
                break;
            }

            // ReAct 显性化（问题4）：中间轮的思考文本不丢弃——
            // ① emit 到 UI（thinking 行）；② 首轮含计划即记录为主干计划。
            if let Some(text) = completion
                .content
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let text = text.to_string();
                if self.plan_text.is_none() && looks_like_plan(&text) {
                    self.plan_text = Some(text.clone());
                }
                self.emit(ChatEvent::AssistantNote { round: round + 1, text });
            }

            // assistant（带 tool_calls）
            msgs.push(ChatMessage {
                role: ChatRole::Assistant,
                content: completion.content.clone().unwrap_or_default(),
                tool_calls: Some(completion.tool_calls.clone()),
                tool_call_id: None,
            });
            // R4-1（dsh 并行池对齐）：≥2 个工具调用且全部并发安全 → prepare 串行
            // （钩子/审批交互逐个）+ 有界池（8）并行 invoke + 按序 finish 回填；
            // 任一非并发安全（写工具/执行类）整体回落串行。
            let tcs = &completion.tool_calls;
            let can_parallel = tcs.len() >= 2
                && tcs.iter().all(|tc| {
                    let probe: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                    self.executor.concurrency_safe(&tc.name, &probe)
                });
            if can_parallel {
                let mut slots: Vec<(bool, Option<Prepared>)> = Vec::with_capacity(tcs.len()); // (denied?, prepared)
                let mut denied_obs: Vec<Option<String>> = Vec::with_capacity(tcs.len());
                for tc in tcs {
                    match self.prepare_tool(&tc.name, &tc.arguments, &mut trace).await {
                        Prepared::Denied(obs) => {
                            slots.push((true, None));
                            denied_obs.push(Some(obs));
                        }
                        p => {
                            slots.push((false, Some(p)));
                            denied_obs.push(None);
                        }
                    }
                }
                let executor = self.executor.clone();
                // 全 owned boxed future（防 HRTB 生命周期问题）：每 job 克隆自己的入参
                let mut queue: std::collections::VecDeque<
                    std::pin::Pin<Box<dyn std::future::Future<Output = Option<ToolOutcome>> + Send>>,
                > = Default::default();
                for (_, p) in slots.iter() {
                    match p {
                        Some(Prepared::Ready { tool_name, args, ctx, .. }) => {
                            let ex = executor.clone();
                            let tn = tool_name.clone();
                            let a = args.clone();
                            let c = ctx.clone();
                            queue.push_back(Box::pin(async move {
                                Some(ex.execute(&c, std::slice::from_ref(&tn), a).await)
                            }));
                        }
                        _ => queue.push_back(Box::pin(std::future::ready(None))),
                    }
                }
                let outcomes: Vec<Option<ToolOutcome>> = futures_util::stream::iter(queue)
                    .buffered(PARALLEL_POOL)
                    .collect::<Vec<_>>()
                    .await;
                for (i, tc) in tcs.iter().enumerate() {
                    let obs = if let Some(obs) = &denied_obs[i] {
                        obs.clone()
                    } else if let Some(outcome) = &outcomes[i] {
                        let p = slots[i].1.as_ref().expect("非 denied 槽位必有 Prepared");
                        self.finish_tool(p, outcome.clone(), &mut trace)
                    } else {
                        json!({"ok": false, "error": "internal: 并行任务未产出结果"}).to_string()
                    };
                    msgs.push(ChatMessage {
                        role: ChatRole::Tool,
                        content: obs,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                }
            } else {
                // 逐个执行并回填观察
                for tc in &completion.tool_calls {
                    let obs = self.run_tool(&tc.name, &tc.arguments, &mut trace).await;
                    msgs.push(ChatMessage {
                        role: ChatRole::Tool,
                        content: obs,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                }
            }
            // Critic（问题4）：连续失败 ≥3 时注入调整/终止提示，防死循环跑偏。
            // 注：置于全部 tool 观察之后，保持 OpenAI 协议顺序（tool 紧随 assistant）。
            if self.consecutive_failures >= 3 {
                msgs.push(system_message(
                    "[反思校验] 工具已连续失败 3 次以上。请先分析失败原因，\
                     换用其他工具或系统命令调整方向；若确认无法达成目标，\
                     停止调用工具并如实向用户汇报已获得的部分结果与失败原因。",
                ));
                self.consecutive_failures = 0;
            }
        }

        if final_reply.is_empty() {
            final_reply = format!(
                "已执行 {} 次工具调用但未得出最终结论（达到 {} 轮上限）。可继续追问或调整目标。",
                trace.len(),
                self.cfg.max_tool_rounds
            );
        }
        // 反思层（反思-6 六层里的「反思」）：失败达阈值时做一次结构化复盘。
        // 只用规则、不再调 LLM —— 反思的时机恰好是"刚才那次调用不顺利"，
        // 再发一次请求既慢又可能同样失败；而失败模式本身是可枚举的。
        if self.cfg.reflect && self.turn_failed >= self.cfg.reflect_min_failures.max(1) {
            let note = self.reflect_note(&trace);
            if !note.is_empty() {
                self.emit(ChatEvent::AssistantNote { round: 0, text: note.clone() });
                if let Some(store) = &self.experience {
                    // 经验库写失败不影响本轮结果（best-effort），但要在日志里留痕
                    if let Err(e) = store.add(&note, vec!["reflection".into()]) {
                        tracing::warn!(error = %e, "反思写入经验库失败");
                    }
                }
            }
        }
        ChatTurn { reply: final_reply, trace, ok, prompt_tokens, completion_tokens }
    }

    /// 执行一次工具调用并返回观察 JSON（写入 tool 消息）。
    /// R4-1 拆分为 prepare→invoke→finish；本方法是串行快捷路径（规则降级/单工具用）。
    async fn run_tool(&mut self, tool: &str, arguments: &str, trace: &mut Vec<String>) -> String {
        match self.prepare_tool(tool, arguments, trace).await {
            Prepared::Denied(obs) => obs,
            p @ Prepared::Ready { .. } => {
                let outcome = self.invoke_tool(&p).await.expect("Ready 必有 outcome");
                self.finish_tool(&p, outcome, trace)
            }
        }
    }

    /// 准备阶段（&mut self，必须串行）：denylist → 钩子 → 风险 → 审批 → 重复守卫。
    async fn prepare_tool(&mut self, tool: &str, arguments: &str, trace: &mut Vec<String>) -> Prepared {
        let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
        // 拒绝清单硬校验（问题10）：目标命中 deny 即拒绝，不进审批、不执行。
        let deny_hit = self.cfg.denylist.iter().any(|d| {
            let d = d.trim();
            !d.is_empty()
                && ["target", "url", "host"].iter().any(|k| {
                    args.get(*k)
                        .and_then(Value::as_str)
                        .map(|t| t.contains(d) || d.contains(t))
                        .unwrap_or(false)
                })
        });
        if deny_hit {
            trace.push(format!("[FAIL] {tool}（目标命中拒绝清单）"));
            self.emit(ChatEvent::ToolFinished {
                tool: tool.to_string(),
                ok: false,
                summary: "目标命中拒绝清单（denylist），已拒绝执行".into(),
                duration_ms: 0,
                detail: String::new(),
            });
            return Prepared::Denied(json!({
                "ok": false,
                "error": "目标命中拒绝清单（授权范围中排除的目标），禁止对该目标调用任何工具"
            }).to_string());
        }
        // 授权范围校验（ScopeGuard）：带目标的调用不在白名单内时，不再硬拒绝 +
        // "让用户自己去配置里开"，而是发审批请求让用户当场授权（本次/本轮/空间白名单）。
        // 注意：denylist 命中在前面已硬拒绝，这里只处理"未显式授权"这类可当场批准的情况。
        if let Some(reason) = scope_denial(&self.cfg.authorized_scope, &self.cfg.denylist, &args) {
            self.emit(ChatEvent::ApprovalRequested {
                tool: tool.to_string(),
                risk: "OutOfScope".into(),
                args_summary: format!("{reason}｜参数：{}", summarize_args(&args)),
            });
            let allowed = match self.approval_mode {
                Some(v) => v,
                None if self.session_allowlist.contains(tool) => true,
                None => (self.approval)(tool, RiskLevel::High, &reason),
            };
            self.emit(ChatEvent::ApprovalDecision {
                tool: tool.to_string(),
                allowed,
                via: "scope-approval".into(),
            });
            if !allowed {
                trace.push(format!("[FAIL] {tool}（授权范围外，审批未通过：{reason}）"));
                self.emit(ChatEvent::ToolFinished {
                    tool: tool.to_string(),
                    ok: false,
                    summary: reason.clone(),
                    duration_ms: 0,
                    detail: String::new(),
                });
                return Prepared::Denied(
                    json!({ "ok": false, "error": reason, "via": "scope-approval-denied" }).to_string(),
                );
            }
            trace.push(format!("[OK] {tool}（授权范围外，用户当场批准）"));
        }
        // 插件前置钩子（R3-1，dsh guard 语义）：denylist 之后、审批之前。
        // Deny=硬拦截（不进审批）；Ask=视同需审批（即使工具风险低于阈值也走审批链）。
        let static_risk0 = self.risks.get(tool).copied().unwrap_or(RiskLevel::Low);
        let risk0 = self.executor.risk_for(tool, &args).unwrap_or(static_risk0);
        let verdict = toolkit_core::hooks::evaluate(&self.hooks, tool, risk0, &args);
        if let toolkit_core::hooks::HookVerdict::Deny(reason) = &verdict {
            trace.push(format!("[FAIL] {tool}（钩子拦截：{reason}）"));
            self.emit(ChatEvent::ToolFinished {
                tool: tool.to_string(),
                ok: false,
                summary: format!("插件钩子拦截：{reason}"),
                duration_ms: 0,
                detail: String::new(),
            });
            return Prepared::Denied(json!({
                "ok": false,
                "error": format!("工具调用被插件钩子 `{reason}` 拦截，禁止执行"),
                "hook": reason
            }).to_string());
        }
        // 未登记工具默认低风险，避免"名字对不上就当高风险"。
        // R1-1：执行类工具按参数动态分级（risk_for），无动态判定回落静态表。
        let static_risk = self.risks.get(tool).copied().unwrap_or(RiskLevel::Low);
        let mut risk = self
            .executor
            .risk_for(tool, &args)
            .unwrap_or(static_risk);
        // Ask 裁决：钩子要求人工确认——低风险工具也提升到审批阈值
        if verdict == toolkit_core::hooks::HookVerdict::Ask && !risk.needs_approval() {
            risk = RiskLevel::High;
        }
        let args_summary = summarize_args(&args);
        self.emit(ChatEvent::ToolStarted {
            tool: tool.to_string(),
            args_summary: args_summary.clone(),
        });

        if risk.needs_approval() {
            self.emit(ChatEvent::ApprovalRequested {
                tool: tool.to_string(),
                risk: format!("{risk:?}"),
                args_summary: args_summary.clone(),
            });
            // 审批模式短路（问题5）：Some(true)=全部放行 / Some(false)=全部拒绝；None=逐次询问。
            // R2-4：None 时先查会话工具白名单（用户「本会话放行该工具」授权），再走逐次审批。
            let allowed = match self.approval_mode {
                Some(v) => {
                    self.emit(ChatEvent::ApprovalDecision {
                        tool: tool.to_string(),
                        allowed: v,
                        via: if v { "mode-allow" } else { "mode-deny" }.into(),
                    });
                    v
                }
                None if self.session_allowlist.contains(tool) => {
                    self.emit(ChatEvent::ApprovalDecision {
                        tool: tool.to_string(),
                        allowed: true,
                        via: "session-allowlist".into(),
                    });
                    true
                }
                None => {
                    let v = (self.approval)(tool, risk, &args_summary);
                    // R2-4 审计事件对之 decided：once / deny / timeout（回调内部超时返回 false）
                    self.emit(ChatEvent::ApprovalDecision {
                        tool: tool.to_string(),
                        allowed: v,
                        via: "interactive".into(),
                    });
                    v
                }
            };
            if !allowed {
                trace.push(format!("[FAIL] {tool}（审批拒绝）"));
                self.emit(ChatEvent::ApprovalDenied {
                    tool: tool.to_string(),
                });
                return Prepared::Denied(
                    json!({"ok": false, "error": "审批拒绝：该工具为高风险，用户/策略未放行"}).to_string(),
                );
            }
        }

        let ctx = self.ctx_for(&args, risk);
        // R2-3 重复调用守卫（dsh 软指引）：规范化指纹比对，连续重复 3/5/8 次分级注入提醒，
        // 只建议不否决（不拦截执行）。
        let key = fingerprint_call(tool, &args);
        if self.repeat_key == Some(key) {
            self.repeat_count += 1;
        } else {
            self.repeat_key = Some(key);
            self.repeat_count = 1;
        }
        let guard_note = match self.repeat_count {
            3 => Some("重复提醒：这是本次会话第 3 次完全相同的调用，请确认结果是否变化、参数是否需要调整。"),
            5 => Some("重复警告：第 5 次相同调用，该动作大概率已无效。建议换工具、改参数或推进其他子任务。"),
            8 => Some("重复硬警告：第 8 次相同调用，重试意义极低。请停止重复，重新评估计划并向用户汇报卡点。"),
            _ => None,
        };
        Prepared::Ready { tool_name: tool.to_string(), args, ctx, guard_note }
    }

    /// R4-1 装载执行：真正调用执行器（仅 &self，可并行）。
    async fn invoke_tool(&self, p: &Prepared) -> Option<ToolOutcome> {
        match p {
            Prepared::Denied(_) => None,
            Prepared::Ready { tool_name, args, ctx, .. } => Some(
                self.executor
                    .execute(ctx, std::slice::from_ref(tool_name), args.clone())
                    .await,
            ),
        }
    }

    /// R4-1 结果落账（&mut self，串行保序）：trace/事件/spill/Critic 记账/目标背书/守卫注入。
    fn finish_tool(
        &mut self,
        p: &Prepared,
        outcome: ToolOutcome,
        trace: &mut Vec<String>,
    ) -> String {
        let Prepared::Ready { tool_name, guard_note, .. } = p else {
            return String::new();
        };
        let tool: &str = tool_name;
        let brief: String = outcome.summary.chars().take(160).collect();
        trace.push(format!(
            "{} {tool} → {brief}（{}ms）",
            if outcome.ok { "[OK]" } else { "[FAIL]" },
            outcome.duration_ms
        ));
        self.emit(ChatEvent::ToolFinished {
            tool: tool.to_string(),
            ok: outcome.ok,
            summary: outcome.summary.clone(),
            duration_ms: outcome.duration_ms,
            detail: outcome
                .detail
                .as_ref()
                .map(|d| {
                    let s = d.to_string();
                    s.chars().take(12_000).collect()
                })
                .unwrap_or_default(),
        });
        // 把结构化明细（如 kb.search 的 id/内容预览）回填给 LLM，
        // 否则模型只能看到"命中 N 条"，无法取得证据细节（问题9/11）。
        let mut obs = json!({
            "ok": outcome.ok,
            "tool": outcome.tool_name,
            "summary": outcome.summary,
            "duration_ms": outcome.duration_ms,
        });
        if let Some(detail) = &outcome.detail {
            // R1-2：超过 spill 阈值先全文落盘，回填 file.read 可读的 locator。
            let raw_chars = detail.to_string().chars().count();
            if raw_chars > SPILL_THRESHOLD {
                if let Some(store) = &self.spill {
                    let seq = self.turn_no * 1000 + trace.len() as u64;
                    if let Ok(loc) =
                        store.save(&self.cfg.session_id.0.to_string(), seq, tool, &detail.to_string())
                    {
                        obs["spill"] = json!(loc);
                        obs["spill_note"] =
                            json!("detail 全文已落盘，可用 file.read 读取上述路径获取完整内容");
                    } // 落盘失败：静默回落原截断流程（best-effort）
                }
            }
            let kept = trim_detail_for_llm(detail, 12_000);
            obs["detail"] = kept;
        }
        // Critic 记账：工具失败累计，成功清零（问题4）。
        if outcome.ok {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures += 1;
            self.turn_failed += 1;
        }
        // R2-2 目标完成背书（dsh 权威规则）：模型自报完成 → HITL 审批背书后才 completed。
        // 注：会话级「全部放行」开启时背书随之自动通过（用户已整体授权该会话）。
        if tool == "goal.state" {
            if let Some(g) = &self.goals {
                let cur = g.get();
                if cur.status == base_store::GOAL_AWAITING_ENDORSEMENT {
                    let ask = format!("背书目标完成：{}", cur.note);
                    self.emit(ChatEvent::ApprovalRequested {
                        tool: "goal.endorsement".into(),
                        risk: "High".into(),
                        args_summary: ask.clone(),
                    });
                    let allowed = match self.approval_mode {
                        Some(v) => v,
                        None => (self.approval)("goal.endorsement", RiskLevel::High, &ask),
                    };
                    if allowed {
                        let _ = g.endorse("user-approval");
                        obs["goal_status"] = json!("completed");
                        obs["endorsement"] = json!("用户已背书，目标置为 completed");
                        trace.push("[OK] goal.endorsement → 用户背书，目标 completed".into());
                    } else {
                        let _ = g.reject("审批拒绝");
                        obs["goal_status"] = json!("rejected");
                        obs["endorsement"] =
                            json!("用户否决完成声明，目标回退 in_progress（请继续推进或如实汇报差距）");
                        trace.push("[FAIL] goal.endorsement → 用户否决完成声明".into());
                    }
                }
            }
        }
        if let Some(note) = guard_note {
            obs["guard_note"] = json!(note);
        }
        obs.to_string()
    }

    fn ctx_for(&self, args: &Value, risk: RiskLevel) -> ExecutionContext {
        let target = ["target", "url", "host"]
            .iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
            .map(|s| s.to_string())
            .or_else(|| self.cfg.authorized_scope.first().cloned())
            .unwrap_or_default();
        ExecutionContext {
            session_id: self.cfg.session_id,
            agent_id: AgentId::new("chat"),
            task_id: TaskId::new(format!("chat-{}", self.turn_no)),
            target,
            risk,
            approved: true,
            // 完整授权范围随上下文下发：工具层（含插件）需要它做二次校验
            scope: self.cfg.authorized_scope.clone(),
        }
    }

    /// R5-4（dsh M4）：两级 AGENTS.md 读取——exe 同级全局 < 工作区根（后到者叠加）。
    /// 每轮重读（文件几 KB）：修改后下一轮自动生效（变更 reconciliation）。
    fn read_agents_md(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        let global = toolkit_core::tools::pysandbox::app_root().join("AGENTS.md");
        if let Ok(s) = std::fs::read_to_string(&global) {
            if !s.trim().is_empty() {
                parts.push(format!("【全局指令（AGENTS.md）】\n{}", s.trim()));
            }
        }
        if let Some(root) = &self.workspace_root {
            if let Ok(s) = std::fs::read_to_string(root.join("AGENTS.md")) {
                if !s.trim().is_empty() {
                    parts.push(format!("【工作区指令（AGENTS.md）】\n{}", s.trim()));
                }
            }
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// 组装本轮上下文：system(角色+范围+工具+技能) → 滚动摘要 → 经验召回 → 历史 → 输入。
    /// 生成一段结构化复盘（纯规则，不调 LLM）。
    ///
    /// 为什么不用 LLM 复盘：反思的触发时机正是"刚才不顺"，再发一次请求既慢又可能
    /// 同样失败；而失败模式是可枚举的（拒绝/审批/超时/参数错/工具不存在），
    /// 规则化反而更稳定、更省钱，也更容易被经验库检索命中。
    fn reflect_note(&self, trace: &[String]) -> String {
        let fails: Vec<&String> = trace.iter().filter(|t| t.starts_with("[FAIL]")).collect();
        if fails.is_empty() {
            return String::new();
        }
        let mut tools: Vec<String> = Vec::new();
        for f in &fails {
            let rest = f.trim_start_matches("[FAIL] ");
            let name: String = rest
                .split([' ', '（', '→'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !name.is_empty() && !tools.contains(&name) {
                tools.push(name);
            }
        }
        let mut s = format!(
            "本轮 {} 次工具失败（{}），共 {} 次调用。",
            fails.len(),
            if tools.is_empty() { "未记录工具名".to_string() } else { tools.join("、") },
            trace.len()
        );
        if let Some(first) = fails.first() {
            let brief: String = first.chars().take(160).collect();
            s.push_str(&format!("首个失败：{brief}。"));
        }
        if let Some(p) = &self.plan {
            if let Ok(g) = p.lock() {
                if let Some(b) = g
                    .items
                    .iter()
                    .find(|i| i.status == crate::plan_state::PlanStatus::Blocked)
                {
                    s.push_str(&format!("受阻步骤：{} {}。", b.id, b.title));
                }
            }
        }
        s.push_str(
            "复盘动作：先确认前置条件（路径、权限、授权范围、参数格式）是否成立，\
再换等效工具或把单步范围缩小；同一原因连续两次失败就停下如实汇报，不要重复同一调用。",
        );
        s
    }

    fn build_context(&self, input: &str) -> Vec<ChatMessage> {
        let mut msgs: Vec<ChatMessage> = vec![system_message(self.system_prompt(input))];
        // R5-4：AGENTS.md durable 注入（每轮重读，紧随主 system 指令）
        if let Some(md) = self.read_agents_md() {
            msgs.push(system_message(format!(
                "[工作区/全局指令——用户长期约定，优先遵守]\n{md}"
            )));
        }
        // 滚动历史摘要（上下文压缩产物）
        if let Some(summary) = &self.rolling_summary {
            msgs.push(system_message(format!("[此前对话摘要]\n{summary}")));
        }
        // 结构化任务状态（问题4，程序维护）：主干计划要点 + 轮次/失败计数，模型只读摘要。
        if let Some(plan) = &self.plan_text {
            let plan_brief: String = plan.chars().take(600).collect();
            msgs.push(system_message(format!(
                "[任务状态]\n主干计划（首轮生成，可动态调整）：\n{plan_brief}"
            )));
        }
        // 结构化计划（规划层）：每轮回填进度与「可执行的下一步」。
        // 有了它，模型不必重读整段历史来回忆做到哪一步；依赖关系由程序判定，
        // 而不是靠模型自己记得"这一步要等上一步"。
        if let Some(p) = &self.plan {
            if let Ok(g) = p.lock() {
                let view = g.render();
                if !view.is_empty() {
                    msgs.push(system_message(view));
                }
            }
        }
        // 经验库召回（按当前输入关键词加权；死路仅显式命中出现；无命中回退最近条目）。
        // R1-4：条数上限之外再做 token 预算裁剪，超预算条目丢弃。
        if let Some(store) = &self.experience {
            let hits = store.recall(input, self.cfg.experience_inject);
            let mut budget = self.cfg.experience_token_budget;
            let mut lines: Vec<String> = Vec::new();
            for e in hits {
                let tags = if e.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", e.tags.join(","))
                };
                let prefix = if e.kind == base_store::KIND_DEAD_END { "[死路·勿重复] " } else { "" };
                let line = format!("- {prefix}{}{tags}", e.text);
                let cost = estimate_text_tokens(&line);
                if cost > budget {
                    break;
                }
                budget -= cost;
                lines.push(line);
            }
            if !lines.is_empty() {
                msgs.push(system_message(format!(
                    "[经验库（既往会话沉淀，供参考）]\n{}",
                    lines.join("\n")
                )));
            }
        }
        // 结构化任务清单（R2-1，dsh TodoWrite 投影）：模型经 task.todo 维护，每轮回注。
        // 预算约 500 字符，超出截断尾部条目。
        if let Some(ts) = &self.tasks {
            let todos = ts.agent_todos();
            if !todos.is_empty() {
                let mut budget = 500usize;
                let mut lines: Vec<String> = Vec::new();
                for t in todos {
                    let mark = match t.status {
                        base_store::tasks::TASK_COMPLETED => "x",
                        base_store::tasks::TASK_IN_PROGRESS => "~",
                        _ => " ",
                    };
                    let line = format!("- [{mark}] {}", t.subject);
                    let cost = line.chars().count();
                    if cost > budget {
                        lines.push("- …（后续条目已截断）".to_string());
                        break;
                    }
                    budget -= cost;
                    lines.push(line);
                }
                msgs.push(system_message(format!(
                    "[任务清单（task.todo 维护，状态变化后立即整表更新）]\n{}",
                    lines.join("\n")
                )));
            }
        }
        // 目标状态机（R2-2，dsh goal 语义）：每轮回注，完成需用户背书
        if let Some(gs) = &self.goals {
            let g = gs.get();
            if !g.objective.is_empty() || g.status != base_store::GOAL_IN_PROGRESS {
                msgs.push(system_message(format!(
                    "[目标状态（goal.state 维护；completed 必须用户背书，自报完成无效）]\n{}",
                    g.brief()
                )));
            }
        }
        msgs.extend(self.history.iter().cloned());
        msgs.push(user_message(input.to_string()));
        msgs
    }

    /// 技能目录（渐进披露 + 关键词预筛）。
    ///
    /// 三条规则：
    /// 1. `available = false`（依赖的工具组没装配）→ 不进目录：教模型用不存在的工具只会白跑；
    /// 2. 命中当前输入关键词的技能排前面并标 ★（预筛是排序 + 截断，不是硬隐藏：
    ///    一次没命中不代表后面几轮也不会用到）；
    /// 3. 命中项优先保留，总数超过上限时截断并注明还有多少。
    fn skill_directory(&self, query: &str) -> String {
        let usable: Vec<&SkillNote> = self.skills.iter().filter(|s| s.available).collect();
        if usable.is_empty() {
            return String::new();
        }
        let (mut hit, rest): (Vec<&SkillNote>, Vec<&SkillNote>) =
            usable.iter().partition(|s| s.matches(query));
        let total = hit.len() + rest.len();
        hit.extend(rest);
        let shown = &hit[..hit.len().min(SKILL_DIR_CAP)];

        let mut block = String::from("\n\n## 可用技能目录\n");
        for s in shown {
            let star = if s.matches(query) { "★ " } else { "- " };
            block.push_str(&format!("{star}{}（触发：{}）\n", s.name, s.trigger));
        }
        if total > shown.len() {
            block.push_str(&format!(
                "（另有 {} 个技能与当前任务无关，未列出；确需时可换更具体的说法再问）\n",
                total - shown.len()
            ));
        }
        block.push_str(
            "\n任务命中某技能的触发场景时，先调用 skill.load 工具按名称加载其正文，\
再按正文方法论规划与执行。\n",
        );
        block
    }

    fn system_prompt(&self, query: &str) -> String {
        let scope = if self.cfg.authorized_scope.is_empty() {
            "(未声明，全部工具调用将被拒绝)".to_string()
        } else {
            self.cfg.authorized_scope.join(", ")
        };
        let deny = if self.cfg.denylist.is_empty() {
            String::new()
        } else {
            format!(
                "\n拒绝清单（授权范围内仍禁止执行的目标，「除这些之外」）：{}",
                self.cfg.denylist.join(", ")
            )
        };
        // R5-5 计划评审门：plan.exit 已注册时提示模型先出计划待批
        let plan_gate = if self.risks.contains_key("plan.exit") {
            "\n\n## 计划评审门\n对多步骤/有副作用/不可逆的复杂任务：先把计划全文（目标/步骤/工具与风险/回退方式）\
通过 `plan.exit` 工具的 plan 参数提交——用户批准（approved）后按计划执行；被拒绝则修订后重新提交，\
不得跳过评审直接执行复杂动作。简单只读查询无需提交计划。"
        } else {
            ""
        };
        // 可用工具清单只列**本轮真正下发的**工具：列了却调不通比不列更糟。
        let tools = self
            .risks
            .keys()
            .filter(|n| self.tool_filter.as_ref().map(|f| f(n)).unwrap_or(true))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        // R3-5 渐进披露（dsh 对齐）：system prompt 只注入技能目录（名称+触发场景），
        // 正文由模型经 skill.load 按需加载——技能数量增长不膨胀上下文。
        // S-7 预筛：依赖未装的技能不进目录；命中当前输入关键词的排在前面（★）。
        let skills_block = self.skill_directory(query);
        let persona = self
            .cfg
            .persona
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or(DEFAULT_PERSONA);
        let sections = if self.cfg.prompt_sections.is_empty() {
            String::new()
        } else {
            format!("\n\n{}", self.cfg.prompt_sections.join("\n\n"))
        };
        format!(
            "{persona}\n\n\
授权范围（只允许对这些目标使用工具）：{scope}。{deny}\n\
可用工具：{tools}。{plan_gate}\n\n\
## 工作范式（ReAct：思考 → 行动 → 观察 → 反思）\n\
1. 每轮动作前先用一两句话思考：当前目标是什么、已获得什么信息、还缺什么、下一步为什么选这个动作。\n\
2. 复杂任务（多目标/多阶段）在第一轮先输出主干计划：任务目标、编号分解步骤、停止条件；\
若 task.todo 可用则整表写入任务清单（含状态），此后每次状态变化都整表更新；\
之后逐步执行，计划可随观察动态调整。\n\
3. 动作选择遵循三级决策树：\n\
   ① 优先使用已注册工具；\n\
   ② 无合适工具时，用 os.exec 或 shell.run 执行系统自带命令；\n\
   ③ 仍不足时，用 file.write 写脚本，必要时先安装依赖，再用 sandbox.python 执行。\n\
4. 工具失败先判断是否重试（同参数最多重试 2 次）；重试仍失败则记录原因，换工具或推进其他子任务；\
系统提示连续失败 3 次时必须调整方向或终止。\n\
5. 每轮评估是否已满足停止条件：信息足够即停止调用工具，输出最终结论。\n\n\
## 行为约束\n\
- 每次只调用必要的工具；高风险工具会触发人工审批。\n\
- 工具观察是一切结论的唯一依据，禁止编造数据、禁止臆测未执行的检查结果。\n\
- 用简体中文，答复简洁、结论先行、附关键证据。\n\
- 不得把执行责任推回用户：必须自行利用现有工具推进。唯一例外是超出授权范围\
（authorized_scope 之外或命中拒绝清单）的请求，此时先澄清、不得擅自行动。\n\
- 命中「可用技能目录」的触发场景时，先用 skill.load 加载该技能正文，再按其方法论规划与执行。{skills_block}{sections}"
        )
    }

    // ------------------------------------------------------------------
    // 无 LLM 规则降级
    // ------------------------------------------------------------------

    async fn rule_turn(&mut self, input: &str) -> ChatTurn {
        let mut trace = Vec::new();

        // 降级模式全量工具命令帮助集（问题3）：help / 工具列表 / ? 列出全部工具；
        // "help <名>" 查看单个工具参数；"/call <名> <json>" 直接调用任意工具。
        let trimmed = input.trim();
        if has_any(trimmed, &["help", "帮助", "工具列表", "能做什么", "怎么用"]) {
            let specific = trimmed
                .split_whitespace()
                .nth(1)
                .filter(|_| !has_any(trimmed, &["工具列表", "能做什么", "怎么用"]));
            let reply = match specific {
                Some(name) => self.tool_help(Some(name)),
                None => self.full_tool_help(),
            };
            trace.push("(规则模式：工具帮助)".into());
            return ChatTurn { reply, trace, ok: true, ..Default::default() };
        }
        if trimmed == "?" {
            let reply = self.full_tool_help();
            trace.push("(规则模式：工具帮助)".into());
            return ChatTurn { reply, trace, ok: true, ..Default::default() };
        }
        if let Some(rest) = trimmed.strip_prefix("/call") {
            return self.rule_call_command(rest.trim(), &mut trace).await;
        }

        // 基座不做领域意图路由：业务指令解析属于专业智能树的「业务逻辑群组」。
        // 无 LLM 时仅保留通用命令（help / ? / /call），其余如实告知。
        trace.push("(规则模式：无 LLM，未路由业务意图)".into());
        ChatTurn {
            reply: "（降级模式：大模型不可用，仅支持通用工具命令）\n输入 help 查看全部工具，或 /call <工具名> <JSON> 直接调用。".to_string(),
            trace,
            ok: true,
            ..Default::default()
        }
    }

    /// 降级模式全量工具命令帮助集（问题3）。列出所有已注册工具名、风险与参数。
    fn full_tool_help(&self) -> String {
        let mut lines = vec![
            "【降级模式 · 全量工具命令集】".to_string(),
            "大模型未配置：仅可直接调用已注册的通用工具。".to_string(),
            "手动调用：/call <工具名> <JSON参数>（受风险审批约束）。".to_string(),
            "查看参数：help <工具名>。".to_string(),
            "".to_string(),
            format!("已注册工具 {} 个：", self.schemas.len()),
        ];
        for s in &self.schemas {
            let risk = self.risks.get(&s.name).copied().unwrap_or(RiskLevel::Low);
            lines.push(format!("  · {:<26} [{risk:?}] {}", s.name, tool_brief(&s.description)));
        }
        lines.join("\n")
    }

    /// 单个工具的参数帮助。
    fn tool_help(&self, name: Option<&str>) -> String {
        let Some(name) = name else { return self.full_tool_help(); };
        let key = name.trim();
        // 精确匹配或清洗名/子串匹配
        let found = self.schemas.iter().find(|s| {
            s.name == key
                || sanitize_name(&s.name) == key
                || s.name.contains(key)
        });
        match found {
            Some(s) => {
                let risk = self.risks.get(&s.name).copied().unwrap_or(RiskLevel::Low);
                let params = schema_params(&s.parameters);
                format!(
                    "【{}】[风险 {risk:?}]\n说明：{}\n参数：{}\n调用：/call {} {{{}}}",
                    s.name,
                    s.description,
                    params,
                    s.name,
                    params_example(&s.parameters)
                )
            }
            None => format!("未找到工具 `{key}`。输入 help 查看全部。"),
        }
    }

    /// 降级模式手动调用：/call <工具名> <JSON>。复用 run_tool 的审批/执行链。
    async fn rule_call_command(&mut self, rest: &str, trace: &mut Vec<String>) -> ChatTurn {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim();
        let args_raw = parts.next().unwrap_or("{}").trim();
        if name.is_empty() {
            return ChatTurn {
                reply: "用法：/call <工具名> <JSON参数>，例如 /call net.http {\"url\":\"http://x\"}".into(),
                trace: trace.clone(),
                ok: true,
                ..Default::default()
            };
        }
        // 支持清洗名（net_http）映射回原名（net.http）
        let resolved = self
            .schemas
            .iter()
            .find(|s| s.name == name || sanitize_name(&s.name) == name || s.name.contains(name))
            .map(|s| s.name.clone())
            .unwrap_or_else(|| name.to_string());
        if !self.schemas.iter().any(|s| s.name == resolved) {
            return ChatTurn {
                reply: format!("未找到工具 `{name}`。输入 help 查看全部工具。"),
                trace: trace.clone(),
                ok: true,
                ..Default::default()
            };
        }
        let obs = self.run_tool(&resolved, args_raw, trace).await;
        trace.push(format!("(规则模式：/call {resolved})"));
        ChatTurn {
            reply: format!("（降级 /call）已调用 {resolved}：\n{}", truncate_for_summary(&obs, 400)),
            trace: trace.clone(),
            ok: !obs.contains("\"ok\":false") && !obs.contains("审批拒绝"),
            ..Default::default()
        }
    }
}

// ------------------------------------------------------------------
// 规则降级辅助（纯函数，便于单测）
// ------------------------------------------------------------------

/// 粗提取输入中的候选目标：IPv4 或「像域名」的 token。
fn has_any(s: &str, keys: &[&str]) -> bool {
    keys.iter().any(|k| s.to_lowercase().contains(k))
}

fn summarize_args(args: &Value) -> String {
    let raw = args.to_string();
    raw.chars().take(200).collect()
}

/// 宽松解析模型输出中的首个 JSON 对象：兼容前后夹带文字（定位首个 `{` 到末个 `}`）。
fn parse_leading_json(text: &str) -> Option<Value> {
    let raw = text.trim();
    let start = raw.find('{')?;
    let end = raw.rfind('}')? + 1;
    serde_json::from_str(&raw[start..end]).ok()
}

/// 调用指纹（R2-3）：工具名 + 参数规范化（对象键排序递归）后哈希，
/// 使 `{"a":1,"b":2}` 与 `{"b":2,"a":1}` 视为同一调用。
fn fingerprint_call(tool: &str, args: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut h);
    canonical_value(args).hash(&mut h);
    h.finish()
}

/// 参数规范化：对象键排序重组，数组保序，标量原样。
fn canonical_value(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> =
                keys.iter().map(|k| format!("{k}:{}", canonical_value(&map[*k]))).collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(canonical_value).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

/// 工具名清洗（与 base-llm::sanitize_tool_name 同规则），用于 help 匹配。
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// 工具描述首句（帮助集精简展示）。
fn tool_brief(desc: &str) -> String {
    let first = desc.split(['。', '.', '\n']).next().unwrap_or(desc);
    truncate_for_summary(first.trim(), 60)
}

/// 从 JSON Schema 提取参数名与类型（帮助展示）。
fn schema_params(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return "无".into();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let parts: Vec<String> = props
        .iter()
        .map(|(k, v)| {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("any");
            let mark = if required.contains(&k.as_str()) { "*" } else { "" };
            format!("{k}{mark}:{ty}")
        })
        .collect();
    if parts.is_empty() { "无".into() } else { parts.join(", ") }
}

/// 从 JSON Schema 生成示例参数 JSON（占位值）。
fn params_example(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return "}".into();
    };
    let parts: Vec<String> = props
        .iter()
        .take(6)
        .map(|(k, v)| {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("string");
            let val = match ty {
                "number" | "integer" => "0".to_string(),
                "boolean" => "true".to_string(),
                "array" => "[]".to_string(),
                "object" => "{}".to_string(),
                _ => format!("\"<{k}>\""),
            };
            format!("\"{k}\":{val}")
        })
        .collect();
    parts.join(", ")
}

/// 估算文本 token：中文/全角按 1 字≈2 token，ASCII 按 4 字符≈1 token（偏保守）。
pub fn estimate_text_tokens(text: &str) -> usize {
    let cjk = text.chars().filter(|c| !c.is_ascii()).count();
    let ascii = text.chars().count() - cjk;
    cjk * 2 + ascii.div_ceil(4)
}

/// 参数里的目标字段（与 ScopeGuard 对齐的键集合）。
///
/// **故意不含 `name`**：`name` 在 ws.create / skill 类工具里是"名字"不是"目标"，
/// 拿它做范围校验会把"创建空间"误判成越权。
const TARGET_KEYS: &[&str] = &["target", "host", "url", "domain", "endpoint", "ip", "targets"];

/// 把 URL / host:port 归一成"主机名或 IP"，供 ScopeRule 匹配。
pub fn normalize_target(raw: &str) -> String {
    let s = raw.trim();
    let s = s.split("://").nth(1).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s);
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let s = s.rsplit('@').next().unwrap_or(s);
    if s.starts_with('[') {
        return s.split(']').next().unwrap_or(s).trim_start_matches('[').to_string();
    }
    if s.matches(':').count() == 1 {
        return s.split(':').next().unwrap_or(s).to_string();
    }
    s.to_string()
}

/// 提取一次调用涉及的目标（去重保序）。
pub fn targets_in_args(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in TARGET_KEYS {
        let Some(v) = args.get(*key) else { continue };
        let mut push = |s: &str| {
            let t = normalize_target(s);
            if !t.is_empty() && !out.contains(&t) {
                out.push(t);
            }
        };
        match v {
            Value::String(s) => push(s),
            Value::Array(a) => {
                for x in a {
                    if let Some(s) = x.as_str() {
                        push(s);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 授权范围硬校验：带目标的调用必须落在白名单内（黑名单优先）。
///
/// 返回 `Some(理由)` 表示拒绝。**未声明授权范围时，带目标的调用一律拒绝**（fail-closed）：
/// 不声明红线就没有"合法目标"这一说。
///
/// 会话主链路与 CLI 直调（`/call`）共用这一份判定，避免"直调绕过范围校验"。
pub fn scope_denial(scope: &[String], denylist: &[String], args: &Value) -> Option<String> {
    let targets = targets_in_args(args);
    if targets.is_empty() {
        return None;
    }
    if scope.iter().all(|s| s.trim().is_empty()) {
        return Some(format!(
            "未声明授权范围，拒绝对目标 {} 执行（先在配置/空间里声明授权目标）",
            targets.join("、")
        ));
    }
    // 宽松解析：写错的授权条目只丢自己（丢弃只会缩小授权面），写错的拒绝条目按字面包含（只会更严）。
    let (guard, notes) = kern_core::ScopeGuard::from_lists_lenient(scope, denylist);
    if !notes.is_empty() {
        tracing::warn!(notes = ?notes, "授权范围/拒绝清单存在无法识别的条目（已按安全方向处理）");
    }
    if guard.allowed_len() == 0 {
        return Some(format!(
            "授权范围里没有任何可用条目（{}），拒绝对目标 {} 执行",
            notes.join("；"),
            targets.join("、")
        ));
    }
    for t in &targets {
        if let Err(v) = guard.check_target(t) {
            return Some(format!("目标 {t} 未通过授权校验：{v}"));
        }
    }
    None
}

/// 统计末尾 N 个完整轮次占用的消息数（一轮以 user 起，含其后 assistant/tool）。
fn count_trailing_turn_msgs(history: &VecDeque<ChatMessage>, turns: usize) -> usize {
    let mut count = 0usize;
    let mut found = 0usize;
    for m in history.iter().rev() {
        count += 1;
        if m.role == ChatRole::User {
            found += 1;
            // 数到第 turns 个 user（它开启了倒数第 turns 轮）即停，连同其后消息一起保留。
            if found >= turns {
                break;
            }
        }
    }
    count
}

/// 消息序列 → 摘要用 transcript（含工具调用/结果，供结构化摘要）。
fn transcript_from_messages(msgs: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in msgs {
        let role = match m.role {
            ChatRole::User => "用户",
            ChatRole::Assistant => "智能体",
            ChatRole::Tool => "工具",
            _ => "系统",
        };
        out.push_str(&format!("{role}: {}\n", m.content));
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                out.push_str(&format!("  调用 {}({})\n", tc.name, truncate_for_summary(&tc.arguments, 200)));
            }
        }
    }
    out
}

/// 规则降级压缩摘要：无 LLM 时按结构化字段拼出要点。
fn rule_compress_summary(msgs: &[ChatMessage], prev: &str) -> String {
    let mut facts = Vec::new();
    let mut goals = Vec::new();
    for m in msgs {
        match m.role {
            ChatRole::User => goals.push(truncate_for_summary(&m.content, 60)),
            ChatRole::Tool | ChatRole::Assistant => {
                let t = truncate_for_summary(&m.content, 80);
                if !t.is_empty() && !t.starts_with('(') {
                    facts.push(t);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    if !prev.is_empty() {
        out.push_str(prev);
        out.push('\n');
    }
    out.push_str(&format!("【目标与授权范围】{}\n", goals.join("；")));
    out.push_str(&format!("【关键发现】{}\n", facts.join("；")));
    out.push_str("【用户决定】见最近对话；【待办】延续最近任务。");
    truncate_for_summary(&out, 400)
}

/// 裁剪工具 detail（问题11）：超上限时保留头尾（头 2/3、尾 1/3），
/// 中间以截断标记衔接——文件列表类输出头部（表头/前几行）与尾部（末尾行）最有价值。
/// 上限内原样透传；results 数组超 16 条截到 16（每条 content_preview 截 800 字符）。
fn trim_detail_for_llm(detail: &Value, max_chars: usize) -> Value {
    let mut v = detail.clone();
    if let Some(results) = v.get_mut("results").and_then(Value::as_array_mut) {
        for r in results.iter_mut() {
            if let Some(preview) = r.get("content_preview").and_then(Value::as_str) {
                let p: String = preview.chars().take(800).collect();
                if let Some(slot) = r.get_mut("content_preview") {
                    *slot = json!(p);
                }
            }
        }
        if results.len() > 16 {
            results.truncate(16);
        }
    }
    let text = v.to_string();
    if text.chars().count() <= max_chars {
        return v;
    }
    // 超大 detail：对大文本字段（stdout/stderr/output/output_text）头尾保留。
    let head_keep = max_chars * 2 / 3;
    let tail_keep = max_chars / 3;
    let big_fields = ["stdout", "stderr", "output", "output_text", "body"];
        if let Some(obj) = v.as_object_mut() {
            for key in big_fields.iter() {
                if let Some(Value::String(s)) = obj.get_mut(*key) {
                let chars: Vec<char> = s.chars().collect();
                if chars.len() > head_keep + tail_keep {
                    let dropped = chars.len() - head_keep - tail_keep;
                    let head: String = chars[..head_keep].iter().collect();
                    let tail: String = chars[chars.len() - tail_keep..].iter().collect();
                    *s = format!("{head}\n[...中间截断 {dropped} 字符...]\n{tail}");
                }
            }
        }
    }
    // 整体仍超限时再整体头尾截断一次（如字段名不在白名单的巨型 JSON）。
    let text = v.to_string();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > max_chars * 2 {
        let head: String = chars[..max_chars].iter().collect();
        let tail_start = chars.len().saturating_sub(max_chars / 3);
        let tail: String = chars[tail_start..].iter().collect();
        return json!({
            "truncated": true,
            "note": format!("detail 过大（{} 字符），已头尾截断", chars.len()),
            "head": head,
            "tail": tail,
        });
    }
    v
}

/// detail 序列化超过该字符数且配置了 spill 存储时，全文落盘并回填 locator（R1-2）。
const SPILL_THRESHOLD: usize = 24_000;

/// R4-1：并行工具池容量上限（dsh 对齐：有界 4-8，取 8）。
const PARALLEL_POOL: usize = 8;

/// 首轮模型文本是否像主干计划（问题4）：含计划关键词且带编号/步骤结构。
fn looks_like_plan(text: &str) -> bool {
    let has_keyword = ["任务目标", "分解步骤", "执行计划", "停止条件", "计划"]
        .iter()
        .any(|k| text.contains(k));
    let has_steps =
        text.contains("1.") || text.contains("①") || text.contains("步骤") || text.contains("1、");
    has_keyword && has_steps
}

/// 摘要文本截断（按字符而非字节，避免中文乱码）。
fn truncate_for_summary(text: &str, max: usize) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() <= max {
        return clean;
    }
    let head: String = clean.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 规则降级摘要：无 LLM 时把窗口内每轮「用户目标 → 答复要点」拼成一条经验。
fn rule_summarize(turns: &[RecentTurn], from: u64, to: u64) -> String {
    let parts: Vec<String> = turns
        .iter()
        .map(|t| {
            let goal = truncate_for_summary(&t.input, 40);
            let result = truncate_for_summary(&t.reply, 60);
            format!("{goal} → {result}")
        })
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    truncate_for_summary(&format!("第{from}-{to}轮对话：{}", parts.join("；")), 200)
}

// ------------------------------------------------------------------
// 测试：脚本化 Provider + Mock 执行器（无网络）
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use base_llm::{ChatMessage, Completion as C, LlmError, ToolCall, ToolSchema};
    use kern_core::{ToolExecutor, ToolOutcome};
    use std::sync::Mutex;

    struct MockExecutor;
    #[async_trait]
    impl ToolExecutor for MockExecutor {
        async fn execute(
            &self,
            _ctx: &ExecutionContext,
            tool_hint: &[String],
            _args: Value,
        ) -> ToolOutcome {
            ToolOutcome {
                ok: true,
                tool_name: tool_hint.first().cloned().unwrap_or_default(),
                summary: "mock-result".into(),
                facts: vec![],
                detail: None,
                duration_ms: 1,
            }
        }

        fn concurrency_safe(&self, tool: &str, _args: &Value) -> bool {
            tool == "util.echo"
        }
        fn available_tools(&self) -> Vec<String> {
            vec!["util.echo".into(), "scan.port".into()]
        }
    }

    struct ScriptProvider {
        steps: Mutex<Vec<C>>,
    }
    #[async_trait]
    impl LlmProvider for ScriptProvider {
        fn provider_name(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-1"
        }
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
        ) -> Result<C, LlmError> {
            self.steps
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| LlmError::Config("脚本耗尽".into()))
        }
    }

    fn schemas() -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "util.echo".into(),
            description: "回显".into(),
            parameters: json!({"type": "object"}),
        }]
    }

    fn session(
        provider: Option<Arc<dyn LlmProvider>>,
        approval: ApprovalCallback,
    ) -> ChatSession {
        let mut risks = HashMap::new();
        risks.insert("util.echo".to_string(), RiskLevel::None);
        risks.insert("scan.port".to_string(), RiskLevel::Medium);
        ChatSession::new(
            provider,
            Arc::new(MockExecutor),
            schemas(),
            risks,
            ChatConfig {
                authorized_scope: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            approval,
        )
    }

    fn allow_all(_: &str, _: RiskLevel, _: &str) -> bool {
        true
    }

    fn deny_all(_: &str, _: RiskLevel, _: &str) -> bool {
        false
    }

    #[tokio::test]
    async fn llm_react_loop_completes() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptProvider {
            steps: Mutex::new(vec![
                // 第二次调用：给出最终答复
                C {
                    content: Some("回环正常，echo 成功。".into()),
                    tool_calls: vec![],
                    usage: Default::default(),
                },
                // 第一次调用：请求工具
                C {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "util.echo".into(),
                        arguments: "{\"text\":\"hi\"}".into(),
                    }],
                    usage: Default::default(),
                },
            ]),
        });
        let mut s = session(Some(provider), Arc::new(allow_all));
        let t = s.turn("帮我自检一下").await;
        assert_eq!(t.reply, "回环正常，echo 成功。");
        assert_eq!(t.trace.len(), 1);
        assert!(t.trace[0].contains("[OK] util.echo"));
    }

    /// R4-1：一轮两个并发安全工具 → 并行池执行，结果按序回填（tool_call_id 对齐）。
    /// 协议消息不落 history（turn 只持久化 user/assistant 文本），故断言第二轮
    /// LLM 请求（CaptureProvider.captured[1]）中的 tool 消息顺序。
    #[tokio::test]
    async fn parallel_tools_execute_and_fill_in_order() {
        let concrete = Arc::new(CaptureProvider {
            step: Mutex::new(vec![
                // 第二次调用：汇总答复
                C {
                    content: Some("两项并行检查完成。".into()),
                    tool_calls: vec![],
                    usage: Default::default(),
                },
                // 第一次调用：同轮请求两个并发安全工具
                C {
                    content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "c-a".into(),
                            name: "util.echo".into(),
                            arguments: "{\"text\":\"first\"}".into(),
                        },
                        ToolCall {
                            id: "c-b".into(),
                            name: "util.echo".into(),
                            arguments: "{\"text\":\"second\"}".into(),
                        },
                    ],
                    usage: Default::default(),
                },
            ]),
            captured: Mutex::new(vec![]),
        });
        let provider: Arc<dyn LlmProvider> = concrete.clone();
        let mut s = session(Some(provider), Arc::new(allow_all));
        let t = s.turn("并行自检").await;
        assert_eq!(t.reply, "两项并行检查完成。");
        // 两条轨迹都执行了
        assert_eq!(t.trace.len(), 2);
        assert!(t.trace.iter().all(|x| x.contains("[OK] util.echo")));
        // 协议消息按序回填：第二轮请求中 tool(c-a) 在 tool(c-b) 之前
        let captured = concrete.captured.lock().unwrap();
        assert!(captured.len() >= 2, "应至少捕获两次 LLM 调用");
        let ids: Vec<&str> = captured[captured.len() - 1]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(ids, vec!["c-a", "c-b"], "结果必须按请求顺序回填");
    }

    #[tokio::test]
    async fn approval_denial_blocks_tool() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptProvider {
            steps: Mutex::new(vec![
                // 第二次调用：给出最终答复
                C {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    usage: Default::default(),
                },
                // 第一次调用：请求高风险工具（将被拒绝）
                C {
                    content: Some("正在请求审批…".into()),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "scan.port".into(),
                        arguments: "{\"target\":\"127.0.0.1\"}".into(),
                    }],
                    usage: Default::default(),
                },
            ]),
        });
        let deny: ApprovalCallback =
            Arc::new(|_, _, _| false);
        let mut s = session(Some(provider), deny);
        // 审批阈值现为 needs_approval()（High/Critical），把工具风险提到 High 验证拒绝链
        s.update_tools(schemas(), HashMap::from([("scan.port".to_string(), RiskLevel::High)]));
        let t = s.turn("扫描").await;
        assert!(t.reply.contains("done")); // 循环继续到最终答复
        assert!(t.trace[0].contains("审批拒绝"));
    }

    #[tokio::test]
    async fn fallback_without_llm_has_no_domain_routing() {
        // 基座不做领域意图路由（那属于专业智能树的业务逻辑群组）：
        // 无 LLM 时只保留通用命令，其余如实告知。
        let mut s = session(None, Arc::new(allow_all));
        let t = s.turn("随便问一个业务问题").await;
        assert!(t.reply.contains("降级模式"), "reply={}", t.reply);
        assert!(t.trace.iter().any(|l| l.contains("未路由业务意图")), "trace={:?}", t.trace);
    }

    #[tokio::test]
    async fn fallback_help_and_unknown() {
        let mut s = session(None, Arc::new(allow_all));
        let t = s.turn("你能做什么").await;
        assert!(t.reply.contains("工具"));
        let t2 = s.turn("今天天气怎么样").await;
        assert!(t2.reply.contains("降级模式"));
    }

    // ------------------------------------------------------------------
    // 上下文记忆与技能注入（v2.4.0）
    // ------------------------------------------------------------------

    /// 捕获每次请求的完整消息列表（验证注入内容）。
    struct CaptureProvider {
        step: Mutex<Vec<C>>,
        captured: Mutex<Vec<Vec<ChatMessage>>>,
    }
    #[async_trait]
    impl LlmProvider for CaptureProvider {
        fn provider_name(&self) -> &str {
            "capture"
        }
        fn model(&self) -> &str {
            "capture-1"
        }
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _tools: &[ToolSchema],
        ) -> Result<C, LlmError> {
            self.captured
                .lock()
                .unwrap()
                .push(messages.to_vec());
            self.step
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| LlmError::Config("脚本耗尽".into()))
        }
    }

    /// 宽松解析经验抽取输出（旧格式，保留给解析兼容性测试）。
    fn parse_experience_json(text: &str) -> Vec<(String, Vec<String>)> {
        let raw = text.trim();
        let start = match raw.find('[') {
            Some(i) => i,
            None => return Vec::new(),
        };
        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
        let parsed: Value = match serde_json::from_str(&raw[start..end]) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        parsed
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let text = item.get("text")?.as_str()?.trim().to_string();
                        if text.is_empty() {
                            return None;
                        }
                        let tags = item
                            .get("tags")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|t| t.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        Some((text, tags))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn parse_experience_json_variants() {
        let v = parse_experience_json(r#"好的，结果：[{"text":"nmap -sV 探测版本","tags":["nmap","扫描"]}]"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "nmap -sV 探测版本");
        assert_eq!(v[0].1, vec!["nmap".to_string(), "扫描".to_string()]);

        assert!(parse_experience_json("[]").is_empty());
        assert!(parse_experience_json("无可提取").is_empty());
        // 缺 text 字段的项被跳过
        let v = parse_experience_json(r#"[{"nope":1},{"text":"有效经验"}]"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "有效经验");
    }

    #[tokio::test]
    async fn experience_extracted_into_store() {
        let store = Arc::new(ExperienceStore::open_tmp().unwrap());
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptProvider {
            steps: Mutex::new(vec![
                // 第三次调用：经验抽取（R1-4 固定字段 JSON，含一条死路）
                C {
                    content: Some(
                        r#"{"objective":"摸清 127.0.0.1 服务","findings":"22 端口开放","techniques":"util.echo 探活","dead_ends":["telnet 探测被拒"],"next_steps":"服务版本识别"}"#
                            .into(),
                    ),
                    tool_calls: vec![],
                    usage: Default::default(),
                },
                // 第二次调用：最终答复
                C { content: Some("扫描完成，22 端口开放。".into()), tool_calls: vec![], usage: Default::default() },
                // 第一次调用：工具调用
                C {
                    content: None,
                    tool_calls: vec![ToolCall { id: "c1".into(), name: "util.echo".into(), arguments: "{}".into() }],
                    usage: Default::default(),
                },
            ]),
        });
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let events: ChatEventCallback = Arc::new(move |ev| { let _ = evt_tx.send(ev); });
        let risks: HashMap<String, RiskLevel> = HashMap::from([("util.echo".to_string(), RiskLevel::None)]);
        let mut s = ChatSession::new(
            Some(provider),
            Arc::new(MockExecutor),
            schemas(),
            risks,
            ChatConfig { authorized_scope: vec!["127.0.0.1".into()], extract_every: 1, ..Default::default() },
            Arc::new(allow_all),
        )
        .with_memory(store.clone())
        .with_events(events);
        let t = s.turn("扫描一下").await;
        assert!(t.ok);
        // R1-4：主摘要 1 条 + dead_ends 1 条 = 2 条入库
        assert_eq!(store.count(), 2);
        let all = store.all();
        let summary = all.iter().find(|e| e.kind == base_store::KIND_SUMMARY).unwrap();
        assert!(summary.text.contains("目标"), "主摘要应含固定字段：{}", summary.text);
        assert!(summary.text.contains("发现") && summary.text.contains("待办"));
        let de = all.iter().find(|e| e.kind == base_store::KIND_DEAD_END).unwrap();
        assert!(de.text.contains("telnet"), "死路应单独入库：{}", de.text);
        loop {
            match evt_rx.recv().await {
                Some(ChatEvent::ExperienceSaved { count }) => {
                    assert_eq!(count, 2);
                    break;
                }
                Some(_) => continue, // 跳过轮次/工具过程事件
                other => panic!("期望 ExperienceSaved，得到 {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn experience_injection_respects_token_budget() {
        use base_store::{ExperienceStore as ES, KIND_DEAD_END};
        let store = Arc::new(ES::open_tmp().unwrap());
        // 两条长经验（各约 900 token 中文）→ 1500 预算只装得下第一条
        let long_a = "甲".repeat(600);
        let long_b = "乙".repeat(600);
        store.add(&long_a, vec!["a".into()]).unwrap();
        store.add(&long_b, vec!["b".into()]).unwrap();
        // 死路：无关查询不应注入
        store.add_typed("无关查询的死路经验描述", vec!["x".into()], KIND_DEAD_END).unwrap();

        let (evt_tx, _evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let events: ChatEventCallback = Arc::new(move |ev| { let _ = evt_tx.send(ev); });
        let concrete = Arc::new(CaptureProvider {
            step: Mutex::new(vec![C {
                content: Some("ok".into()),
                tool_calls: vec![],
                usage: Default::default(),
            }]),
            captured: Mutex::new(Vec::new()),
        });
        let provider: Arc<dyn LlmProvider> = concrete.clone();
        let mut s = ChatSession::new(
            Some(provider),
            Arc::new(MockExecutor),
            schemas(),
            HashMap::new(),
            ChatConfig {
                authorized_scope: vec!["127.0.0.1".into()],
                context_window_tokens: 0,
                history_cap: 8,
                ..Default::default()
            },
            Arc::new(allow_all),
        )
        .with_memory(store.clone())
        .with_events(events);
        let t = s.turn("与经验完全无关的查询").await;
        assert!(t.ok);
        // 注入内容检查：预算只装下第一条长经验；死路不进兜底
        let msgs = concrete.captured.lock().unwrap().last().unwrap().clone();
        let exp: Vec<&str> = msgs
            .iter()
            .filter(|m| m.content.contains("经验库"))
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(exp.len(), 1, "应注入一条经验库 system 消息");
        // 两条长经验各自约 1200 token，1500 预算只装得下第一条（兜底顺序按时间戳，不确定先后）
        let has_a = exp[0].contains("甲甲");
        let has_b = exp[0].contains("乙乙");
        assert!(has_a ^ has_b, "应恰好注入一条长经验：{}", exp[0]);
        assert!(!exp[0].contains("死路经验"), "无关查询死路不应注入");
    }

    #[test]
    fn default_summary_interval_is_five_turns() {
        assert_eq!(ChatConfig::default().extract_every, 5);
    }

    #[test]
    fn token_estimate_and_80pct_defaults() {
        let cfg = ChatConfig::default();
        assert!(cfg.context_window_tokens >= 8_000);
        assert!((cfg.compact_threshold - 0.8).abs() < 1e-9);
        assert!(cfg.keep_recent_turns >= 3 && cfg.keep_recent_turns <= 5);
        // 中文比 ASCII 更"贵"
        assert!(estimate_text_tokens("中文自然语言句子") > estimate_text_tokens("scan port"));
    }

    #[test]
    fn keep_recent_turns_counts_full_rounds() {
        let mut h = VecDeque::new();
        for _ in 0..5 {
            h.push_back(user_message("u".to_string()));
            h.push_back(assistant_message("a".to_string()));
        }
        assert_eq!(count_trailing_turn_msgs(&h, 3), 6);
        assert_eq!(count_trailing_turn_msgs(&h, 4), 8);
    }

    #[tokio::test]
    async fn recall_injects_old_session_summary() {
        let store = Arc::new(base_store::SessionStore::open_tmp().unwrap());
        let rec = store.create("旧会话标题", "ws", "profile").unwrap();
        store
            .save_state(
                &rec.id,
                base_store::SessionState {
                    rolling_summary: Some("【关键发现】80/22 端口开放".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut s = session(None, Arc::new(allow_all)).with_sessions(store);
        let t = s.turn("/recall ").await;
        assert!(!t.ok, "空 id 应失败");
        let t = s.turn(&format!("/recall {}", rec.id)).await;
        assert!(t.ok, "{}", t.reply);
        assert!(t.reply.contains("旧会话标题"));
        assert!(t.trace[0].contains("[OK] /recall"));
        // 注入的 system 消息进入窗口历史
        let snap = s.state_snapshot();
        assert!(snap
            .history
            .iter()
            .any(|m| m.role == ChatRole::System && m.content.contains("80/22 端口开放")));
    }

    #[test]
    fn sanitize_matches_openai_pattern() {
        assert_eq!(sanitize_name("net.http"), "net_http");
        assert_eq!(sanitize_name("scan.web-vuln-internal"), "scan_web-vuln-internal");
    }

    /// R3-1：插件前置钩子拦截工具调用（Deny 不进审批、Ask 提升至审批）。
    struct DenyEchoHook;
    impl toolkit_core::hooks::ToolHook for DenyEchoHook {
        fn name(&self) -> &str {
            "test-deny-echo"
        }
        fn on_tool_call(&self, tool: &str, _: RiskLevel, _: &str) -> toolkit_core::hooks::HookVerdict {
            if tool == "util.echo" {
                toolkit_core::hooks::HookVerdict::Deny("echo 被测试策略禁止".into())
            } else {
                toolkit_core::hooks::HookVerdict::Allow
            }
        }
    }

    #[tokio::test]
    async fn hook_denies_tool_before_execution() {
        let mut s = session(None, Arc::new(allow_all))
            .with_hook(std::sync::Arc::new(DenyEchoHook));
        let mut trace = Vec::new();
        let obs = s
            .run_tool("util.echo", r#"{"message":"hi"}"#, &mut trace)
            .await;
        assert!(obs.contains("插件钩子"), "观察应含拦截原因：{obs}");
        assert!(obs.contains("echo 被测试策略禁止"));
        assert!(!obs.contains("hi"), "被拦截调用不得真正执行");
        assert!(trace.iter().any(|t| t.contains("钩子拦截")));
    }

    #[tokio::test]
    async fn session_allowlist_skips_approval_callback() {
        // R2-4：白名单命中的高风险工具不触发审批回调；未白名单的正常触发
        let calls = Arc::new(Mutex::new(0usize));
        let c2 = calls.clone();
        let approval: ApprovalCallback = Arc::new(move |_t, _r, _a| {
            *c2.lock().unwrap() += 1;
            true
        });
        let mut s = ChatSession::new(
            None,
            Arc::new(MockExecutor),
            schemas(),
            HashMap::from([("util.echo".to_string(), RiskLevel::High)]),
            ChatConfig::default(),
            approval,
        );
        let mut trace = Vec::new();
        let o = s.run_tool("util.echo", "{}", &mut trace).await;
        assert!(o.contains("\"ok\":true"));
        assert_eq!(*calls.lock().unwrap(), 1, "未白名单应触发审批回调");
        s.allow_tool("util.echo");
        let o = s.run_tool("util.echo", "{}", &mut trace).await;
        assert!(o.contains("\"ok\":true"));
        assert_eq!(*calls.lock().unwrap(), 1, "白名单命中不应再触发审批回调");
        // 重建会话后经 set_allowlist 恢复
        let mut s2 = ChatSession::new(
            None,
            Arc::new(MockExecutor),
            schemas(),
            HashMap::from([("util.echo".to_string(), RiskLevel::High)]),
            ChatConfig::default(),
            Arc::new(|_t, _r, _a| false),
        );
        s2.set_allowlist(vec!["util.echo".to_string()]);
        let o = s2.run_tool("util.echo", "{}", &mut trace).await;
        assert!(o.contains("\"ok\":true"), "set_allowlist 恢复后应免审：{o}");
    }

    #[test]
    fn fingerprint_call_order_insensitive() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(fingerprint_call("t1", &a), fingerprint_call("t1", &b));
        assert_ne!(fingerprint_call("t1", &a), fingerprint_call("t2", &a));
        let nested = json!({"x": {"p": [1, 2], "q": "s"}});
        let nested2 = json!({"x": {"q": "s", "p": [1, 2]}});
        assert_eq!(fingerprint_call("t", &nested), fingerprint_call("t", &nested2));
    }

    #[tokio::test]
    async fn duplicate_call_guard_escalates() {
        let mut s = session(None, Arc::new(allow_all));
        let mut trace = Vec::new();
        let args = r#"{"msg":"hi"}"#;
        let o1 = s.run_tool("util.echo", args, &mut trace).await;
        assert!(!o1.contains("guard_note"), "首次调用不应提醒");
        s.run_tool("util.echo", args, &mut trace).await;
        let o3 = s.run_tool("util.echo", args, &mut trace).await;
        assert!(o3.contains("第 3 次"), "第 3 次触发 gentle 提醒：{o3}");
        s.run_tool("util.echo", args, &mut trace).await;
        let o5 = s.run_tool("util.echo", args, &mut trace).await;
        assert!(o5.contains("第 5 次"), "第 5 次触发警告：{o5}");
        // 换参数 → 计数重置
        let o = s.run_tool("util.echo", r#"{"msg":"changed"}"#, &mut trace).await;
        assert!(!o.contains("guard_note"), "参数变化应重置计数：{o}");
    }

    #[tokio::test]
    async fn rule_mode_lists_full_tool_help() {
        let mut s = session(None, Arc::new(allow_all));
        let t = s.turn("help").await;
        assert!(t.reply.contains("全量工具命令集"), "reply={}", t.reply);
        assert!(t.reply.contains("/call"), "reply={}", t.reply);
        assert!(t.reply.contains("util.echo"), "reply={}", t.reply);
        let t2 = s.turn("?").await;
        assert!(t2.reply.contains("已注册工具"));
    }

    #[test]
    fn schema_params_extracts_required() {
        let schema = json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {"type": "string"},
                "timeout": {"type": "number"}
            }
        });
        let p = schema_params(&schema);
        assert!(p.contains("url*:string"), "p={p}");
        assert!(p.contains("timeout:number"), "p={p}");
    }

    #[tokio::test]
    async fn five_turn_rule_summary_written_to_experience() {
        // 无 LLM：5 轮后规则降级摘要一次，写入一条带「对话摘要」标签的经验
        let store = Arc::new(ExperienceStore::open_tmp().unwrap());
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let events: ChatEventCallback = Arc::new(move |ev| {
            let _ = evt_tx.send(ev);
        });
        let mut s = ChatSession::new(
            None,
            Arc::new(MockExecutor),
            schemas(),
            HashMap::new(),
            ChatConfig {
                authorized_scope: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            Arc::new(allow_all),
        )
        .with_memory(store.clone())
        .with_events(events);
        for n in 0..4 {
            let _ = s.turn(&format!("扫描 127.0.0.1 第 {} 轮", n + 1)).await;
        }
        assert_eq!(store.count(), 0, "未满 5 轮不应入库");
        let _ = s.turn("扫描 127.0.0.1 第 5 轮").await;
        assert_eq!(store.count(), 1, "第 5 轮应写入一条摘要");
        let all = store.all();
        assert!(all[0].text.starts_with("第1-5轮对话："), "text={}", all[0].text);
        assert!(all[0].text.contains("扫描 127.0.0.1"), "text={}", all[0].text);
        assert_eq!(all[0].tags, vec!["对话摘要", "轮1-5"]);
        // 事件只发一次
        let mut saved = 0;
        while let Ok(ev) = evt_rx.try_recv() {
            if let ChatEvent::ExperienceSaved { count } = ev {
                saved += count;
            }
        }
        assert_eq!(saved, 1);
    }

    #[tokio::test]
    async fn history_compression_produces_summary() {
        // 预置 5 轮历史（10 条），keep_recent_turns=3 → 保留最近 3 轮（6 条），压缩最旧 4 条。
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptProvider {
            steps: Mutex::new(vec![
                C { content: Some("【目标与授权范围】本机系统巡检\n【关键发现】磁盘可用空间充足".into()), tool_calls: vec![], usage: Default::default() },
            ]),
        });
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let events: ChatEventCallback = Arc::new(move |ev| { let _ = evt_tx.send(ev); });
        let risks: HashMap<String, RiskLevel> = HashMap::from([("util.echo".to_string(), RiskLevel::None)]);
        let mut s = ChatSession::new(
            Some(provider),
            Arc::new(MockExecutor),
            schemas(),
            risks,
            ChatConfig {
                authorized_scope: vec!["127.0.0.1".into()],
                history_cap: 4,
                summarize_history: true,
                extract_every: 0,
                context_window_tokens: 0, // 条数兜底触发（短文本达不到 token 阈值）
                keep_recent_turns: 3,
                ..Default::default()
            },
            Arc::new(allow_all),
        )
        .with_events(events);
        // 直接恢复 5 轮历史（10 条消息）
        let mut hist = Vec::new();
        for i in 0..5 {
            hist.push(user_message(format!("第{i}轮：扫描 127.0.0.1")));
            hist.push(assistant_message(format!("第{i}轮：22 端口开放")));
        }
        s.restore_history(hist, 5);
        s.maybe_compress_history().await;
        loop {
            match evt_rx.recv().await {
                Some(ChatEvent::MemoryCompressed { compressed, kept }) => {
                    assert_eq!(compressed, 4, "应压缩最旧 2 轮=4 条");
                    assert_eq!(kept, 6, "应保留最近 3 轮=6 条");
                    break;
                }
                Some(_) => continue,
                other => panic!("期望 MemoryCompressed，得到 {other:?}"),
            }
        }
        assert!(s.rolling_summary().is_some());
        assert!(s.rolling_summary().unwrap().contains("关键发现"));
        // R1-3 可逆压缩：被淘汰的 4 条进归档，快照可整体还原
        let snap = s.state_snapshot();
        assert_eq!(snap.archived.len(), 4, "压缩淘汰的原始消息应进归档");
        assert_eq!(snap.history.len(), 6);
        assert!(snap.rolling_summary.is_some());
        // roundtrip：新会话 restore_full 后历史/摘要/归档一致
        let mut s2 = ChatSession::new(
            None,
            Arc::new(MockExecutor),
            schemas(),
            HashMap::new(),
            ChatConfig::default(),
            Arc::new(allow_all),
        );
        s2.restore_full(snap.clone(), 5);
        let snap2 = s2.state_snapshot();
        assert_eq!(snap2.history.len(), snap.history.len());
        assert_eq!(snap2.archived.len(), 4);
        assert_eq!(snap2.rolling_summary, snap.rolling_summary);
        assert_eq!(s2.turn_no(), 5);
    }

    #[tokio::test]
    async fn state_snapshot_preserves_tool_calls() {
        // 协议消息含 tool_calls/tool 结果时，快照与还原必须原样保留（dsh：模型可见即已记录）
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let events: ChatEventCallback = Arc::new(move |ev| { let _ = evt_tx.send(ev); });
        let mut s = ChatSession::new(
            None,
            Arc::new(MockExecutor),
            schemas(),
            HashMap::new(),
            ChatConfig::default(),
            Arc::new(allow_all),
        )
        .with_events(events);
        let tcs = vec![base_llm::ToolCall {
            id: "call-1".into(),
            name: "net_tcp_scan".into(),
            arguments: r#"{"host":"127.0.0.1"}"#.into(),
        }];
        let tool_msg = base_llm::ChatMessage {
            role: base_llm::ChatRole::Tool,
            content: "22,80 open".into(),
            tool_calls: None,
            tool_call_id: Some("call-1".into()),
        };
        let mut hist = vec![
            user_message("扫端口"),
            {
                let mut m = assistant_message("");
                m.tool_calls = Some(tcs.clone());
                m
            },
            tool_msg,
            assistant_message("发现 22/80 开放"),
        ];
        let mut m2 = assistant_message("中间轮");
        m2.tool_calls = Some(tcs.clone());
        hist.push(user_message("继续"));
        hist.push(m2);
        s.restore_full(
            ChatSnapshot { history: hist, rolling_summary: None, archived: vec![] },
            2,
        );
        let snap = s.state_snapshot();
        assert_eq!(snap.history.len(), 6, "4+2 条协议消息应原样保留");
        let with_calls: Vec<&base_llm::ChatMessage> =
            snap.history.iter().filter(|m| m.tool_calls.is_some()).collect();
        assert_eq!(with_calls.len(), 2, "tool_calls 应在快照中原样保留");
        let tool_msg = snap.history.iter().find(|m| m.tool_call_id.is_some()).unwrap();
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call-1"));
    }

    #[tokio::test]
    async fn skills_and_experience_injected_into_context() {
        let store = Arc::new(ExperienceStore::open_tmp().unwrap());
        store.add("Grafana 默认端口 3000", vec!["grafana".into()]).unwrap();
        let concrete = Arc::new(CaptureProvider {
            step: Mutex::new(vec![C { content: Some("ok".into()), tool_calls: vec![], usage: Default::default() }]),
            captured: Mutex::new(vec![]),
        });
        let provider: Arc<dyn LlmProvider> = concrete.clone();
        let risks: HashMap<String, RiskLevel> = HashMap::new();
        let s = ChatSession::new(
            Some(provider),
            Arc::new(MockExecutor),
            schemas(),
            risks,
            ChatConfig { authorized_scope: vec!["127.0.0.1".into()], ..Default::default() },
            Arc::new(allow_all),
        )
        .with_memory(store)
        .with_skills(vec![SkillNote {
            name: "attack-chain".into(),
            trigger: "通用 分析链".into(),
            body: "## 通用分析链规划\n检索 → 归纳 → 验证".into(),
            keywords: vec!["分析链".into()],
            available: true,
        }]);
        let mut s = s;
        s.turn("用分析链思路评估示例对象").await;

        let captured = concrete.captured.lock().unwrap();
        let msgs = &captured[0];
        // system(角色) + system(经验) + user；R3-5：system 只含技能目录（名称+触发），正文按需 skill.load
        assert!(msgs[0].content.contains("attack-chain"));
        assert!(msgs[0].content.contains("通用 分析链"));
        assert!(msgs[0].content.contains("skill.load"));
        assert!(!msgs[0].content.contains("通用分析链规划"), "正文不得全文注入 system prompt");
        assert!(msgs[1].content.contains("经验库"));
        assert!(msgs[1].content.contains("Grafana 默认端口 3000"));
        assert!(msgs.last().unwrap().content.contains("示例对象"));
    }

    /// util.echo 置为 High（触发审批路径）的会话（审批模式测试用）。
    fn high_risk_session(approval: ApprovalCallback) -> ChatSession {
        let mut risks = HashMap::new();
        risks.insert("util.echo".to_string(), RiskLevel::High);
        ChatSession::new(
            None,
            Arc::new(MockExecutor),
            schemas(),
            risks,
            ChatConfig {
                authorized_scope: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            approval,
        )
    }

    #[tokio::test]
    async fn approval_mode_all_allow_bypasses_callback() {
        // 回调一律拒绝；Some(true) 全放行模式应短路回调直接放行。
        let mut s = high_risk_session(Arc::new(|_, _, _| false));
        s.set_approval_mode(Some(true));
        let mut trace = Vec::new();
        let obs = s.run_tool("util.echo", "{}", &mut trace).await;
        assert!(obs.contains("\"ok\":true"), "全放行模式应执行成功: {obs}");
        assert!(!trace.iter().any(|t| t.contains("审批拒绝")));
    }

    #[tokio::test]
    async fn approval_mode_all_deny_blocks_callback() {
        // 回调一律放行；Some(false) 全拒绝模式应短路回调直接拒绝。
        let mut s = high_risk_session(Arc::new(|_, _, _| true));
        s.set_approval_mode(Some(false));
        let mut trace = Vec::new();
        let obs = s.run_tool("util.echo", "{}", &mut trace).await;
        assert!(obs.contains("审批拒绝"));
        assert!(trace.iter().any(|t| t.contains("审批拒绝")));
    }

    #[tokio::test]
    async fn denylist_blocks_target_hard() {
        // 问题10：目标命中拒绝清单（denylist）→ 不进审批、不执行。
        let mut s = session(None, Arc::new(allow_all));
        // 授权范围必须显式声明（未声明 = 带目标一律拒绝），此测试针对黑名单优先级。
        s.cfg.authorized_scope = vec!["10.0.0.0/24".into(), "192.168.1.16".into()];
        s.cfg.denylist = vec!["192.168.1.16".into()];
        let mut trace = Vec::new();
        let obs = s
            .run_tool("util.echo", r#"{"host":"192.168.1.16"}"#, &mut trace)
            .await;
        assert!(obs.contains("拒绝清单"), "{obs}");
        assert!(trace.iter().any(|t| t.contains("拒绝清单")));
        // 未命中拒绝清单的正常目标不受影响
        let obs2 = s
            .run_tool("util.echo", r#"{"host":"10.0.0.5"}"#, &mut trace)
            .await;
        assert!(obs2.contains("\"ok\":true"), "{obs2}");
    }

    #[test]
    fn trim_detail_keeps_head_and_tail() {
        // 问题11：超上限的 stdout 保留头尾并标注截断，不静默丢失全部内容。
        let big: String = "A".repeat(5000);
        let v = json!({"stdout": big, "exit_code": 0});
        let out = trim_detail_for_llm(&v, 1200);
        let s = out.to_string();
        assert!(s.contains("中间截断"), "{s}");
        assert!(s.contains('A'));
        // 上限内原样透传
        let small = json!({"stdout": "abc", "exit_code": 0});
        assert_eq!(trim_detail_for_llm(&small, 1200), small);
    }

    // ---------- S-6 阶段收敛：工具面过滤器 ----------

    #[test]
    fn tool_filter_hides_tools_per_call() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let allow = Arc::new(AtomicBool::new(false));
        let flag = allow.clone();

        // 两个工具：util.echo（基座，无归属组）+ scan.port（业务组）
        let mut s = session(None, Arc::new(allow_all));
        let both = vec![
            ToolSchema { name: "util.echo".into(), description: "回显".into(), parameters: json!({"type":"object"}) },
            ToolSchema { name: "scan.port".into(), description: "扫描".into(), parameters: json!({"type":"object"}) },
        ];
        let mut risks = HashMap::new();
        risks.insert("util.echo".to_string(), RiskLevel::None);
        risks.insert("scan.port".to_string(), RiskLevel::Medium);
        s.update_tools(both, risks);
        let s = s.with_tool_filter(Arc::new(move |name: &str| {
            name == "util.echo" || flag.load(Ordering::Relaxed)
        }));

        let names = |v: Vec<ToolSchema>| v.into_iter().map(|t| t.name).collect::<Vec<_>>();

        // 业务工具被挡住：schema 与 prompt 清单都不列
        assert_eq!(names(s.visible_schemas()), vec!["util.echo"]);
        assert!(!s.system_prompt("").contains("scan.port"));
        // 开关翻转（等价于 plan.phase 推进）→ 当场生效，不用重建会话
        allow.store(true, Ordering::Relaxed);
        assert_eq!(names(s.visible_schemas()), vec!["util.echo", "scan.port"]);
        assert!(s.system_prompt("").contains("scan.port"));
    }

    // ---------- S-7 技能预筛 ----------

    fn note(name: &str, kw: &[&str], available: bool) -> SkillNote {
        SkillNote {
            name: name.into(),
            trigger: format!("{name} 触发场景"),
            body: format!("{name} 正文"),
            keywords: kw.iter().map(|s| s.to_string()).collect(),
            available,
        }
    }

    #[test]
    fn skill_dir_hides_unavailable_and_marks_hits() {
        let mut s = session(None, Arc::new(allow_all));
        s.update_skills(vec![
            note("recon-playbook", &["侦察", "扫端口"], true),
            note("verify-playbook", &["验证", "漏洞"], true),
            note("exploit-boundary", &["利用"], false), // 依赖未装
        ]);
        let dir = s.skill_directory("先做一轮端口侦察");
        assert!(dir.contains("★ recon-playbook"), "{dir}");
        assert!(dir.contains("verify-playbook"), "未命中也保留（只排后面）");
        assert!(!dir.contains("exploit-boundary"), "依赖未装的技能不得进目录");
        // 非命中项不带星
        let verify_line = dir
            .lines()
            .find(|l| l.contains("verify-playbook"))
            .unwrap();
        assert!(verify_line.starts_with("- "), "{verify_line}");
    }

    #[test]
    fn skill_dir_caps_entries_but_keeps_hits() {
        let mut s = session(None, Arc::new(allow_all));
        let mut skills: Vec<SkillNote> = (0..20)
            .map(|i| note(&format!("bg-{i}"), &[&format!("无关{i}")], true))
            .collect();
        skills.push(note("target-skill", &["渗透"], true));
        s.update_skills(skills);
        let dir = s.skill_directory("做一次渗透测试");
        assert!(dir.contains("★ target-skill"), "命中项必须保留：{dir}");
        let listed = dir.lines().filter(|l| l.starts_with("- ") || l.starts_with("★ ")).count();
        assert_eq!(listed, SKILL_DIR_CAP);
        assert!(dir.contains("另有"), "截断要注明还有多少：{dir}");
    }

    #[test]
    fn skill_dir_empty_when_all_unavailable() {
        let mut s = session(None, Arc::new(allow_all));
        s.update_skills(vec![note("x", &[], false)]);
        assert!(s.skill_directory("随便").is_empty());
    }

    #[test]
    fn looks_like_plan_detects_plan_text() {
        assert!(looks_like_plan("任务目标：整理资料\n1. 收集素材\n2. 归纳要点\n停止条件：形成结论"));
        assert!(looks_like_plan("计划如下\n1、先收集信息"));
        assert!(!looks_like_plan("这是普通答复，不含计划结构"));
        assert!(!looks_like_plan("步骤这两个字单独出现"));
    }

    // ---------- ScopeGuard 主链路：授权范围硬校验 ----------

    #[test]
    fn normalize_target_strips_scheme_path_port() {
        assert_eq!(normalize_target("https://a.com/x/y?q=1#f"), "a.com");
        assert_eq!(normalize_target("10.0.0.5:8080"), "10.0.0.5");
        assert_eq!(normalize_target("user:pw@host.example.com:22"), "host.example.com");
        assert_eq!(normalize_target("[2001:db8::1]:443"), "2001:db8::1");
        // 裸 IPv6（多冒号）不被端口切割误伤
        assert_eq!(normalize_target("2001:db8::1"), "2001:db8::1");
        assert_eq!(normalize_target("  "), "");
    }

    #[test]
    fn targets_in_args_collects_dedup_ordered() {
        let args = json!({
            "target": "https://a.com/x",
            "url": "http://a.com/y",      // 归一后与上一条相同 → 去重
            "host": "b.com",
            "targets": ["c.com", "b.com"],
            "name": "不要当成目标"
        });
        assert_eq!(targets_in_args(&args), vec!["a.com", "b.com", "c.com"]);
        // 无目标字段 → 空（纯配置类工具不受范围限制）
        assert!(targets_in_args(&json!({"name": "ws1", "path": "D:/x"})).is_empty());
    }

    #[test]
    fn scope_denial_fail_closed() {
        let scope: Vec<String> = vec!["10.0.0.0/24".into()];
        let deny: Vec<String> = vec!["10.0.0.7".into()];

        // 1) 无目标参数 → 放行（不涉及目标的操作不该被范围规则拦住）
        assert!(scope_denial(&scope, &deny, &json!({"name": "x"})).is_none());
        // 2) 范围内 → 放行
        assert!(scope_denial(&scope, &deny, &json!({"target": "10.0.0.5"})).is_none());
        // 3) 范围外 → 拒绝
        let r = scope_denial(&scope, &deny, &json!({"host": "10.0.1.5"})).expect("应拒绝");
        assert!(r.contains("未通过授权校验"), "{r}");
        // 4) 命中 denylist（黑名单优先，即使落在白名单里） → 拒绝
        let r = scope_denial(&scope, &deny, &json!({"target": "10.0.0.7"})).expect("应拒绝");
        assert!(r.contains("拒绝清单") || r.contains("未通过"), "{r}");
        // 5) 空授权范围 + 带目标 → 一律拒绝（fail-closed）
        let r = scope_denial(&[], &[], &json!({"url": "https://example.com/a"})).expect("应拒绝");
        assert!(r.contains("未声明授权范围"), "{r}");
        // 6) 空授权范围 + 无目标 → 仍放行
        assert!(scope_denial(&[], &[], &json!({"path": "D:/x"})).is_none());
        // 7) 全部条目非法 → 授权面为空 → 仍然拒绝（fail-closed），并说明问题所在
        let bad: Vec<String> = vec!["10.0.0.0/99".into()];
        let r = scope_denial(&bad, &[], &json!({"target": "10.0.0.5"})).expect("应拒绝");
        assert!(r.contains("没有任何可用条目"), "{r}");
        assert!(r.contains("无法识别"), "{r}");
        // 8) 一条写错不牵连其余：合法条目照常生效（写错的那条只丢自己）
        let mixed: Vec<String> = vec!["10.0.0.0/24".into(), "10.0.0.0/99".into(), "localhost".into()];
        assert!(scope_denial(&mixed, &[], &json!({"target": "10.0.0.5"})).is_none());
        assert!(scope_denial(&mixed, &[], &json!({"host": "localhost"})).is_none());
        assert!(scope_denial(&mixed, &[], &json!({"host": "10.0.1.5"})).is_some());
        // 9) 单标签主机名只做全等，不能顺带授权子域
        assert!(scope_denial(&mixed, &[], &json!({"host": "foo.localhost"})).is_some());
        // 10) 写错的拒绝条目退化为字面包含（拒绝方向只能更严）
        let r = scope_denial(&mixed, &["10.0.0.0/XX".into()], &json!({"host": "10.0.0.5"}))
            .expect("应拒绝");
        assert!(r.contains("未通过授权校验"), "{r}");
    }

    #[tokio::test]
    async fn scope_guard_blocks_out_of_scope_target() {
        // 安全回归：授权范围外的目标必须弹审批；审批拒绝则不执行。
        // （本版按诉求改为「超授权走审批」而非硬拒绝，用户可当场授权；
        //   deny_all 模拟用户点「拒绝」，验证仍拦得住。）
        let mut s = session(None, Arc::new(deny_all));
        s.cfg.authorized_scope = vec!["10.0.0.0/24".into()];
        let mut trace = Vec::new();
        let obs = s
            .run_tool("util.echo", r#"{"host":"192.168.1.16"}"#, &mut trace)
            .await;
        assert!(obs.contains("未通过授权校验"), "{obs}");
        // 范围内照常执行（不触发审批）
        let obs2 = s
            .run_tool("util.echo", r#"{"host":"10.0.0.8"}"#, &mut trace)
            .await;
        assert!(obs2.contains("\"ok\":true"), "{obs2}");
    }
}
