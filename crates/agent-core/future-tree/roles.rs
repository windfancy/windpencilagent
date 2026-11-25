//! 业务域智能体角色：scanner / analyst / verifier / reporter。

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use kern_core::model::{AgentId, FactEntry, Objective};

/// 角色枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentRole {
    #[default]
    Orchestrator,
    Scanner,
    Analyst,
    Verifier,
    Reporter,
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl AgentRole {
    pub fn id(&self) -> AgentId {
        AgentId::new(format!("{:?}", self).to_ascii_lowercase())
    }
}

/// 角色一次产出的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleOutcome {
    pub role: AgentRole,
    pub ok: bool,
    pub summary: String,
    pub facts: Vec<FactEntry>,
}

/// 业务域智能体的统一行为接口。
#[async_trait]
pub trait DomainAgent: Send + Sync + 'static {
    fn role(&self) -> AgentRole;
    async fn run(&self, objective: &Objective, agenda: Option<&RoleOutcome>) -> RoleOutcome;
}

/// 简易日志型占位角色（骨架期用于链路验证）。
#[derive(Debug, Default)]
pub struct LoggingAgent {
    role: AgentRole,
    tag: String,
}

impl LoggingAgent {
    pub fn new(role: AgentRole, tag: impl Into<String>) -> Self {
        Self {
            role,
            tag: tag.into(),
        }
    }
}

#[async_trait]
impl DomainAgent for LoggingAgent {
    fn role(&self) -> AgentRole {
        self.role
    }

    async fn run(&self, objective: &Objective, agenda: Option<&RoleOutcome>) -> RoleOutcome {
        tracing::info!(
            role = %self.role,
            tag = %self.tag,
            objective = %objective.description,
            "domain agent executed"
        );
        let summary = if let Some(a) = agenda {
            format!("{} processed input from {:?}", self.tag, a.role)
        } else {
            format!("{} ran standalone", self.tag)
        };
        RoleOutcome {
            role: self.role,
            ok: true,
            summary,
            facts: vec![],
        }
    }
}
