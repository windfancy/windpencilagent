//! 目标状态机（R2-2，dsh goal 语义）：模型无法自报完成——完成声明一律转为
//! 「待用户背书」，由 HITL 审批通道背书后才真正 completed；连续受阻自动计数。

use serde::{Deserialize, Serialize};

use crate::store::{Store, StoreError};

/// 目标状态。
pub const GOAL_IN_PROGRESS: u8 = 0;
pub const GOAL_BLOCKED: u8 = 1;
pub const GOAL_AWAITING_ENDORSEMENT: u8 = 2;
pub const GOAL_COMPLETED: u8 = 3;

/// 目标记录（sled domain=`goal`，单键 `current`：一次会话一个顶层目标）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GoalState {
    /// 状态（GOAL_* 常量）
    pub status: u8,
    /// 目标一句话描述（创建/更新时可改写）
    pub objective: String,
    /// 最近备注（受阻原因 / 背书时间等）
    pub note: String,
    /// 连续自报受阻次数（非 blocked 状态调用清零）
    pub blocked_streak: u32,
    /// 最近更新毫秒时间戳
    pub updated_ms: u64,
    /// 背书来源（"user-approval" / 空=未背书）
    pub endorsed_by: String,
}

impl GoalState {
    pub fn status_name(&self) -> &'static str {
        match self.status {
            GOAL_IN_PROGRESS => "in_progress",
            GOAL_BLOCKED => "blocked",
            GOAL_AWAITING_ENDORSEMENT => "awaiting_endorsement",
            GOAL_COMPLETED => "completed",
            _ => "unknown",
        }
    }

    /// UI/注入摘要行。
    pub fn brief(&self) -> String {
        let st = self.status_name();
        match self.status {
            GOAL_BLOCKED => format!("目标状态：{st}（连续受阻 {} 次）· {}", self.blocked_streak, self.note),
            GOAL_AWAITING_ENDORSEMENT => format!("目标状态：{st}（模型自报完成，待用户背书）· {}", self.note),
            GOAL_COMPLETED => format!("目标状态：{st}（{}）", self.endorsed_by),
            _ => {
                if self.objective.is_empty() {
                    format!("目标状态：{st}")
                } else {
                    format!("目标状态：{st} · {}", self.objective)
                }
            }
        }
    }
}

/// 目标库句柄。
pub struct GoalStore {
    db: Store,
}

const KEY: &str = "current";

impl GoalStore {
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

    fn save(&self, g: &GoalState) -> Result<(), StoreError> {
        self.db.put("goal", KEY, g)
    }

    /// 当前目标（无记录返回缺省 in_progress）。
    pub fn get(&self) -> GoalState {
        self.db.get("goal", KEY).unwrap_or_default()
    }

    /// 模型报告状态。完成声明被拦截：一律转为 awaiting_endorsement（模型无法自报完成）。
    /// 连续第 3 次自报 blocked 时强制 blocked 并在返回提示中建议终止。
    /// 返回 (生效状态, 需要终止建议)。
    pub fn report(&self, status_req: &str, note: &str, objective: &str) -> Result<(u8, bool), StoreError> {
        let mut g = self.get();
        if !objective.is_empty() {
            g.objective = objective.trim().to_string();
        }
        g.note = note.trim().to_string();
        g.updated_ms = Self::now_ms();
        match status_req {
            "completed" | "done" | "完成" => {
                // 权威规则（dsh）：完成需人类背书，模型无法自报完成
                g.status = GOAL_AWAITING_ENDORSEMENT;
                g.note = format!(
                    "模型自报完成：{}（需用户背书后方可置为 completed）",
                    g.note
                );
                self.save(&g)?;
                Ok((g.status, false))
            }
            "blocked" | "受阻" => {
                g.blocked_streak += 1;
                g.status = GOAL_BLOCKED;
                let suggest_stop = g.blocked_streak >= 3;
                self.save(&g)?;
                Ok((g.status, suggest_stop))
            }
            _ => {
                // in_progress / 重启推进：清零受阻计数
                g.blocked_streak = 0;
                g.status = GOAL_IN_PROGRESS;
                self.save(&g)?;
                Ok((g.status, false))
            }
        }
    }

    /// 人类背书（仅 chat.rs 的 HITL 审批通过后调用）：awaiting_endorsement → completed。
    /// 返回是否成功背书（非待背书状态返回 false）。
    pub fn endorse(&self, by: &str) -> Result<bool, StoreError> {
        let mut g = self.get();
        if g.status != GOAL_AWAITING_ENDORSEMENT {
            return Ok(false);
        }
        g.status = GOAL_COMPLETED;
        g.endorsed_by = by.trim().to_string();
        g.updated_ms = Self::now_ms();
        self.save(&g)?;
        Ok(true)
    }

    /// 用户否决（审批拒绝）：回退 in_progress，备注记录否决。
    pub fn reject(&self, reason: &str) -> Result<(), StoreError> {
        let mut g = self.get();
        g.status = GOAL_IN_PROGRESS;
        g.note = format!("用户否决完成声明：{reason}");
        g.updated_ms = Self::now_ms();
        self.save(&g)
    }

    /// 重置（新目标/新会话）。
    pub fn reset(&self, objective: &str) -> Result<(), StoreError> {
        let g = GoalState {
            status: GOAL_IN_PROGRESS,
            objective: objective.trim().to_string(),
            note: String::new(),
            blocked_streak: 0,
            updated_ms: Self::now_ms(),
            endorsed_by: String::new(),
        };
        self.save(&g)
    }

    pub fn flush(&self) -> Result<(), StoreError> {
        self.db.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_requires_endorsement() {
        let s = GoalStore::open_tmp().unwrap();
        // 模型自报完成 → 待背书，非 completed
        let (st, _) = s.report("completed", "全部端口已扫", "摸清本机").unwrap();
        assert_eq!(st, GOAL_AWAITING_ENDORSEMENT);
        assert_eq!(s.get().status_name(), "awaiting_endorsement");
        // 背书 → completed
        assert!(s.endorse("user-approval").unwrap());
        assert_eq!(s.get().status_name(), "completed");
        assert_eq!(s.get().endorsed_by, "user-approval");
    }

    #[test]
    fn blocked_streak_suggests_stop_at_three() {
        let s = GoalStore::open_tmp().unwrap();
        let (_, a1) = s.report("blocked", "防火墙拦截", "").unwrap();
        let (_, a2) = s.report("blocked", "仍然拦截", "").unwrap();
        assert!(!a1 && !a2);
        let (_, a3) = s.report("blocked", "第三次受阻", "").unwrap();
        assert!(a3, "连续受阻 3 次应建议终止");
        assert_eq!(s.get().blocked_streak, 3);
        // 推进恢复清零
        s.report("in_progress", "换思路推进", "").unwrap();
        assert_eq!(s.get().blocked_streak, 0);
    }

    #[test]
    fn reject_falls_back_to_in_progress() {
        let s = GoalStore::open_tmp().unwrap();
        s.report("completed", "宣称完成", "").unwrap();
        s.reject("证据不足").unwrap();
        let g = s.get();
        assert_eq!(g.status, GOAL_IN_PROGRESS);
        assert!(g.note.contains("否决"));
        // 否决后背书无效
        assert!(!s.endorse("user-approval").unwrap());
    }
}
