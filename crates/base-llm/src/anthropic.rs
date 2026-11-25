//! Anthropic Messages API 原生实现（Claude 系列）。
//!
//! 为什么不走 OpenAI 兼容层：Anthropic 的 tool-calling 结构完全不同——
//! `tools` 用 `input_schema` 而非 `parameters`，工具调用是 `content` 里的
//! `tool_use` 块，工具结果必须作为 **user 消息里的 `tool_result` 块**回填，
//! 且 `system` 是顶层字段而不是一条 message。用兼容层翻译会在多轮工具链上崩。
//!
//! 协议要点（实测）：
//! - 端点 `{base}/messages`；鉴权 `x-api-key` + `anthropic-version`
//! - `max_tokens` 必填
//! - 连续多个 `tool_result` 必须合并在**同一条** user 消息里
//!
//! 本文件只做协议转换，不做重试策略以外的任何决策。

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::types::{
    sanitize_tool_name, ChatMessage, ChatRole, Completion, CompletionUsage, LlmError, LlmProvider,
    ProviderConfig, ToolCall, ToolSchema,
};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Claude 原生 provider。
pub struct AnthropicProvider {
    cfg: ProviderConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(cfg: ProviderConfig) -> Result<Self, LlmError> {
        if cfg.base_url.is_empty() || cfg.model.is_empty() {
            return Err(LlmError::Config("base_url/model 必填".into()));
        }
        if cfg.api_key.as_deref().unwrap_or("").trim().is_empty() {
            return Err(LlmError::Config("anthropic 驱动需要 api_key".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|e| LlmError::Config(e.to_string()))?;
        Ok(Self { cfg, client })
    }

    fn messages_url(&self) -> String {
        let base = self.cfg.base_url.trim_end_matches('/');
        // 允许用户把 base_url 写到 /v1，也允许写到域名
        if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    /// 把内部消息序列翻译成 Anthropic 结构。
    ///
    /// 返回 `(body, 清洗名→原始名)`。`system` 被抽成顶层字段，
    /// 连续的 tool 消息合并为同一条 user 消息（协议硬要求）。
    pub fn build_body(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> (Value, HashMap<String, String>) {
        let tools_slice: &[ToolSchema] = if self.cfg.mode == "chat" { &[] } else { tools };
        let name_to_orig: HashMap<String, String> = tools_slice
            .iter()
            .map(|t| (sanitize_tool_name(&t.name), t.name.clone()))
            .collect();

        let mut system_parts: Vec<String> = Vec::new();
        let mut out: Vec<Value> = Vec::new();
        for m in messages {
            match m.role {
                ChatRole::System => {
                    if !m.content.trim().is_empty() {
                        system_parts.push(m.content.clone());
                    }
                }
                ChatRole::User => out.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": m.content }],
                })),
                ChatRole::Assistant => {
                    let mut blocks: Vec<Value> = Vec::new();
                    if !m.content.trim().is_empty() {
                        blocks.push(json!({ "type": "text", "text": m.content }));
                    }
                    for tc in m.tool_calls.iter().flatten() {
                        let input: Value = serde_json::from_str(&tc.arguments)
                            .unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": sanitize_tool_name(&tc.name),
                            "input": input,
                        }));
                    }
                    if !blocks.is_empty() {
                        out.push(json!({ "role": "assistant", "content": blocks }));
                    }
                }
                ChatRole::Tool => {
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                        "content": m.content,
                    });
                    // 上一条也是 user 且只含 tool_result ⇒ 追加（多条结果必须合并）
                    let appendable = out
                        .last()
                        .map(|last| {
                            last["role"] == "user"
                                && last["content"]
                                    .as_array()
                                    .map(|a| {
                                        !a.is_empty()
                                            && a.iter().all(|b| b["type"] == "tool_result")
                                    })
                                    .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if appendable {
                        if let Some(arr) = out.last_mut().and_then(|l| l["content"].as_array_mut()) {
                            arr.push(block);
                        }
                    } else {
                        out.push(json!({ "role": "user", "content": [block] }));
                    }
                }
            }
        }

        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens.unwrap_or(4096),
            "messages": out,
        });
        if !system_parts.is_empty() {
            body["system"] = json!(system_parts.join("\n\n"));
        }
        if let Some(t) = self.cfg.temperature {
            body["temperature"] = json!(t);
        }
        if !tools_slice.is_empty() {
            let tools_json: Vec<Value> = tools_slice
                .iter()
                .map(|t| {
                    json!({
                        "name": sanitize_tool_name(&t.name),
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools_json);
        }
        (body, name_to_orig)
    }

    /// 解析响应。纯函数，便于离线测试。
    pub fn parse_response(
        &self,
        v: &Value,
        name_to_orig: &HashMap<String, String>,
    ) -> Result<Completion, LlmError> {
        let blocks = v
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::InvalidResponse(format!("缺少 content：{}", brief(v))))?;
        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
                "tool_use" => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let orig = name_to_orig.get(&name).cloned().unwrap_or(name);
                    let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(ToolCall {
                        id: b
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("call_0")
                            .to_string(),
                        name: orig,
                        arguments: args.to_string(),
                    });
                }
                _ => {}
            }
        }
        let usage = v.get("usage");
        let usage = CompletionUsage {
            prompt_tokens: usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            completion_tokens: usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        };
        Ok(Completion {
            content: if text.trim().is_empty() { None } else { Some(text) },
            tool_calls,
            usage,
        })
    }
}

