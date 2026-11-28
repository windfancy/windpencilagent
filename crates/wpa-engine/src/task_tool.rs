//! `task.todo`：模型维护结构化任务清单（dsh TodoWrite 语义）。
//!
//! 与 `plan.*` / `subagent.spawn` 同属**智能体层工具**：不归任何能力组，
//! 不随领域收敛，装配时直接注册到内核。
//!
//! 语义：**整表覆盖**。模型每次传完整清单，未列出的旧条目会被移除；
//! 写入后由 `ChatSession` 每轮回注进上下文（`base_store::tasks` 注入段）。

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};

use base_store::tasks::{TaskStore, TASK_COMPLETED, TASK_IN_PROGRESS, TASK_PENDING};
use kern_core::model::{ExecutionContext, RiskLevel};
use toolkit_core::tools::{Tool, ToolResult};

pub struct TaskTodoTool(pub Arc<TaskStore>);

#[async_trait]
impl Tool for TaskTodoTool {
    fn name(&self) -> &str {
        "task.todo"
    }

    fn description(&self) -> String {
        "维护本次会话的任务清单（整表覆盖）：传入完整条目数组，未列出的旧条目会被移除。\
清单会在每轮对话回注给你，作为计划与进度的唯一依据。\
参数：items=[{subject, status}]，status ∈ todo / doing / done。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {
                    "type": "array",
                    "description": "完整任务清单（整表覆盖，不要只传增量）",
                    "items": {
                        "type": "object",
                        "required": ["subject"],
                        "properties": {
                            "subject": { "type": "string", "description": "任务标题" },
                            "status": {
                                "type": "string",
                                "enum": ["todo", "doing", "done"],
                                "description": "状态：todo=待办 / doing=进行中 / done=已完成"
                            }
                        }
                    }
                }
            }
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(arr) = args.get("items").and_then(Value::as_array) else {
            return ToolResult::error("缺少 items 数组（整表覆盖：传完整清单）", ms());
        };

        let mut items: Vec<(String, i32)> = Vec::new();
        for it in arr {
            let subject = it
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if subject.is_empty() {
                continue;
            }
            let status = match it.get("status").and_then(Value::as_str).unwrap_or("todo") {
                "done" => TASK_COMPLETED,
                "doing" => TASK_IN_PROGRESS,
                _ => TASK_PENDING,
            };
            items.push((subject.to_string(), status));
        }

        match self.0.replace_agent_todos(&items) {
            Ok(n) => {
                let detail = json!({
                    "count": n,
                    "items": items
                        .iter()
                        .map(|(s, st)| json!({ "subject": s, "status": st }))
                        .collect::<Vec<_>>(),
                });
                ToolResult::json(format!("任务清单已更新：{n} 条"), detail, ms())
            }
            Err(e) => ToolResult::error(format!("写入任务清单失败：{e}"), ms()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecutionContext {
        use kern_core::model::{AgentId, SessionId, TaskId};
        ExecutionContext {
            session_id: SessionId::new(),
            agent_id: AgentId::new("a-test"),
            task_id: TaskId::new("t-test"),
            target: String::new(),
            risk: RiskLevel::Low,
            approved: true,
            scope: Vec::new(),
        }
    }

    #[tokio::test]
    async fn todo_replaces_whole_list() {
        let dir = std::env::temp_dir().join(format!("wpa_task_todo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(TaskStore::open(&dir).unwrap());
        let tool = TaskTodoTool(store.clone());

        let r = tool
            .call(
                &ctx(),
                json!({"items": [
                    {"subject": "收集目标", "status": "done"},
                    {"subject": "端口扫描", "status": "doing"},
                    {"subject": "出报告"}
                ]}),
            )
            .await;
        assert!(r.ok, "{}", r.summary);
        let todos = store.agent_todos();
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].status, TASK_COMPLETED);
        assert_eq!(todos[1].status, TASK_IN_PROGRESS);
        assert_eq!(todos[2].status, TASK_PENDING);

        // 整表覆盖：只传一条，旧的三条应被清掉
        let r = tool
            .call(&ctx(), json!({"items": [{"subject": "只剩一条", "status": "todo"}]}))
            .await;
        assert!(r.ok);
        let todos = store.agent_todos();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].subject, "只剩一条");

        // 缺 items → 报错而不是清空
        assert!(!tool.call(&ctx(), json!({})).await.ok);
        assert_eq!(store.agent_todos().len(), 1);
    }
}
