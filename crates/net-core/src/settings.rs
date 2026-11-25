//! 网络全局参数（唯一来源：配置 `[net]`，由装配层安装）。
//!
//! 为什么用进程级单例而不是把配置透传进每个工具：`net.*` 工具由内核按
//! `Tool` 契约统一实例化（`net_core::tools::all()` 不接收参数），而工具调用期
//! 能拿到的只有 `ExecutionContext`——那里放业务身份，不适合塞网络开关。
//!
//! 后果与约束：
//! - 装一次即全局生效；改配置后由装配层重新 `install`（会话重建时发生）。
//! - 读出的是**快照**（clone），调用期内不会因并发写而变脸。
//! - 未安装时用 [`NetSettings::default`]，与"配置项全用默认值"完全等价。

use std::sync::{OnceLock, RwLock};

/// 网络参数快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSettings {
    /// 单请求超时（秒）。
    pub timeout_secs: u64,
    /// 连接超时（秒）。
    pub connect_timeout_secs: u64,
    /// 全局 User-Agent（空 = 内置默认）。
    pub user_agent: String,
    /// 代理地址（`http://` / `socks5://`；None = 直连）。
    pub proxy: Option<String>,
    /// 并发上限（扫描类工具的默认并发度）。
    pub concurrency: usize,
    /// 单域名每秒请求上限（0 = 不限）。
    pub per_host_qps: u32,
}

impl Default for NetSettings {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            connect_timeout_secs: 10,
            // 保留基座标识，便于对端从访问日志里认出这是谁在打
            user_agent: concat!("windpencilagent/", env!("CARGO_PKG_VERSION")).to_string(),
            proxy: None,
            concurrency: 4,
            per_host_qps: 0,
        }
    }
}

impl NetSettings {
    /// 实际生效的 UA（空字符串视为未配置）。
    pub fn effective_user_agent(&self) -> String {
        if self.user_agent.trim().is_empty() {
            NetSettings::default().user_agent
        } else {
            self.user_agent.trim().to_string()
        }
    }

    /// 实际生效的并发度（至少 1，且不超过扫描实现的硬上限）。
    pub fn effective_concurrency(&self) -> usize {
        self.concurrency.clamp(1, crate::scan::MAX_CONCURRENCY)
    }
}

fn cell() -> &'static RwLock<NetSettings> {
    static CUR: OnceLock<RwLock<NetSettings>> = OnceLock::new();
    CUR.get_or_init(|| RwLock::new(NetSettings::default()))
}

/// 安装网络参数（装配层调用；可重复调用以热更新）。
pub fn install(s: NetSettings) {
    match cell().write() {
        Ok(mut g) => *g = s,
        Err(p) => *p.into_inner() = s,
    }
}

/// 读取当前网络参数快照。
pub fn current() -> NetSettings {
    match cell().read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keeps_base_identity() {
        let d = NetSettings::default();
        assert!(d.effective_user_agent().starts_with("windpencilagent/"));
        assert_eq!(d.effective_concurrency(), 4);
    }

    #[test]
    fn blank_user_agent_falls_back() {
        let s = NetSettings { user_agent: "   ".into(), ..Default::default() };
        assert_eq!(s.effective_user_agent(), NetSettings::default().user_agent);
    }

    #[test]
    fn concurrency_is_clamped() {
        let s = NetSettings { concurrency: 0, ..Default::default() };
        assert_eq!(s.effective_concurrency(), 1);
        let s = NetSettings { concurrency: 99_999, ..Default::default() };
        assert_eq!(s.effective_concurrency(), crate::scan::MAX_CONCURRENCY);
    }

    #[test]
    fn install_then_read_roundtrips() {
        // 单例是进程级的：这条测试会改全局状态，末尾必须还原，
        // 否则同进程内其他用例（http_client 等）会读到脏值。
        let old = current();
        install(NetSettings {
            timeout_secs: 7,
            user_agent: "probe/1".into(),
            proxy: Some("http://127.0.0.1:7890".into()),
            ..Default::default()
        });
        let got = current();
        assert_eq!(got.timeout_secs, 7);
        assert_eq!(got.effective_user_agent(), "probe/1");
        assert_eq!(got.proxy.as_deref(), Some("http://127.0.0.1:7890"));
        install(old);
    }
}
