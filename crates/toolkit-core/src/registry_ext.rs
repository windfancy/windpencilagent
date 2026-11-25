//! 把工具以组件形式注册进 Kernel Registry 的扩展。
//!
//! 展示了「一切皆插接」体系中工具如何作为一等组件注入 Registry。

use std::any::Any;
use std::sync::Arc;

use kern_core::{Component, ComponentKind, ComponentStatus, Registry, RegistryError};

use crate::tools::Tool;

/// 将工具包装为内核组件。
pub struct ToolComponent {
    tool: Arc<dyn Tool>,
}

impl ToolComponent {
    pub fn wrap(tool: Arc<dyn Tool>) -> Arc<Self> {
        Arc::new(Self { tool })
    }

    pub fn tool(&self) -> &Arc<dyn Tool> {
        &self.tool
    }
}

impl Component for ToolComponent {
    fn id(&self) -> &str {
        self.tool.name()
    }
    fn kind(&self) -> ComponentKind {
        ComponentKind::Tool
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> String {
        self.tool.description()
    }
    fn status(&self) -> ComponentStatus {
        ComponentStatus::Loaded
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Registry 扩展：注册一批工具并可按名解析。
pub trait ToolRegistryExt {
    fn register_tool(&self, tool: Arc<dyn Tool>) -> Result<(), RegistryError>;
    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>>;
    fn tools(&self) -> Vec<String>;
}

impl ToolRegistryExt for Registry {
    fn register_tool(&self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        self.register_typed(ToolComponent::wrap(tool))
    }

    fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let c = self.get(ComponentKind::Tool, name)?;
        let tc = c.as_any().downcast_ref::<ToolComponent>()?;
        Some(tc.tool().clone())
    }

    fn tools(&self) -> Vec<String> {
        self.list(ComponentKind::Tool)
            .into_iter()
            .map(|s| s.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test::Echo;
    use kern_core::model::{
        AgentId, ExecutionContext, RiskLevel, SessionId, TaskId,
    };
    use serde_json::json;

    #[tokio::test]
    async fn register_and_resolve_tool() {
        let reg = Registry::new();
        let tool: Arc<dyn Tool> = Arc::new(Echo);
        reg.register_tool(tool).unwrap();
        let t = reg.tool("util.echo").expect("tool resolved");
        assert_eq!(t.name(), "util.echo");
        assert_eq!(reg.tools(), vec!["util.echo".to_string()]);

        let ctx = ExecutionContext {
            scope: Vec::new(),
            session_id: SessionId::new(),
            agent_id: AgentId::new("test"),
            task_id: TaskId::new("t"),
            target: "127.0.0.1".into(),
            risk: RiskLevel::None,
            approved: true,
        };
        let r = t.call(&ctx, json!({"text": "hello"})).await;
        assert!(r.ok);
        assert_eq!(r.summary, "echo: hello");
    }
}
