//! 规划层：结构化的任务 DAG（`plan.*` 三个工具 + 会话内共享状态）。
//!
//! 与 `plan.exit`（人工评审门）的分工：
//! - `plan.exit` 管**要不要做**：复杂动作先交计划给人批
//! - `plan.create/update/view` 管**做到哪了**：把"我要做 A、B、C"变成程序可核对的状态
//!
//! 为什么不让模型自己在文本里记计划：文本计划无法校验（依赖写错、漏项、循环依赖
//! 都发现不了），也无法在下一轮准确知道"还剩哪几步"。结构化之后，每轮把
//! **可执行的下一步**（ready 集合）回填给模型，它就不必重读整段历史来回忆进度。
//!
//! 状态刻意做小：只有 `todo / doing / done / blocked` 四态。加 `review`、`failed`
//! 这类状态会让"计划整体是否完成"变得需要额外解释，得不偿失。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kern_core::model::{ExecutionContext, RiskLevel};
use serde_json::{json, Value};
use toolkit_core::tools::{Tool, ToolResult};

/// 一条计划项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    /// 声明的能力标签（可选；用于让执行层核对"这一步需要什么能力"）
    pub needs: Vec<String>,
    /// 依赖的前置项 id（必须全部 done 才 ready）
    pub deps: Vec<String>,
    pub status: PlanStatus,
    /// 完成/受阻时的一句话说明
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Todo,
    Doing,
    Done,
    Blocked,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Todo => "todo",
            PlanStatus::Doing => "doing",
            PlanStatus::Done => "done",
            PlanStatus::Blocked => "blocked",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "todo" | "pending" => Some(PlanStatus::Todo),
            "doing" | "in_progress" | "started" => Some(PlanStatus::Doing),
            "done" | "completed" | "ok" => Some(PlanStatus::Done),
            "blocked" | "stuck" | "failed" => Some(PlanStatus::Blocked),
            _ => None,
        }
    }
}

/// 会话级计划状态。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanState {
    pub objective: String,
    pub items: Vec<PlanItem>,
}

impl PlanState {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 依赖已全部完成且自身未开始的项 = 现在就可以做的那几步。
    pub fn ready(&self) -> Vec<&PlanItem> {
        let done: Vec<&str> = self
            .items
            .iter()
            .filter(|i| i.status == PlanStatus::Done)
            .map(|i| i.id.as_str())
            .collect();
        self.items
            .iter()
            .filter(|i| i.status == PlanStatus::Todo || i.status == PlanStatus::Doing)
            .filter(|i| i.deps.iter().all(|d| done.contains(&d.as_str())))
            .collect()
    }

    pub fn progress(&self) -> (usize, usize) {
        let done = self.items.iter().filter(|i| i.status == PlanStatus::Done).count();
        (done, self.items.len())
    }

