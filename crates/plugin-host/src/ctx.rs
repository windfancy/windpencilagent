//! 调用上下文：宿主能力实现从线程局部读"这次调用属于谁、能碰什么"。
//!
//! 为什么是 thread_local 而不是参数：跨 dll 的 `HostApi` 只有裸函数指针，
//! 没有 `self`，也没法传 Rust 结构体。调用发生在 `spawn_blocking` 的独立线程里，
//! 在该线程进入插件前设置、退出后清除，插件拿不到也改不了。

use std::cell::RefCell;
use std::path::PathBuf;

/// 一次插件调用的上下文。
#[derive(Debug, Clone, Default)]
pub struct CallContext {
    pub session_id: String,
    /// 授权范围（空 = 未声明，带目标一律拒绝）。
    pub scope: Vec<String>,
    /// 拒绝清单（「除这些之外」）。
    pub denylist: Vec<String>,
    /// 本次调用的插件 id（审计用）。
    pub plugin: String,
    /// 证据 / 原始结果的落盘根目录。
    pub data_dir: Option<PathBuf>,
}

thread_local! {
    static CURRENT: RefCell<Option<CallContext>> = const { RefCell::new(None) };
}

/// 在上下文内执行 `f`（嵌套调用会恢复外层上下文）。
pub fn with<R>(ctx: CallContext, f: impl FnOnce() -> R) -> R {
    let prev = CURRENT.with(|c| c.borrow_mut().replace(ctx));
    let out = f();
    CURRENT.with(|c| *c.borrow_mut() = prev);
    out
}

/// 取当前上下文（宿主能力实现里调用）。
pub fn current() -> Option<CallContext> {
    CURRENT.with(|c| c.borrow().clone())
}

/// 只取会话 id（高频路径，避免整份 clone）。
pub fn current_session() -> Option<String> {
    CURRENT.with(|c| c.borrow().as_ref().map(|x| x.session_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_is_scoped_and_nestable() {
        assert!(current().is_none());
        let a = CallContext {
            session_id: "s1".into(),
            plugin: "p1".into(),
            ..Default::default()
        };
        with(a, || {
            assert_eq!(current_session().as_deref(), Some("s1"));
            let b = CallContext {
                session_id: "s2".into(),
                plugin: "p2".into(),
                ..Default::default()
            };
            with(b, || {
                assert_eq!(current_session().as_deref(), Some("s2"));
            });
            assert_eq!(current_session().as_deref(), Some("s1"), "内层退出应恢复外层");
        });
        assert!(current().is_none(), "外层退出应清空");
    }
}
