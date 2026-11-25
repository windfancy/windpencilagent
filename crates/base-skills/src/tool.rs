//! `skill.load` 工具（R3-5 渐进披露）：system prompt 只注入技能目录，
//! 模型命中触发场景时按需拉取正文，技能数量增长不膨胀上下文。

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_core::tools::{ResultKind, Tool, ToolResult};
use kern_core::model::ExecutionContext;
use serde_json::Value;

use crate::manager::SkillManager;

/// 单次加载正文上限（超出截断并提示用 file.read 读完整文件）。
const BODY_LIMIT: usize = 8_000;

/// 按名称加载已启用技能正文的工具。
pub struct SkillLoadTool {
    mgr: Arc<SkillManager>,
}

impl SkillLoadTool {
    pub fn new(mgr: Arc<SkillManager>) -> Self {
        Self { mgr }
    }
}

#[async_trait]
impl Tool for SkillLoadTool {
    fn name(&self) -> &str {
        "skill.load"
    }

    fn description(&self) -> String {
        "按名称加载技能正文（方法论与步骤）。技能目录见系统提示词「可用技能目录」；\
命中某技能的触发场景时，先调用本工具获取正文再执行。"
            .into()
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "技能名（目录中的 name，如 attack-chain）"
                }
            },
            "required": ["name"]
        })
    }

    fn risk(&self) -> kern_core::model::RiskLevel {
        // 只读本地技能正文，无副作用
        kern_core::model::RiskLevel::Low
    }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = std::time::Instant::now();
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if name.is_empty() {
            return ToolResult {
                ok: false,
                kind: ResultKind::Error,
                summary: "缺少 name 参数（可用技能见系统提示词「可用技能目录」）".into(),
                detail: None,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
        let skills = self.mgr.enabled_skills();
        let Some(skill) = skills.iter().find(|s| s.manifest.name == name) else {
            let names: Vec<&str> = skills.iter().map(|s| s.manifest.name.as_str()).collect();
            return ToolResult {
                ok: false,
                kind: ResultKind::Error,
                summary: format!("技能 `{name}` 不存在或未启用。可用技能：{}", names.join(", ")),
                detail: None,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        };
        let body = skill.manifest.body.trim();
        if body.len() > BODY_LIMIT {
            // 截断保护：正文仍可经 file.read 回读完整文件
            let mut cut = body[..BODY_LIMIT].to_string();
            cut.push_str("\n\n[正文过长已截断，完整内容可 file.read 读取：");
            cut.push_str(&skill.manifest.path.to_string_lossy());
            cut.push(']');
            return ToolResult::text(cut, start.elapsed().as_millis() as u64);
        }
        ToolResult::text(body.to_string(), start.elapsed().as_millis() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec() -> ExecutionContext {
        ExecutionContext {
            scope: Vec::new(),
            session_id: kern_core::model::SessionId::new(),
            agent_id: kern_core::model::AgentId::new("t"),
            task_id: kern_core::model::TaskId::new("t"),
            target: "127.0.0.1".into(),
            risk: kern_core::model::RiskLevel::Low,
            approved: true,
        }
    }

    #[tokio::test]
    async fn loads_enabled_skill_body() {
        let tmp = std::env::temp_dir().join(format!("wpc-skload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("port-lore");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: port-lore\ntrigger: 端口扫描\n---\n先 SYN 再全连接",
        )
        .unwrap();
        let mgr = Arc::new(SkillManager::open(&tmp).unwrap());
        let tool = SkillLoadTool::new(mgr);
        let r = tool.call(&exec(), serde_json::json!({"name": "port-lore"})).await;
        assert!(r.ok);
        assert!(r.summary.contains("SYN"));
        // 不存在的技能报错并列出可用项
        let r2 = tool.call(&exec(), serde_json::json!({"name": "ghost"})).await;
        assert!(!r2.ok);
        assert!(r2.summary.contains("attack-chain"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