    pub fn all_done(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|i| i.status == PlanStatus::Done)
    }

    /// 结构校验：id 唯一非空、依赖存在、无环。
    pub fn validate(&self) -> Result<(), String> {
        let mut ids: Vec<&str> = Vec::new();
        for i in &self.items {
            if i.id.trim().is_empty() {
                return Err("存在空 id 的计划项".into());
            }
            if ids.contains(&i.id.as_str()) {
                return Err(format!("计划项 id 重复：{}", i.id));
            }
            ids.push(&i.id);
        }
        for i in &self.items {
            for d in &i.deps {
                if d == &i.id {
                    return Err(format!("计划项 {} 依赖了自己", i.id));
                }
                if !ids.contains(&d.as_str()) {
                    return Err(format!("计划项 {} 依赖了不存在的 {}", i.id, d));
                }
            }
        }
        // 三色标记找环（节点数很小，递归深度可控）
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            White,
            Grey,
            Black,
        }
        let mut marks = vec![Mark::White; self.items.len()];
        fn idx(items: &[PlanItem], id: &str) -> Option<usize> {
            items.iter().position(|i| i.id == id)
        }
        fn visit(items: &[PlanItem], marks: &mut [Mark], at: usize) -> Result<(), String> {
            match marks[at] {
                Mark::Black => return Ok(()),
                Mark::Grey => return Err(format!("计划存在循环依赖：{}", items[at].id)),
                Mark::White => {}
            }
            marks[at] = Mark::Grey;
            for d in &items[at].deps {
                if let Some(j) = idx(items, d) {
                    visit(items, marks, j)?;
                }
            }
            marks[at] = Mark::Black;
            Ok(())
        }
        for i in 0..self.items.len() {
            visit(&self.items, &mut marks, i)?;
        }
        Ok(())
    }

    /// 回填给模型的计划视图（每轮注入，约 300 字以内）。
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let (done, total) = self.progress();
        let mut s = format!(
            "[当前计划] 目标：{}\n进度 {done}/{total}\n",
            if self.objective.trim().is_empty() { "(未写目标)" } else { &self.objective }
        );
        for i in &self.items {
            let mark = match i.status {
                PlanStatus::Done => "[x]",
                PlanStatus::Doing => "[>]",
                PlanStatus::Blocked => "[!]",
                PlanStatus::Todo => "[ ]",
            };
            let deps = if i.deps.is_empty() {
                String::new()
            } else {
                format!("（依赖 {}）", i.deps.join(","))
            };
            let note = if i.note.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", i.note)
            };
            s.push_str(&format!("{mark} {} {}{deps}{note}\n", i.id, i.title));
        }
        let ready: Vec<&str> = self.ready().iter().map(|i| i.id.as_str()).collect();
        if self.all_done() {
            s.push_str("全部步骤已完成，可以给出结论。");
        } else if ready.is_empty() {
            s.push_str("当前没有可执行步骤（检查是否有受阻项需要汇报）。");
        } else {
            s.push_str(&format!("可执行的下一步：{}。完成一步后调用 plan.update 标记。", ready.join("、")));
        }
        s
    }
}

/// 计划状态的共享句柄（工具与会话共用）。
pub type SharedPlan = Arc<Mutex<PlanState>>;

pub fn shared() -> SharedPlan {
    Arc::new(Mutex::new(PlanState::default()))
}

fn lock(p: &SharedPlan) -> Result<std::sync::MutexGuard<'_, PlanState>, String> {
    p.lock().map_err(|_| "计划状态锁中毒（上一次更新 panic 了）".to_string())
}

/// `plan.create`：建立（或替换）任务 DAG。
pub struct PlanCreateTool {
    plan: SharedPlan,
}

impl PlanCreateTool {
    pub fn new(plan: SharedPlan) -> Self {
        Self { plan }
    }
}

#[async_trait]
impl Tool for PlanCreateTool {
    fn name(&self) -> &str {
        "plan.create"
    }

    fn description(&self) -> String {
        "为一个多步骤任务建立结构化计划（任务 DAG），之后每轮都会把计划进度与「可执行的下一步」\
回填给你。参数：objective(必填，一句话目标)；tasks(必填，数组，每项 {id,title,deps?,needs?})。\
id 自己起短名（如 s1/s2）；deps 填前置任务 id，无依赖留空。\
只读小任务不必建计划；重建计划会覆盖旧计划（已有进度会丢失）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["objective", "tasks"],
            "properties": {
                "objective": { "type": "string", "description": "一句话目标（可验证的终态）" },
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["id", "title"],
                        "properties": {
                            "id": { "type": "string", "description": "短 id，如 s1" },
                            "title": { "type": "string", "description": "这一步做什么（可验证）" },
                            "deps": { "type": "array", "items": { "type": "string" }, "description": "前置任务 id" },
                            "needs": { "type": "array", "items": { "type": "string" }, "description": "需要的能力标签（可选）" }
                        }
                    }
                }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let objective = args
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let Some(tasks) = args.get("tasks").and_then(Value::as_array) else {
            return ToolResult::error("缺少参数 tasks（任务数组）", ms());
        };
        if tasks.is_empty() {
            return ToolResult::error("tasks 不能为空；只读小任务不必建计划", ms());
        }
        if tasks.len() > 40 {
            return ToolResult::error("tasks 过多（上限 40），请先收敛目标", ms());
        }
        let mut items: Vec<PlanItem> = Vec::new();
        for (n, t) in tasks.iter().enumerate() {
            let id = t
                .get("id")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("s{}", n + 1));
            let title = t
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if title.is_empty() {
                return ToolResult::error(format!("计划项 {id} 缺少 title"), ms());
            }
            let deps = t
                .get("deps")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let needs = t
                .get("needs")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            items.push(PlanItem {
                id,
                title,
                needs,
                deps,
                status: PlanStatus::Todo,
                note: String::new(),
            });
        }
        let next = PlanState { objective, items };
        if let Err(e) = next.validate() {
            return ToolResult::error(format!("计划不合法：{e}"), ms());
        }
        let view = next.render();
        match lock(&self.plan) {
            Ok(mut g) => *g = next,
            Err(e) => return ToolResult::error(e, ms()),
        }
        ToolResult::json(
            format!("计划已建立。{view}"),
            json!({ "ok": true, "tasks": tasks.len() }),
            ms(),
        )
    }
}

