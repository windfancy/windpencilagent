//! 事件总线：组件 / 插件 / UI 的观察通道（tokio broadcast）。

use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{SendError};

use crate::model::{SessionId, TaskId, TaskState};
use serde_json::Value;

/// 内核层向外部广播的事件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    SessionStarted,
    PlanCreated,
    PlanUpdated,
    TaskUpdated,
    TaskDone,
    ToolCalled,
    ResultObserved,
    ApprovalRequested,
    ApprovalResolved,
    PluginLoaded,
    SessionDone,
}

/// 事件总线上的通用载荷：结构化字段 + 附带 JSON。
#[derive(Debug, Clone)]
pub struct KernelEvent {
    pub kind: EventKind,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub state: Option<TaskState>,
    pub detail: Value,
}

impl KernelEvent {
    pub fn new(kind: EventKind, session_id: SessionId) -> Self {
        Self {
            kind,
            session_id,
            task_id: None,
            state: None,
            detail: Value::Null,
        }
    }

    pub fn with_task(mut self, task_id: TaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    pub fn with_state(mut self, state: TaskState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn with_detail(mut self, detail: impl Into<Value>) -> Self {
        self.detail = detail.into();
        self
    }
}

type Sender = broadcast::Sender<KernelEvent>;

/// 事件总线封装。内部为 channel 容量可配的 broadcast。
#[derive(Clone)]
pub struct EventBus {
    tx: Sender,
}

impl EventBus {
    /// 创建一个事件总线；`capacity` 决定广播通道缓冲的最近事件数。
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self { tx }
    }

    pub fn sender(&self) -> Sender {
        self.tx.clone()
    }

    /// 发布一个事件。滞后订阅者（lagged）按其已有最近事件继续。
    pub fn publish(&self, event: KernelEvent) -> Result<usize, SendError<KernelEvent>> {
        self.tx.send(event)
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<KernelEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn publish_and_receive() {
        let bus = EventBus::new(64);
        let mut rx = bus.subscribe();
        let ev = KernelEvent::new(EventKind::SessionStarted, SessionId::new())
            .with_detail(json!({"probe": true}));
        bus.publish(ev.clone()).unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.kind, ev.kind);
    }
}
