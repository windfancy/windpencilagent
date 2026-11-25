//! Orchestrator：拆单、角色调度、结果汇总。

use kern_core::model::{AgentId, Objective};
use tracing::info;

use crate::roles::{AgentRole, DomainAgent, RoleOutcome};

/// 编排器：按 侦察→分析→验证→汇总 的流水线调度角色智能体。
#[derive(Default)]
pub struct Orchestrator {
    agents: Vec<Box<dyn DomainAgent>>,
}

impl Orchestrator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_agent(&mut self, agent: Box<dyn DomainAgent>) {
        self.agents.push(agent);
    }

    /// 单角色运行（最简：调用指定角色，若未注册则返回空 ok 结果）。
    pub async fn run_role(
        &self,
        role: AgentRole,
        objective: &Objective,
    ) -> Option<RoleOutcome> {
        if let Some(agent) = self.agents.iter().find(|a| a.role() == role) {
            Some(agent.run(objective, None).await)
        } else {
            None
        }
    }

    /// 完整流水线：scanner → analyst → verifier → reporter（结果依次传递）。
    pub async fn pipeline(
        &self,
        objective: &Objective,
    ) -> Vec<RoleOutcome> {
        let roles = [
            AgentRole::Scanner,
            AgentRole::Analyst,
            AgentRole::Verifier,
            AgentRole::Reporter,
        ];
        let mut outcomes = Vec::new();
        let mut last: Option<RoleOutcome> = None;
        for role in roles {
            if let Some(agent) = self.agents.iter().find(|a| a.role() == role) {
                info!(role = %role, %objective.id, "pipeline step start");
                let out = agent.run(objective, last.as_ref()).await;
                last = Some(out.clone());
                outcomes.push(out);
            } else {
                info!(role = %role, "no agent registered, skip");
            }
        }
        outcomes
    }

    pub fn agent_ids(&self) -> Vec<AgentId> {
        self.agents.iter().map(|a| a.role().id()).collect()
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::LoggingAgent;

    #[tokio::test]
    async fn pipeline_runs() {
        let mut orch = Orchestrator::new();
        orch.add_agent(Box::new(LoggingAgent::new(AgentRole::Scanner, "scanner")));
        orch.add_agent(Box::new(LoggingAgent::new(AgentRole::Analyst, "analyst")));

        let obj = Objective {
            id: kern_core::model::SessionId::new(),
            description: "demo objective".into(),
            authorized_scope: vec!["10.0.0.0/8".into()],
            constraints: vec![],
            ref_plan: None,
        };
        let out = orch.pipeline(&obj).await;
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| o.ok));
        // 未注册的验证/报告角色被跳过
        assert!(orch.agent_ids().len() == 2);
    }
}
