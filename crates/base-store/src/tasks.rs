//! 任务持久化 v2：结构化任务清单（WorkBuddy 式状态机 + 依赖 + 认领，sled domain=`task`）。
//!
//! 状态机：pending(0) → in_progress(1) → completed(2)；deleted(3) 软删。
//! 看板四列（dsh 语义）：待规划 / 待办 / 进行中 / 已完成，由 §board 规则推导。

use serde::{Deserialize, Serialize};

use crate::store::{Store, StoreError};

pub const TASK_PENDING: i32 = 0;
pub const TASK_IN_PROGRESS: i32 = 1;
pub const TASK_COMPLETED: i32 = 2;
pub const TASK_DELETED: i32 = 3;

/// 看板列名（下标与 TaskRecord::board_col 返回值对应）。
pub const BOARD_COL_NAMES: [&str; 4] = ["待规划", "待办", "进行中", "已完成"];

/// 看板四列计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardCounts {
    /// 待规划：pending 且（未认领 或 依赖未完成）
    pub planned: usize,
    /// 待办：pending 且已认领 且 依赖全部完成
    pub todo: usize,
    /// 进行中
    pub doing: usize,
    /// 已完成
    pub done: usize,
}

/// 一条任务记录（v2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// 自增 id（启动时取 max+1）
    pub id: u64,
    /// 祈使句标题（旧数据从 title 迁移）
    #[serde(default)]
    pub subject: String,
    /// 详细要求 / 验收标准
    #[serde(default)]
    pub description: String,
    /// 进行时短语（in_progress 时 UI 展示）
    #[serde(default)]
    pub active_form: String,
    /// 0=pending 1=in_progress 2=completed 3=deleted
    pub status: i32,
    /// 认领者（agent/角色名），空=未认领
    #[serde(default)]
    pub owner: String,
    /// 依赖任务 id（全部 completed 才可开工/完结）
    #[serde(default)]
    pub blocked_by: Vec<u64>,
    /// 最新进展 / 结论摘要（旧数据从 detail 迁移）
    #[serde(default)]
    pub note: String,
    /// 执行轨迹明细（兼容保留）
    #[serde(default)]
    pub steps: Vec<String>,
    /// 创建毫秒时间戳（旧数据从 time_ms 迁移）
    #[serde(default)]
    pub created_ms: u64,
    /// 最近更新毫秒时间戳
    #[serde(default)]
    pub updated_ms: u64,
}

impl TaskRecord {
    /// 从 v1 记录（title/status/detail/steps/time_ms）构造 v2 记录。
    /// v1 状态：0 运行中 1 完成 2 失败；v2 无"失败"，2 → pending 并标注需重做。
    pub fn from_v1(
        id: u64,
        title: &str,
        status: i32,
        detail: &str,
        steps: Vec<String>,
        time_ms: u64,
    ) -> Self {
        let mut note = detail.to_string();
        let st = match status {
            1 => TASK_COMPLETED,
            2 => {
                if !note.is_empty() {
                    note.push(' ');
                }
                note.push_str("（旧失败记录，需重做）");
                TASK_PENDING
            }
            _ => TASK_PENDING,
        };
        Self {
            id,
            subject: title.to_string(),
            description: String::new(),
            active_form: String::new(),
            status: st,
            owner: String::new(),
            blocked_by: vec![],
            note,
            steps,
            created_ms: time_ms,
            updated_ms: time_ms,
        }
    }

    /// 看板列归属（0=待规划 1=待办 2=进行中 3=已完成；deleted 不属于任何列）。
    pub fn board_col(&self, all: &[(u64, i32)]) -> Option<u8> {
        match self.status {
            TASK_IN_PROGRESS => Some(2),
            TASK_COMPLETED => Some(3),
            TASK_PENDING => {
                let has_owner = !self.owner.is_empty();
                let blocked = self
                    .blocked_by
                    .iter()
                    .any(|id| all.iter().any(|(x, s)| x == id && *s != TASK_COMPLETED));
                Some(if has_owner && !blocked { 1 } else { 0 })
            }
            _ => None,
        }
    }
}

/// 任务库句柄。
pub struct TaskStore {
    db: Store,
}

