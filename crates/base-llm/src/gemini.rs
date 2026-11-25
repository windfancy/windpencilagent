//! Google Gemini `generateContent` 原生实现。
//!
//! 与 OpenAI 协议的差异（都不是"翻译一下就行"的差异）：
//! - 角色只有 `user` / `model`，system 走 `systemInstruction` 顶层字段
//! - 工具调用是 `parts[].functionCall{name,args}`，**没有调用 id** ⇒ 内部合成 `gc_{n}`
//! - 工具结果用 `parts[].functionResponse{name,response}`，按 **函数名** 对应而不是 id
//!   ⇒ 必须记住"这一轮哪个 id 对应哪个名字"，否则多工具并发回填会串
//! - 工具声明用 `functionDeclarations`，参数 schema 走 `parameters`
//!
//! 因此这里维护 `id → 原始工具名` 的映射，并在解析 tool 消息时回查。

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::types::{
    sanitize_tool_name, ChatMessage, ChatRole, Completion, CompletionUsage, LlmError, LlmProvider,
    ProviderConfig, ToolCall, ToolSchema,
};

/// Gemini 原生 provider。
pub struct GeminiProvider {
    cfg: ProviderConfig,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(cfg: ProviderConfig) -> Result<Self, LlmError> {
        if cfg.base_url.is_empty() || cfg.model.is_empty() {
            return Err(LlmError::Config("base_url/model 必填".into()));
        }
        if cfg.api_key.as_deref().unwrap_or("").trim().is_empty() {
            return Err(LlmError::Config("gemini 驱动需要 api_key".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .map_err(|e| LlmError::Config(e.to_string()))?;
        Ok(Self { cfg, client })
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.cfg.base_url.trim_end_matches('/'),
            self.cfg.model
        )
    }

    /// 构造请求体。返回 `(body, 清洗名→原始名, id→清洗名)`。
    pub fn build_body(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> (Value, HashMap<String, String>, HashMap<String, String>) {
        let tools_slice: &[ToolSchema] = if self.cfg.mode == "chat" { &[] } else { tools };
        let name_to_orig: HashMap<String, String> = tools_slice
            .iter()
            .map(|t| (sanitize_tool_name(&t.name), t.name.clone()))
            .collect();

        let mut system_parts: Vec<String> = Vec::new();
        let mut contents: Vec<Value> = Vec::new();
        // tool_call_id → 清洗后的函数名（Gemini 回填结果只认名字）
        let mut id_to_name: HashMap<String, String> = HashMap::new();

        for m in messages {
            match m.role {
                ChatRole::System => {
                    if !m.content.trim().is_empty() {
                        system_parts.push(m.content.clone());
                    }
                }
                ChatRole::User => push_part(
                    &mut contents,
                    "user",
                    json!({ "text": m.content }),
                ),
                ChatRole::Assistant => {
                    let mut parts: Vec<Value> = Vec::new();
                    if !m.content.trim().is_empty() {
                        parts.push(json!({ "text": m.content }));
                    }
                    for tc in m.tool_calls.iter().flatten() {
                        let clean = sanitize_tool_name(&tc.name);
                        id_to_name.insert(tc.id.clone(), clean.clone());
                        let args: Value = serde_json::from_str(&tc.arguments)
                            .unwrap_or_else(|_| json!({}));
                        parts.push(json!({ "functionCall": { "name": clean, "args": args } }));
                    }
                    if !parts.is_empty() {
                        push_parts(&mut contents, "model", parts);
                    }
                }
                ChatRole::Tool => {
                    let id = m.tool_call_id.clone().unwrap_or_default();
                    let name = id_to_name.get(&id).cloned().unwrap_or_else(|| id.clone());
                    let payload: Value = serde_json::from_str(&m.content)
                        .unwrap_or_else(|_| json!({ "text": m.content }));
                    push_part(
                        &mut contents,
                        "user",
                        json!({ "functionResponse": { "name": name, "response": payload } }),
                    );
                }
            }
        }

        let mut body = json!({ "contents": contents });
        if !system_parts.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system_parts.join("\n\n") }] });
        }
        if !tools_slice.is_empty() {
            let decls: Vec<Value> = tools_slice
                .iter()
                .map(|t| {
                    json!({
                        "name": sanitize_tool_name(&t.name),
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!([{ "functionDeclarations": decls }]);
        }
        let mut gen = serde_json::Map::new();
        if let Some(t) = self.cfg.temperature {
            gen.insert("temperature".into(), json!(t));
        }
        if let Some(mt) = self.cfg.max_tokens {
            gen.insert("maxOutputTokens".into(), json!(mt));
        }
        if !gen.is_empty() {
            body["generationConfig"] = Value::Object(gen);
        }
        (body, name_to_orig, id_to_name)
    }

    /// 解析响应。纯函数，便于离线测试。
    pub fn parse_response(
        &self,
        v: &Value,
        name_to_orig: &HashMap<String, String>,
    ) -> Result<Completion, LlmError> {
        // 上游安全拦截：有 promptFeedback.blockReason 且无 candidates
        if let Some(reason) = v
            .get("promptFeedback")
            .and_then(|p| p.get("blockReason"))
            .and_then(Value::as_str)
        {
            return Err(LlmError::InvalidResponse(format!("请求被安全策略拦截：{reason}")));
        }
        let cand = v
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .ok_or_else(|| LlmError::InvalidResponse(format!("缺少 candidates：{}", brief(v))))?;
        let parts = cand
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            if let Some(fc) = p.get("functionCall") {
                let name = fc.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let orig = name_to_orig.get(&name).cloned().unwrap_or(name);
                let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(ToolCall {
                    id: format!("gc_{i}"),
                    name: orig,
                    arguments: args.to_string(),
                });
            }
        }
        let usage = v.get("usageMetadata");
        let usage = CompletionUsage {
            prompt_tokens: usage
                .and_then(|u| u.get("promptTokenCount"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            completion_tokens: usage
                .and_then(|u| u.get("candidatesTokenCount"))
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

/// 相邻同角色消息合并：Gemini 不接受连续两条 user（会 400）。
fn push_parts(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = contents.last_mut() {
        if last["role"] == role {
            if let Some(arr) = last.get_mut("parts").and_then(Value::as_array_mut) {
                arr.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

fn push_part(contents: &mut Vec<Value>, role: &str, part: Value) {
    push_parts(contents, role, vec![part]);
}

fn brief(v: &Value) -> String {
    v.to_string().chars().take(300).collect()
}

#[async_trait]
impl LlmProvider for GeminiProvider {
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
        let (body, name_to_orig, _) = self.build_body(messages, tools);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let resp = self
                .client
                .post(self.endpoint())
                .header("x-goog-api-key", self.cfg.api_key.clone().unwrap_or_default())
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
                tracing::warn!(attempt, error = %err, "gemini 请求失败，退避重试");
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
    use crate::types::{system_message, user_message};

    fn p() -> GeminiProvider {
        GeminiProvider::new(ProviderConfig {
            driver: "gemini".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-2.5-pro".into(),
            api_key: Some("k".into()),
            temperature: Some(0.3),
            max_tokens: Some(2048),
            mode: "agent".into(),
            max_retries: 0,
        })
        .unwrap()
    }

    fn schema() -> Vec<ToolSchema> {
        vec![ToolSchema {
            name: "file.read".into(),
            description: "读文件".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }]
    }

    #[test]
    fn endpoint_and_system_instruction() {
        let pr = p();
        assert!(pr.endpoint().ends_with("/models/gemini-2.5-pro:generateContent"));
        let (body, _, _) = pr.build_body(&[system_message("你是助手"), user_message("hi")], &schema());
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "你是助手");
        assert_eq!(body["contents"].as_array().unwrap().len(), 1);
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "file_read");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
    }

    #[test]
    fn consecutive_same_role_are_merged() {
        // Gemini 对连续两条 user 会 400，必须合并
        let (body, _, _) = p().build_body(
            &[user_message("第一句"), user_message("第二句")],
            &[],
        );
        let arr = body["contents"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["parts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn tool_result_maps_back_to_function_name() {
        let msgs = vec![
            user_message("读 a.txt"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "gc_0".into(),
                    name: "file.read".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                }]),
                tool_call_id: None,
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: r#"{"content":"hello"}"#.into(),
                tool_calls: None,
                tool_call_id: Some("gc_0".into()),
            },
        ];
        let (body, _, ids) = p().build_body(&msgs, &schema());
        assert_eq!(ids.get("gc_0").map(String::as_str), Some("file_read"));
        let contents = body["contents"].as_array().unwrap();
        // user / model / user(functionResponse)
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "file_read");
        let fr = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "file_read", "回填必须用函数名而不是 id");
        assert_eq!(fr["response"]["content"], "hello");
    }

    #[test]
    fn parses_function_call_and_usage() {
        let v = json!({
            "candidates": [{
                "content": { "parts": [
                    {"text":"查一下"},
                    {"functionCall":{"name":"file_read","args":{"path":"a.txt"}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 88, "candidatesTokenCount": 12}
        });
        let mut map = HashMap::new();
        map.insert("file_read".to_string(), "file.read".to_string());
        let c = p().parse_response(&v, &map).unwrap();
        assert_eq!(c.content.as_deref(), Some("查一下"));
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "file.read");
        assert_eq!(c.tool_calls[0].id, "gc_1");
        assert_eq!(c.usage.prompt_tokens, 88);
    }

    #[test]
    fn blocked_prompt_is_reported() {
        let v = json!({"promptFeedback":{"blockReason":"SAFETY"}});
        let e = p().parse_response(&v, &HashMap::new()).unwrap_err();
        assert!(e.to_string().contains("SAFETY"));
    }

    #[test]
    fn chat_mode_omits_tools() {
        let mut cfg = ProviderConfig {
            driver: "gemini".into(),
            base_url: "https://x/v1beta".into(),
            model: "m".into(),
            api_key: Some("k".into()),
            temperature: None,
            max_tokens: None,
            mode: "chat".into(),
            max_retries: 0,
        };
        cfg.mode = "chat".into();
        let pr = GeminiProvider::new(cfg).unwrap();
        let (body, _, _) = pr.build_body(&[user_message("hi")], &schema());
        assert!(body.get("tools").is_none());
    }
}
