//! 模型链装配（B-8）：主档 + fallback，失败按序降级。
//!
//! - 单档时直接用原生 provider（保住流式能力）；
//! - 多档时用 [`FallbackProvider`] 包装：`complete` 失败即换下一档，
//!   并在事件流里留痕（`tracing::warn`）。
//!
//! 能力字段（`supports_tools` / `context_window`）在装配期决定两件事：
//! 是否下发工具面、上下文预算基数。

use std::sync::Arc;

use async_trait::async_trait;

use base_llm::{
    ChatMessage, Completion, LlmError, LlmProfile, LlmProvider,
    ToolSchema,
};

/// 多档降级 provider：按顺序尝试，任一成功即返回。
pub struct FallbackProvider {
    chain: Vec<Arc<dyn LlmProvider>>,
}

impl FallbackProvider {
    /// 从档案链构建；任何一档初始化失败都跳过（不阻断其余档）。
    ///
    /// 返回 `(provider, warnings)`：全部失败时 provider 为 None。
    pub fn build(profiles: &[LlmProfile]) -> (Option<Arc<dyn LlmProvider>>, Vec<String>) {
        let mut chain: Vec<Arc<dyn LlmProvider>> = Vec::new();
        let mut warnings = Vec::new();
        for p in profiles {
            match base_llm::build_provider(p.to_provider_config()) {
                Ok(prov) => chain.push(Arc::from(prov)),
                Err(e) => warnings.push(format!("模型档案 {} 初始化失败（已跳过）：{e}", p.name)),
            }
        }
        match chain.len() {
            0 => (None, warnings),
            1 => {
                let only = chain.pop().expect("len==1");
                (Some(only), warnings)
            }
            _ => (
                Some(Arc::new(FallbackProvider { chain }) as Arc<dyn LlmProvider>),
                warnings,
            ),
        }
    }

    /// 链上模型名（审计/展示用）。
    pub fn model_names(&self) -> Vec<String> {
        self.chain.iter().map(|p| p.model().to_string()).collect()
    }
}

#[async_trait]
impl LlmProvider for FallbackProvider {
    fn provider_name(&self) -> &str {
        self.chain.first().map(|p| p.provider_name()).unwrap_or("fallback")
    }

    fn model(&self) -> &str {
        self.chain.first().map(|p| p.model()).unwrap_or("")
    }

    fn is_agent_mode(&self) -> bool {
        self.chain.first().map(|p| p.is_agent_mode()).unwrap_or(false)
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<Completion, LlmError> {
        let mut last: Option<LlmError> = None;
        for (i, p) in self.chain.iter().enumerate() {
            match p.complete(messages, tools).await {
                Ok(c) => {
                    if i > 0 {
                        tracing::warn!(model = %p.model(), "主档不可用，已降级到第 {} 档", i + 1);
                    }
                    return Ok(c);
                }
                Err(e) => {
                    tracing::warn!(model = %p.model(), error = %e, "模型档位调用失败，尝试下一档");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| LlmError::Config("模型链为空".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prof(name: &str, url: &str, model: &str) -> LlmProfile {
        LlmProfile {
            name: name.into(),
            driver: "openai-compatible".into(),
            base_url: url.into(),
            model: model.into(),
            api_key: None,
            temperature: None,
            max_tokens: None,
            enabled: true,
            mode: "agent".into(),
            context_window: None,
            supports_tools: None,
            supports_vision: None,
            cost_per_1k_in: None,
            cost_per_1k_out: None,
        }
    }

    #[test]
    fn single_profile_keeps_native_provider() {
        let (p, w) = FallbackProvider::build(&[prof("a", "http://127.0.0.1:1/v1", "m1")]);
        assert!(w.is_empty());
        let p = p.expect("应构造成功");
        assert_eq!(p.model(), "m1");
        assert_eq!(p.provider_name(), "openai-compatible");
    }

    #[test]
    fn multi_profile_wraps_into_chain() {
        let (p, w) = FallbackProvider::build(&[
            prof("a", "http://127.0.0.1:1/v1", "m1"),
            prof("b", "http://127.0.0.1:2/v1", "m2"),
        ]);
        assert!(w.is_empty());
        let p = p.expect("应构造成功");
        assert_eq!(p.model(), "m1", "对外报告主档");
        assert!(p.is_agent_mode());
    }

    #[test]
    fn invalid_profiles_are_skipped() {
        let (p, w) = FallbackProvider::build(&[prof("bad", "", "m0"), prof("ok", "http://x/v1", "m1")]);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bad"));
        assert_eq!(p.expect("仍有一档可用").model(), "m1");
    }

    #[test]
    fn empty_chain_yields_none() {
        let (p, _) = FallbackProvider::build(&[]);
        assert!(p.is_none());
    }
}