impl TaskStore {
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        Ok(Self { db: Store::open(dir)? })
    }

    pub fn open_tmp() -> Result<Self, StoreError> {
        Ok(Self { db: Store::open_tmp()? })
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn key(id: u64) -> String {
        format!("{id:012}")
    }

    /// 解码一条记录；v1 旧格式（含 title 无 subject）自动迁移到 v2。
    fn decode(v: serde_json::Value) -> Option<TaskRecord> {
        let obj = v.as_object()?;
        if obj.contains_key("title") && !obj.contains_key("subject") {
            // v1：title/status/detail/steps/time_ms
            let id = obj.get("id")?.as_u64()?;
            let title = obj.get("title").and_then(serde_json::Value::as_str).unwrap_or("");
            let status = obj.get("status").and_then(serde_json::Value::as_i64).unwrap_or(0) as i32;
            let detail = obj.get("detail").and_then(serde_json::Value::as_str).unwrap_or("");
            let steps: Vec<String> = obj
                .get("steps")
                .and_then(serde_json::Value::as_array)
                .map(|a| a.iter().filter_map(serde_json::Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            let time_ms = obj.get("time_ms").and_then(serde_json::Value::as_u64).unwrap_or(0);
            return Some(TaskRecord::from_v1(id, title, status, detail, steps, time_ms));
        }
        serde_json::from_value(v).ok()
    }

    /// 创建任务，返回分配的 id。
    pub fn create(
        &self,
        subject: &str,
        description: &str,
        active_form: &str,
        owner: &str,
        blocked_by: Vec<u64>,
    ) -> Result<u64, StoreError> {
        let id = self.next_id();
        let now = Self::now_ms();
        let rec = TaskRecord {
            id,
            subject: subject.to_string(),
            description: description.to_string(),
            active_form: active_form.to_string(),
            status: TASK_PENDING,
            owner: owner.to_string(),
            blocked_by,
            note: String::new(),
            steps: vec![],
            created_ms: now,
            updated_ms: now,
        };
        self.db.put("task", &Self::key(id), &rec)?;
        Ok(id)
    }

    /// 更新任务（updated_ms 自动刷新）。
    pub fn update(&self, rec: &mut TaskRecord) -> Result<(), StoreError> {
        rec.updated_ms = Self::now_ms();
        self.db.put("task", &Self::key(rec.id), rec)
    }

    /// 单条任务。
    pub fn get(&self, id: u64) -> Option<TaskRecord> {
        let v: serde_json::Value = self.db.get("task", &Self::key(id)).ok()?;
        Self::decode(v)
    }

    /// 全量任务（按 id 升序 = 创建顺序；含 deleted 与旧格式自动迁移）。
    pub fn all(&self) -> Vec<TaskRecord> {
        let mut list: Vec<TaskRecord> = self
            .db
            .keys("task")
            .into_iter()
            .filter_map(|k| {
                let v: Option<serde_json::Value> = self.db.get("task", &k).ok();
                v.and_then(Self::decode)
            })
            .collect();
        list.sort_by_key(|r| r.id);
        list
    }

    /// 可见任务（排除软删）。
    pub fn visible(&self) -> Vec<TaskRecord> {
        self.all().into_iter().filter(|r| r.status != TASK_DELETED).collect()
    }

    /// blocked_by 中尚未 completed 的依赖 id。
    pub fn blocking_unfinished(&self, rec: &TaskRecord) -> Vec<u64> {
        let all: Vec<(u64, i32)> = self.all().iter().map(|r| (r.id, r.status)).collect();
        rec.blocked_by
            .iter()
            .copied()
            .filter(|id| all.iter().any(|(x, s)| x == id && *s != TASK_COMPLETED))
            .collect()
    }

    /// 软删（status=deleted）。
    pub fn delete(&self, id: u64) -> Result<bool, StoreError> {
        match self.get(id) {
            Some(mut rec) if rec.status != TASK_DELETED => {
                rec.status = TASK_DELETED;
                self.update(&mut rec)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// 模型 todo 清单（owner=="agent" 的可见任务，按 id 升序）。
    pub fn agent_todos(&self) -> Vec<TaskRecord> {
        self.visible().into_iter().filter(|r| r.owner == "agent").collect()
    }

    /// 模型 todo 整表快照（R2-1，dsh TodoWrite 语义）：
    /// 软删全部 owner=="agent" 的任务后按快照重建，用户任务不受影响。
    /// items = (标题, status)；status 0=pending 1=in_progress 2=completed。
    /// 返回写入条数。
    pub fn replace_agent_todos(&self, items: &[(String, i32)]) -> Result<usize, StoreError> {
        for rec in self.visible() {
            if rec.owner == "agent" {
                self.delete(rec.id)?;
            }
        }
        for (subject, st) in items {
            let id = self.next_id();
            let now = Self::now_ms();
            let rec = TaskRecord {
                id,
                subject: subject.trim().to_string(),
                description: String::new(),
                active_form: String::new(),
                status: *st,
                owner: "agent".into(),
                blocked_by: vec![],
                note: String::new(),
                steps: vec![],
                created_ms: now,
                updated_ms: now,
            };
            self.db.put("task", &Self::key(id), &rec)?;
        }
        Ok(items.len())
    }

    /// 看板四列计数。
    pub fn board(&self) -> BoardCounts {
        let all = self.visible();
        let pairs: Vec<(u64, i32)> = all.iter().map(|r| (r.id, r.status)).collect();
        let mut c = BoardCounts::default();
        for rec in &all {
            match rec.board_col(&pairs) {
                Some(0) => c.planned += 1,
                Some(1) => c.todo += 1,
                Some(2) => c.doing += 1,
                Some(3) => c.done += 1,
                _ => {}
            }
        }
        c
    }

    /// 物理清除已完成与软删任务，返回清除条数。
    pub fn clear_finished(&self) -> Result<usize, StoreError> {
        let mut n = 0;
        for k in self.db.keys("task") {
            if let Ok(rec) = self.db.get::<TaskRecord>("task", &k) {
                if rec.status == TASK_COMPLETED || rec.status == TASK_DELETED {
                    self.db.remove("task", &k)?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// 清空全部任务。
    pub fn clear_all(&self) -> Result<usize, StoreError> {
        let n = self.db.keys("task").len();
        for k in self.db.keys("task") {
            self.db.remove("task", &k)?;
        }
        Ok(n)
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        self.db.flush()
    }

    fn next_id(&self) -> u64 {
        self.all().last().map(|r| r.id + 1).unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_todo_snapshot_replaces_only_agent_rows() {
        let store = TaskStore::open_tmp().unwrap();
        // 用户任务（owner 空）与 agent 旧清单并存
        store.create("用户手建任务", "", "", "", vec![]).unwrap();
        store
            .replace_agent_todos(&[
                ("侦察".into(), TASK_PENDING),
                ("扫描".into(), TASK_IN_PROGRESS),
                ("报告".into(), TASK_PENDING),
            ])
            .unwrap();
        assert_eq!(store.agent_todos().len(), 3);
        assert!(store.visible().iter().any(|r| r.subject == "用户手建任务"), "用户任务保留");
        // 第二次快照：旧 agent 清单软删重建
        store.replace_agent_todos(&[("新计划".into(), TASK_IN_PROGRESS)]).unwrap();
        let todos = store.agent_todos();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].subject, "新计划");
        assert_eq!(todos[0].status, TASK_IN_PROGRESS);
        // 用户任务仍不受影响
        assert!(store.visible().iter().any(|r| r.subject == "用户手建任务"));
    }

    #[test]
    fn create_state_machine_and_board() {
        let store = TaskStore::open_tmp().unwrap();
        let a = store.create("扫描目标段", "10.0.0.0/24 全端口", "正在扫描目标段", "", vec![]).unwrap();
        let b = store.create("汇总报告", "按严重度排序", "正在汇总报告", "recon", vec![a]).unwrap();

        // 初始：a 待规划（未认领），b 待规划（依赖 a 未完成）
        let bd = store.board();
        assert_eq!((bd.planned, bd.todo, bd.doing, bd.done), (2, 0, 0, 0));

        // a 认领 → 待办
        let mut ra = store.get(a).unwrap();
        ra.owner = "recon".into();
        let _ = store.update(&mut ra);
        let bd = store.board();
        assert_eq!((bd.planned, bd.todo, bd.doing, bd.done), (1, 1, 0, 0));

        // a 进行中
        ra.status = TASK_IN_PROGRESS;
        ra.active_form = "正在扫描目标段".into();
        let _ = store.update(&mut ra);
        let bd = store.board();
        assert_eq!(bd.doing, 1);
        assert_eq!(store.blocking_unfinished(&store.get(b).unwrap()), vec![a]);

        // b 未完成依赖时不可标 completed（工具层门控，store 层提供 blocking 列表）
        // a 完成 → b 解除阻塞 → 待办
        ra.status = TASK_COMPLETED;
        let _ = store.update(&mut ra);
        assert!(store.blocking_unfinished(&store.get(b).unwrap()).is_empty());
        let bd = store.board();
        assert_eq!((bd.planned, bd.todo, bd.doing, bd.done), (0, 1, 0, 1));
        assert_eq!(store.get(b).unwrap().board_col(
            &store.all().iter().map(|r| (r.id, r.status)).collect::<Vec<_>>()
        ), Some(1));
    }

    #[test]
    fn soft_delete_and_purge() {
        let store = TaskStore::open_tmp().unwrap();
        let a = store.create("x", "", "", "", vec![]).unwrap();
        let b = store.create("y", "", "", "", vec![]).unwrap();
        assert!(store.delete(a).unwrap());
        assert!(!store.delete(a).unwrap(), "重复软删返回 false");
        assert_eq!(store.visible().len(), 1);
        assert_eq!(store.visible()[0].id, b);
        // 依赖软删任务视为未完成（保守）
        let c = store.create("z", "", "", "", vec![a]).unwrap();
        assert_eq!(store.blocking_unfinished(&store.get(c).unwrap()), vec![a]);
        assert_eq!(store.clear_finished().unwrap(), 1, "软删任务被物理清除，pending 保留");
        assert_eq!(store.all().len(), 2);
        store.get(b).unwrap();
        let mut rb = store.get(b).unwrap();
        rb.status = TASK_COMPLETED;
        let _ = store.update(&mut rb);
        assert_eq!(store.clear_finished().unwrap(), 1, "completed 物理清除");
        assert_eq!(store.visible().len(), 1);
    }

    #[test]
    fn v1_record_compat_migration() {
        // v1 JSON：title/status/detail/steps/time_ms，无任何 v2 字段 → decode 自动迁移
        let v1 = serde_json::json!({
            "id": 7, "title": "扫描 127.0.0.1", "status": 1, "detail": "完成",
            "steps": ["第 1 轮推理"], "time_ms": 1700000000000u64
        });
        let rec = TaskStore::decode(v1).unwrap();
        assert_eq!(rec.subject, "扫描 127.0.0.1");
        assert_eq!(rec.note, "完成");
        assert_eq!(rec.status, TASK_COMPLETED);
        assert_eq!(rec.created_ms, 1700000000000);
        assert_eq!(rec.steps.len(), 1);

        // v1 失败记录 → pending + 标注
        let v1f = serde_json::json!({"id": 8, "title": "旧失败", "status": 2, "detail": "超时", "steps": [], "time_ms": 1});
        let rec2 = TaskStore::decode(v1f).unwrap();
        assert_eq!(rec2.status, TASK_PENDING);
        assert!(rec2.note.contains("需重做"));

        // 落盘持久化的 v1 记录经 TaskStore 读出自动迁移
        let dir = std::env::temp_dir().join(format!("wpa_task_v1_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = TaskStore::open(&dir).unwrap();
        store.db.put("task", &TaskStore::key(9), &serde_json::json!({
            "id": 9, "title": "旧运行中", "status": 0, "detail": "", "steps": [], "time_ms": 5u64
        })).unwrap();
        let all = store.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].subject, "旧运行中");
        assert_eq!(all[0].status, TASK_PENDING);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_roundtrip_next_id() {
        let dir = std::env::temp_dir().join(format!("wpa_task_v2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = TaskStore::open(&dir).unwrap();
        let a = store.create("a", "", "", "", vec![]).unwrap();
        let b = store.create("b", "", "", "", vec![]).unwrap();
        store.flush().unwrap();
        drop(store);
        let store2 = TaskStore::open(&dir).unwrap();
        assert_eq!(store2.next_id(), b + 1);
        assert_eq!(store2.get(a).unwrap().subject, "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_refreshes_updated_ms() {
        let store = TaskStore::open_tmp().unwrap();
        let a = store.create("t", "", "", "", vec![]).unwrap();
        let mut rec = store.get(a).unwrap();
        rec.updated_ms = 1;
        store.update(&mut rec).unwrap();
        assert!(store.get(a).unwrap().updated_ms > 1);
    }
}
