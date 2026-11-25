//! HITL 审批中枢：执行器与 UI 通道之间的唯一裁决通道。
//!
//! - 执行侧：`request()` 挂起等待人工裁决（带超时）；
//! - 界面侧：`pending()` 拉取待审批清单，`resolve()` 提交裁决；
//! - 双侧通过事件总线获知动态，但裁决本身只走本中枢（强一致）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::model::{ApprovalDecision, ApprovalRequest};

/// 中枢内的待审批条目（供 UI 展示）。
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub key: String,
    pub request: ApprovalRequest,
}

struct PendingSlot {
    request: ApprovalRequest,
    tx: Option<oneshot::Sender<ApprovalDecision>>,
}

/// 审批中枢。线程安全；key = `{task_id}:{tool_name}`。
pub struct ApprovalHub {
    pending: Mutex<HashMap<String, PendingSlot>>,
}

impl Default for ApprovalHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalHub {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn key_of(task_id: &str, tool: &str) -> String {
        format!("{task_id}:{tool}")
    }

    /// 提交一次审批请求并等待裁决。
    ///
    /// 超时返回 `ApprovalDecision::Timeout`；请求撤除。
    pub async fn request(
        &self,
        req: ApprovalRequest,
        timeout: Duration,
    ) -> ApprovalDecision {
        let key = Self::key_of(req.task_id.as_ref(), &req.tool_name);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            key.clone(),
            PendingSlot {
                request: req,
                tx: Some(tx),
            },
        );

        let decision = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(d)) => d,
            Ok(Err(_)) | Err(_) => ApprovalDecision::Timeout,
        };

        // 无论结果如何，撤除挂起项
        if let Some(slot) = self.pending.lock().unwrap().remove(&key) {
            if let Some(tx) = slot.tx {
                // 裁决来自 resolve 时 tx 已消费；Timeout 时主动关闭
                let _ = tx;
            }
        }
        decision
    }

    /// 提交裁决。返回是否成功送达（挂起项存在且未被超时清走）。
    pub fn resolve(&self, task_id: &str, tool: &str, decision: ApprovalDecision) -> bool {
        let key = Self::key_of(task_id, tool);
        let slot = self.pending.lock().unwrap().remove(&key);
        match slot {
            Some(mut s) => match s.tx.take() {
                Some(tx) => tx.send(decision).is_ok(),
                None => false,
            },
            None => false,
        }
    }

    /// 当前挂起的审批清单。
    pub fn pending(&self) -> Vec<PendingApproval> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(key, slot)| PendingApproval {
                key: key.clone(),
                request: slot.request.clone(),
            })
            .collect()
    }

    /// 是否存在指定任务的挂起审批。
    pub fn is_pending(&self, task_id: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .keys()
            .any(|k| k.starts_with(&format!("{task_id}:")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RiskLevel, TaskId};

    fn req(task: &str) -> ApprovalRequest {
        ApprovalRequest {
            task_id: TaskId::new(task),
            tool_name: "scan.port".into(),
            target: "10.26.0.3".into(),
            risk: RiskLevel::High,
            args_summary: "{}".into(),
            reason: "probe".into(),
        }
    }

    #[tokio::test]
    async fn approve_roundtrip() {
        let hub = std::sync::Arc::new(ApprovalHub::new());
        let r = req("t02");
        let h = hub.clone();
        let waiter = tokio::spawn(async move { h.request(r, Duration::from_secs(5)).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.pending().len(), 1);
        assert!(hub.resolve("t02", "scan.port", ApprovalDecision::Approved));
        let d = waiter.await.unwrap();
        assert_eq!(d, ApprovalDecision::Approved);
        assert!(hub.pending().is_empty());
    }

    #[tokio::test]
    async fn timeout_path() {
        let hub = ApprovalHub::new();
        let d = hub.request(req("t03"), Duration::from_millis(80)).await;
        assert_eq!(d, ApprovalDecision::Timeout);
        assert!(hub.pending().is_empty());
    }
}
