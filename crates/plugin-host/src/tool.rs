//! `PluginTool`：把 dll 包装成基座认识的 [`ApiTool`]。
//!
//! 这一层是"插件体系"与"agent-core 主链路"之间唯一的缝：
//! - 上层看到的仍是普通工具（有 schema、有风险档、走审批）
//! - 下层是同步 C ABI，调用被丢进阻塞线程池并有超时

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use wpa_api::{ApiTool, RiskTier, ToolReply, ToolRequest};

use crate::ctx::{self, CallContext};
use crate::load::LoadedPlugin;
use crate::manifest::{DllManifest, PluginManifest};

/// 默认单次调用超时（插件内部应自己更短，这里是兜底）。
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// 插件工具。
pub struct PluginTool {
    pub pnc: PluginManifest,
    pub dll: DllManifest,
    pub loaded: Arc<LoadedPlugin>,
    pub risk: RiskTier,
    pub timeout_ms: u64,
}

impl std::fmt::Debug for PluginTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTool")
            .field("id", &self.dll.id)
            .field("risk", &self.risk)
            .finish()
    }
}

/// 字符串 → 风险档（未知按 High：拼错的风险名不能当 Low 放行）。
pub fn parse_risk(s: &str) -> RiskTier {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => RiskTier::None,
        "low" => RiskTier::Low,
        "medium" | "mid" => RiskTier::Medium,
        "high" => RiskTier::High,
        _ => {
            if s.trim().is_empty() {
                RiskTier::Low
            } else {
                RiskTier::High
            }
        }
    }
}

/// 取"更高"的一档（manifest 声明 Low 而组默认 High 时按 High）。
pub fn max_risk(a: RiskTier, b: RiskTier) -> RiskTier {
    let rank = |r: RiskTier| match r {
        RiskTier::None => 0,
        RiskTier::Low => 1,
        RiskTier::Medium => 2,
        RiskTier::High => 3,
        RiskTier::Critical => 4,
    };
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

impl PluginTool {
    pub fn new(
        pnc: PluginManifest,
        loaded: Arc<LoadedPlugin>,
        group_default_risk: Option<&str>,
        timeout_ms: u64,
    ) -> Self {
        let declared = parse_risk(&loaded.manifest.risk);
        let risk = match group_default_risk {
            Some(g) => max_risk(declared, parse_risk(g)),
            None => declared,
        };
        Self {
            dll: loaded.manifest.clone(),
            pnc,
            loaded,
            risk,
            timeout_ms,
        }
    }

    fn to_reply(&self, v: &Value, elapsed: u64) -> ToolReply {
        let summary = v
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("插件 `{}` 执行完成", self.dll.id));
        // 摘要不能无限长：模型上下文有限，细节走 detail / 句柄
        let summary = truncate(&summary, 1200);
        match v.get("detail").filter(|d| !d.is_null()) {
            Some(d) => ToolReply::json(summary, d.clone(), elapsed),
            None => ToolReply::ok(summary, elapsed),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("…（已截断）");
    out
}

#[async_trait]
impl ApiTool for PluginTool {
    fn name(&self) -> &str {
        &self.dll.id
    }

    fn description(&self) -> String {
        if self.dll.summary.is_empty() {
            self.pnc.summary.clone()
        } else {
            self.dll.summary.clone()
        }
    }

    fn input_schema(&self) -> Value {
        self.dll.schema.clone()
    }

    fn risk(&self) -> RiskTier {
        self.risk
    }

    async fn call(&self, req: ToolRequest) -> ToolReply {
        let start = Instant::now();
        let args_json = match serde_json::to_string(&req.args) {
            Ok(s) => s,
            Err(e) => return ToolReply::err(format!("参数序列化失败：{e}"), 0),
        };
        let loaded = self.loaded.clone();
        let plugin_id = self.dll.id.clone();
        let call_ctx = CallContext {
            session_id: req.session_id.clone(),
            scope: req.authorized_scope.clone(),
            denylist: req.denylist.clone(),
            plugin: plugin_id.clone(),
            data_dir: req.runtime.data_dir(),
        };

        let timeout = Duration::from_millis(self.timeout_ms.max(1));
        // 插件是同步函数：丢进阻塞池，不占 async 执行器
        let join = tokio::task::spawn_blocking(move || {
            ctx::with(call_ctx, || loaded.invoke(&args_json))
        });

        let result = tokio::time::timeout(timeout, join).await;
        let elapsed = start.elapsed().as_millis() as u64;

        match result {
            // 超时：线程无法强杀（ABI 限制），记一次失败并放弃本次结果
            Err(_) => {
                self.loaded.record_failure("调用超时");
                ToolReply::err(
                    format!(
                        "插件 `{plugin_id}` 超时（{}ms）已放弃；该插件已记一次失败",
                        self.timeout_ms
                    ),
                    elapsed,
                )
            }
            Ok(Err(join_err)) => {
                self.loaded
                    .record_failure(&format!("任务异常：{join_err}"));
                ToolReply::err(format!("插件 `{plugin_id}` 任务异常：{join_err}"), elapsed)
            }
            Ok(Ok(Err(e))) => ToolReply::err(e, elapsed),
            Ok(Ok(Ok(v))) => self.to_reply(&v, elapsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_parsing_is_fail_closed() {
        assert_eq!(parse_risk("low"), RiskTier::Low);
        assert_eq!(parse_risk("HIGH"), RiskTier::High);
        assert_eq!(parse_risk("critical"), RiskTier::High, "critical 不支持时按 High");
        assert_eq!(parse_risk(""), RiskTier::Low, "缺省不声明视为 Low");
        assert_eq!(parse_risk("loww"), RiskTier::High, "拼错必须按 High");
    }

    #[test]
    fn risk_max_wins() {
        assert_eq!(max_risk(RiskTier::Low, RiskTier::High), RiskTier::High);
        assert_eq!(max_risk(RiskTier::High, RiskTier::Low), RiskTier::High);
        assert_eq!(max_risk(RiskTier::None, RiskTier::Medium), RiskTier::Medium);
    }

    #[test]
    fn truncate_keeps_utf8() {
        let s = "中".repeat(10);
        let t = truncate(&s, 3);
        assert!(t.starts_with("中中中"));
        assert!(t.ends_with("已截断）"));
    }
}
