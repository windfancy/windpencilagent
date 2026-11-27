//! 基础能力层 LLM 网关：供应商统一接口与 tool-calling 协议。
//!
//! 三个驱动：`openai`（覆盖所有 OpenAI 兼容端点）/ `anthropic` / `gemini`。
//! 选择哪一个是**协议**决定的，不是厂商决定的：
//! OpenAI 兼容端点（DeepSeek / 通义 / 豆包 / 智谱 / Kimi / Ollama / vLLM …）一律用 `openai`。

mod anthropic;
pub mod embed;
mod gemini;
mod profiles;
mod types;

pub use anthropic::{AnthropicProvider, ANTHROPIC_VERSION};
pub use gemini::GeminiProvider;
pub use profiles::{
    builtin_profiles, LlmProfile, LlmProfilesTool, LlmSettingsComponent, LlmTestTool,
    ProfileRegistry,
};
pub use types::{
    assistant_message, build_provider, canonical_driver, default_component, probe_provider,
    sanitize_tool_name, system_message, user_message, ChatMessage, ChatRole, Completion,
    CompletionUsage, DeltaAggregator, LlmComponent, LlmError, LlmProvider, LlmStreaming,
    OpenAiCompatibleProvider, ProviderConfig, ToolCall, ToolSchema, DRIVERS,
};

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    fn cfg(driver: &str) -> ProviderConfig {
        ProviderConfig {
            driver: driver.into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "m".into(),
            api_key: Some("k".into()),
            temperature: None,
            max_tokens: None,
            mode: "agent".into(),
            max_retries: 0,
        }
    }

    #[test]
    fn driver_names_normalize() {
        assert_eq!(canonical_driver("").unwrap(), "openai");
        assert_eq!(canonical_driver("openai-compatible").unwrap(), "openai");
        assert_eq!(canonical_driver("Claude").unwrap(), "anthropic");
        assert_eq!(canonical_driver("gemini").unwrap(), "gemini");
    }

    #[test]
    fn unknown_driver_fails_loudly() {
        let e = canonical_driver("antropic").unwrap_err();
        assert!(e.contains("未知 LLM 驱动"));
        assert!(e.contains("anthropic"), "报错要列出支持的驱动：{e}");
        assert!(build_provider(cfg("antropic")).is_err());
    }

    #[test]
    fn each_driver_builds() {
        for d in DRIVERS {
            let p = build_provider(cfg(d)).unwrap_or_else(|e| panic!("{d} 构造失败：{e}"));
            assert_eq!(p.model(), "m");
        }
    }
}