/// `plan.update`：推进/受阻/回退某一步。
pub struct PlanUpdateTool {
    plan: SharedPlan,
}

impl PlanUpdateTool {
    pub fn new(plan: SharedPlan) -> Self {
        Self { plan }
    }
}

#[async_trait]
impl Tool for PlanUpdateTool {
    fn name(&self) -> &str {
        "plan.update"
    }

    fn description(&self) -> String {
        "更新计划中某一步的状态。参数：id(必填，任务 id)；status(必填，doing/done/blocked/todo)；\
note(可选，一句话说明结果或受阻原因)。完成一步就标 done，不要攒到最后一起标——\
下一步的可用性由依赖关系决定。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id", "status"],
            "properties": {
                "id": { "type": "string" },
                "status": { "type": "string", "enum": ["todo", "doing", "done", "blocked"] },
                "note": { "type": "string", "description": "结果或受阻原因（一句话）" }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let id = args.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let Some(status) = args.get("status").and_then(Value::as_str).and_then(PlanStatus::parse)
        else {
            return ToolResult::error("status 只能是 todo/doing/done/blocked", ms());
        };
        let note = args.get("note").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let mut g = match lock(&self.plan) {
            Ok(g) => g,
            Err(e) => return ToolResult::error(e, ms()),
        };
        if g.is_empty() {
            return ToolResult::error("还没有计划；先调用 plan.create 建立计划", ms());
        }
        let Some(deps) = g.items.iter().find(|i| i.id == id).map(|i| i.deps.clone()) else {
            let ids: Vec<&str> = g.items.iter().map(|i| i.id.as_str()).collect();
            return ToolResult::error(format!("没有 id 为 {id} 的计划项（现有：{}）", ids.join(",")), ms());
        };
        // 未完成前置就标 done/doing ⇒ 明确告知，而不是悄悄允许
        if matches!(status, PlanStatus::Doing | PlanStatus::Done) {
            let done: Vec<String> = g
                .items
                .iter()
                .filter(|i| i.status == PlanStatus::Done)
                .map(|i| i.id.clone())
                .collect();
            let missing: Vec<String> = deps
                .iter()
                .filter(|d| !done.contains(d))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return ToolResult::error(
                    format!("{id} 的前置 {} 尚未完成，先完成前置（或说明为什么可以跳）", missing.join(",")),
                    ms(),
                );
            }
        }
        if let Some(item) = g.items.iter_mut().find(|i| i.id == id) {
            item.status = status;
            if !note.is_empty() {
                item.note = note;
            }
        }
        let view = g.render();
        drop(g);
        ToolResult::json(format!("已更新。{view}"), json!({ "ok": true, "id": id }), ms())
    }
}

/// `plan.view`：查看当前计划（模型不确定进度时调用）。
pub struct PlanViewTool {
    plan: SharedPlan,
}

impl PlanViewTool {
    pub fn new(plan: SharedPlan) -> Self {
        Self { plan }
    }
}

#[async_trait]
impl Tool for PlanViewTool {
    fn name(&self) -> &str {
        "plan.view"
    }

    fn description(&self) -> String {
        "查看当前计划与进度（无参数）。计划每轮会自动回填，通常不必主动调用；\
长对话后不确定做到哪一步时再用。".into()
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::None
    }