fn brief(v: &Value) -> String {
    let s = v.to_string();
    s.chars().take(300).collect()
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_name(&self) -> &str {
        &self.cfg.driver
    }

    fn model(&self) -> &str {
        &self.cfg.model
    }

    fn is_agent_mode(&self) -> bool {
        self.cfg.mode != "chat"
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<Completion, LlmError> {
        let (body, name_to_orig) = self.build_body(messages, tools);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let resp = self
                .client
                .post(self.messages_url())
                .header("x-api-key", self.cfg.api_key.clone().unwrap_or_default())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;
            let err = match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let text = r.text().await.unwrap_or_default();
                    if (200..300).contains(&status) {
                        let v: Value = serde_json::from_str(&text).map_err(|e| {
                            LlmError::InvalidResponse(format!("响应非 JSON：{e}；{text}"))
                        })?;
                        return self.parse_response(&v, &name_to_orig);
                    }
                    LlmError::Api { status, body: text }
                }
                Err(e) => {
                    if e.is_timeout() {
                        LlmError::Timeout(e.to_string())
                    } else {
                        LlmError::Http(e.to_string())
                    }
                }
            };
            if err.is_retryable() && attempt <= self.cfg.max_retries {
                let backoff = std::time::Duration::from_millis(1000u64 << (attempt - 1).min(3));
                tracing::warn!(attempt, error = %err, "anthropic 请求失败，退避重试");
                tokio::time::sleep(backoff).await;
                continue;
            }
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{assistant_message, system_message, user_message};

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            driver: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4".into(),
            api_key: Some("sk-ant-test".into()),
            temperature: Some(0.2),
            max_tokens: Some(1024),
            mode: "agent".into(),
            max_retries: 0,
        }
    }

    fn p() -> AnthropicProvider {
        AnthropicProvider::new(cfg()).unwrap()
    }

    fn schema() -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "file.read".into(),
            description: "读文件".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }]
    }

    #[test]
    fn url_handles_v1_suffix() {
        assert_eq!(p().messages_url(), "https://api.anthropic.com/v1/messages");
        let mut c = cfg();
        c.base_url = "https://proxy.example/v1".into();
        assert_eq!(
            AnthropicProvider::new(c).unwrap().messages_url(),
            "https://proxy.example/v1/messages"
        );
    }

    #[test]
    fn system_is_hoisted_and_tools_use_input_schema() {
        let msgs = vec![system_message("你是助手"), user_message("读一下 a.txt")];
        let (body, map) = p().build_body(&msgs, &schema());
        assert_eq!(body["system"], "你是助手");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1, "system 不该进 messages");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body["tools"][0].get("parameters").is_none());
        assert!(body["max_tokens"].as_u64().unwrap() >= 1);
        assert_eq!(map.get("file_read").map(String::as_str), Some("file.read"));
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let msgs = vec![
            user_message("做两件事"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                tool_calls: Some(vec![
                    ToolCall { id: "a".into(), name: "file.read".into(), arguments: "{}".into() },
                    ToolCall { id: "b".into(), name: "file.read".into(), arguments: "{}".into() },
                ]),
                tool_call_id: None,
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "{\"ok\":1}".into(),
                tool_calls: None,
                tool_call_id: Some("a".into()),
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: "{\"ok\":2}".into(),
                tool_calls: None,
                tool_call_id: Some("b".into()),
            },
        ];
        let (body, _) = p().build_body(&msgs, &schema());
        let arr = body["messages"].as_array().unwrap();
        assert_eq!(arr.len(), 3, "user + assistant + 合并后的 user");
        assert_eq!(arr[1]["content"].as_array().unwrap().len(), 2, "两个 tool_use");
        assert_eq!(arr[2]["content"].as_array().unwrap().len(), 2, "两个 tool_result 合并在同一 user");
        assert_eq!(arr[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn parses_text_and_tool_use() {
        let v = json!({
            "content": [
                {"type":"text","text":"我先看看文件"},
                {"type":"tool_use","id":"tu_1","name":"file_read","input":{"path":"a.txt"}}
            ],
            "usage": {"input_tokens": 120, "output_tokens": 34},
            "stop_reason": "tool_use"
        });
        let mut map = HashMap::new();
        map.insert("file_read".to_string(), "file.read".to_string());
        let c = p().parse_response(&v, &map).unwrap();
        assert_eq!(c.content.as_deref(), Some("我先看看文件"));
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "file.read");
        assert!(c.tool_calls[0].arguments.contains("a.txt"));
        assert_eq!(c.usage.prompt_tokens, 120);
        assert_eq!(c.usage.completion_tokens, 34);
    }

    #[test]
    fn missing_content_is_protocol_error() {
        let e = p().parse_response(&json!({"error":"x"}), &HashMap::new()).unwrap_err();
        assert!(matches!(e, LlmError::InvalidResponse(_)));
    }

    #[test]
    fn chat_mode_omits_tools() {
        let mut c = cfg();
        c.mode = "chat".into();
        let pr = AnthropicProvider::new(c).unwrap();
        let (body, _) = pr.build_body(&[user_message("hi")], &schema());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn requires_api_key() {
        let mut c = cfg();
        c.api_key = None;
        assert!(AnthropicProvider::new(c).is_err());
    }

    #[test]
    fn assistant_text_and_blank_content() {
        let msgs = vec![assistant_message("   ")];
        let (body, _) = p().build_body(&msgs, &[]);
        assert!(body["messages"].as_array().unwrap().is_empty(), "空白 assistant 不该产出空 content 块");
    }
}