    async fn call(&self, _ctx: &ExecutionContext, _args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        match lock(&self.plan) {
            Ok(g) => {
                if g.is_empty() {
                    ToolResult::json("当前没有计划。", json!({ "ok": true, "empty": true }), ms())
                } else {
                    ToolResult::json(g.render(), json!({ "ok": true }), ms())
                }
            }
            Err(e) => ToolResult::error(e, ms()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kern_core::model::SessionId;

    fn ctx() -> ExecutionContext {
        ExecutionContext {
            session_id: SessionId::new(),
            agent_id: kern_core::model::AgentId::new("test"),
            task_id: kern_core::model::TaskId::new("t"),
            target: "test".into(),
            risk: RiskLevel::Low,
            approved: false,
            scope: vec![],
        }
    }

    fn item(id: &str, deps: &[&str]) -> PlanItem {
        PlanItem {
            id: id.into(),
            title: format!("做 {id}"),
            needs: vec![],
            deps: deps.iter().map(|s| s.to_string()).collect(),
            status: PlanStatus::Todo,
            note: String::new(),
        }
    }

    #[test]
    fn validate_catches_dup_missing_and_cycles() {
        let mut s = PlanState {
            objective: "x".into(),
            items: vec![item("a", &[]), item("a", &[])],
        };
        assert!(s.validate().unwrap_err().contains("重复"));

        s.items = vec![item("a", &["ghost"])];
        assert!(s.validate().unwrap_err().contains("不存在"));

        s.items = vec![item("a", &["b"]), item("b", &["a"])];
        assert!(s.validate().unwrap_err().contains("循环"));

        s.items = vec![item("a", &[]), item("b", &["a"])];
        assert!(s.validate().is_ok());
    }

    #[test]
    fn ready_waits_for_deps() {
        let s = PlanState {
            objective: "x".into(),
            items: vec![item("a", &[]), item("b", &["a"]), item("c", &["a"])],
        };
        assert_eq!(s.ready().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
        let mut s2 = s.clone();
        s2.items[0].status = PlanStatus::Done;
        let ready: Vec<&str> = s2.ready().iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ready, vec!["b", "c"]);
        s2.items[1].status = PlanStatus::Done;
        s2.items[2].status = PlanStatus::Done;
        assert!(s2.all_done());
    }

    #[test]
    fn render_marks_status_and_next_step() {
        let mut s = PlanState {
            objective: "跑通登录".into(),
            items: vec![item("a", &[]), item("b", &["a"])],
        };
        s.items[0].status = PlanStatus::Done;
        s.items[0].note = "接口通了".into();
        let r = s.render();
        assert!(r.contains("[x] a"));
        assert!(r.contains("[ ] b"));
        assert!(r.contains("可执行的下一步：b"));
        assert!(r.contains("接口通了"));
    }

    #[tokio::test]
    async fn create_then_update_flow() {
        let plan = shared();
        let create = PlanCreateTool::new(plan.clone());
        let r = create
            .call(
                &ctx(),
                json!({
                    "objective": "修好登录",
                    "tasks": [
                        {"id":"s1","title":"复现"},
                        {"id":"s2","title":"定位","deps":["s1"]}
                    ]
                }),
            )
            .await;
        assert!(r.ok, "{}", r.summary);

        let update = PlanUpdateTool::new(plan.clone());
        // 前置未完成就标 s2 done ⇒ 拒绝
        let bad = update.call(&ctx(), json!({"id":"s2","status":"done"})).await;
        assert!(!bad.ok);
        assert!(bad.summary.contains("前置"));
        // 按顺序推进
        assert!(update.call(&ctx(), json!({"id":"s1","status":"done","note":"稳定复现"})).await.ok);
        assert!(update.call(&ctx(), json!({"id":"s2","status":"done"})).await.ok);
        let view = PlanViewTool::new(plan).call(&ctx(), json!({})).await;
        assert!(view.summary.contains("2/2"));
        assert!(view.summary.contains("全部步骤已完成"));
    }

    #[tokio::test]
    async fn create_rejects_cycle_and_empty() {
        let plan = shared();
        let create = PlanCreateTool::new(plan);
        let r = create.call(&ctx(), json!({"objective":"x","tasks":[]})).await;
        assert!(!r.ok);
        let r = create
            .call(
                &ctx(),
                json!({"objective":"x","tasks":[{"id":"a","title":"A","deps":["b"]},{"id":"b","title":"B","deps":["a"]}]}),
            )
            .await;
        assert!(!r.ok);
        assert!(r.summary.contains("循环"));
    }
}
