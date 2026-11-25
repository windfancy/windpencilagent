//! LLM 供应商类型与 OpenAI 兼容网关（骨架实现）。


use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

/// 工具名清洗：DeepSeek / OpenAI function-calling 要求 `^[a-zA-Z0-9_-]+$`，
/// 而内置工具用命名空间点号（`net.http`）。发送前把非法字符统一映射为 `_`，
/// 响应回来再按同一映射还原为原始名（见 `complete` 内 `name_to_orig`）。
pub fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn default_agent_mode() -> String {
    "agent".into()
}

use kern_core::{Component, ComponentKind, ComponentStatus};

/// LLM 网关错误。
#[derive(Debug, Error)]
pub enum LlmError {
    #[error("provider misconfigured: {0}")]
    Config(String),
    #[error("http request failed: {0}")]
    Http(String),
    #[error("request timed out: {0}")]
    Timeout(String),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

/// 错误分类（R4-3，dsh 对齐）：供调用方按类处理（重试/降级/上报）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmErrorKind {
    /// 限流（429）
    RateLimit,
    /// 上游 5xx
    Upstream,
    /// 请求超时
    Timeout,
    /// 网络错误（连接失败/断开）
    Network,
    /// 鉴权失败（401/403）
    Auth,
    /// 请求非法（4xx 其他）
    BadRequest,
    /// 响应协议解析失败
    Protocol,
    /// 本地配置错误
    Config,
}

impl LlmError {
    pub fn kind(&self) -> LlmErrorKind {
        match self {
            LlmError::Config(_) => LlmErrorKind::Config,
            LlmError::Http(_) => LlmErrorKind::Network,
            LlmError::Timeout(_) => LlmErrorKind::Timeout,
            LlmError::InvalidResponse(_) => LlmErrorKind::Protocol,
            LlmError::Api { status, .. } => match status {
                429 => LlmErrorKind::RateLimit,
                401 | 403 => LlmErrorKind::Auth,
                s if (500..600).contains(s) => LlmErrorKind::Upstream,
                _ => LlmErrorKind::BadRequest,
            },
        }
    }

    /// 是否值得重试（R4-3）：限流/5xx/超时/网络抖动。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind(),
            LlmErrorKind::RateLimit | LlmErrorKind::Upstream | LlmErrorKind::Timeout | LlmErrorKind::Network
        )
    }
}

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// 对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// 工具调用的 schema 表达。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 传给 provider 的工具定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    #[serde(rename = "parameters")]
    pub parameters: Value,
}

/// 用量统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// 完整回复。
#[derive(Debug, Clone)]
pub struct Completion {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: CompletionUsage,
}

/// Provider 配置。
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub driver: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 工作模式：agent（携带 tools，执行工具任务）/ chat（纯对话，不携带 tools）。
    /// 本工具定位为执行 agent 任务，默认 agent。
    #[serde(default = "default_agent_mode")]
    pub mode: String,
    /// 重试次数上限（R4-3）：429/5xx/超时/网络错误时的额外重试次数，默认 3。
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_max_retries() -> u32 {
    3
}

/// LLM 供应商接口。
#[async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &str;
    fn model(&self) -> &str;
    /// 是否为 agent 模式（决定是否携带 tools）。默认 true。
    fn is_agent_mode(&self) -> bool { true }
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> Result<Completion, LlmError>;
    /// R4-2：若支持 SSE 流式，返回 [`LlmStreaming`] 句柄；默认 None（非流式）。
    fn as_streamer(&self) -> Option<&dyn LlmStreaming> {
        None
    }
}

/// OpenAI 兼容协议实现（Ollama v1、vLLM、DeepSeek OpenAI 等）。
#[derive(Clone)]
pub struct OpenAiCompatibleProvider {
    cfg: ProviderConfig,
    client: reqwest::Client,
}

impl OpenAiCompatibleProvider {
    pub fn new(cfg: ProviderConfig) -> Result<Self, LlmError> {
        if cfg.base_url.is_empty() || cfg.model.is_empty() {
            return Err(LlmError::Config("base_url/model 必填".into()));
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| LlmError::Config(e.to_string()))?;
        Ok(Self { cfg, client })
    }

    fn chat_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    /// 构造 OpenAI 兼容请求体（非流式/流式共用，R4-2 抽取）。
    /// 返回 `(body, 清洗名→原始名映射)`：发出去用合法名，收回来还原成注册表名。
    fn build_request_body(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
    ) -> (Value, HashMap<String, String>) {
        // agent 模式才携带 tools；chat 模式即便传入也忽略，保证纯对话可用。
        let tools: &[ToolSchema] = if self.is_agent_mode() { tools } else { &[] };
        let name_to_orig: HashMap<String, String> = tools
            .iter()
            .map(|t| (sanitize_tool_name(&t.name), t.name.clone()))
            .collect();
        let mut body = json!({
            "model": self.cfg.model,
            // 消息按 OpenAI 标准逐条转换：assistant 回填的 tool_calls 必须为
            // {"id","type":"function","function":{name,arguments}} 嵌套格式，
            // 扁平 {id,name,arguments} 会被部分兼容服务端拒绝
            // （"Failed to parse messages: Missing tool call type"）。
            "messages": Value::Array(
                messages
                    .iter()
                    .map(|m| {
                        let mut v = json!({ "role": m.role, "content": m.content });
                        if let Some(tcs) = &m.tool_calls {
                            v["tool_calls"] = Value::Array(
                                tcs.iter()
                                    .map(|tc| {
                                        json!({
                                            "id": tc.id,
                                            "type": "function",
                                            "function": {
                                                "name": sanitize_tool_name(&tc.name),
                                                "arguments": tc.arguments,
                                            }
                                        })
                                    })
                                    .collect(),
                            );
                        }
                        if let Some(id) = &m.tool_call_id {
                            v["tool_call_id"] = json!(id);
                        }
                        v
                    })
                    .collect(),
            ),
        });
        if !tools.is_empty() {
            // OpenAI 标准嵌套格式：{"type":"function","function":{name,description,parameters}}。
            // 扁平格式（无 type 字段）会被部分兼容服务端拒绝（如本地 Strata 引擎
            // "Failed to parse tools: Missing tool type"），嵌套格式为各端最大公约数。
            let tools_json: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": sanitize_tool_name(&t.name),
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools_json);
        }
        if let Some(t) = self.cfg.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(mt) = self.cfg.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        (body, name_to_orig)
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
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
        let (body, name_to_orig) = self.build_request_body(messages, tools);

        // R4-3（dsh 对齐）：指数退避重试——429/5xx/超时/网络错误最多额外重试
        // `max_retries` 次；优先尊重 Retry-After 头，否则 1s→2s→4s 退避（上限 10s）。
        let mut attempt: u32 = 0;
        let (status, text) = loop {
            attempt += 1;
            let mut request = self.client.post(self.chat_url()).json(&body);
            if let Some(k) = &self.cfg.api_key {
                request = request.bearer_auth(k);
            }
            let send = async {
                let resp = request.send().await.map_err(|e| {
                    if e.is_timeout() {
                        LlmError::Timeout(e.to_string())
                    } else {
                        LlmError::Http(e.to_string())
                    }
                })?;
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let text = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
                Ok::<_, LlmError>((status, retry_after, text))
            };
            let (status, retry_after, text) = match send.await {
                Ok(t) => t,
                // 网络/超时错误无 Retry-After 可用：纯指数退避
                Err(e) if e.is_retryable() && attempt <= self.cfg.max_retries => {
                    let backoff =
                        std::time::Duration::from_millis(1000u64 << (attempt - 1).min(3));
                    tracing::warn!(attempt, error = %e, backoff_ms = backoff.as_millis() as u64, "LLM 请求失败，退避重试");
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if !(200..300).contains(&status)
                && (LlmError::Api { status, body: String::new() }).is_retryable()
                && attempt <= self.cfg.max_retries
            {
                let backoff = retry_after
                    .map(|s| std::time::Duration::from_secs(s.min(30)))
                    .unwrap_or_else(|| {
                        std::time::Duration::from_millis(1000u64 << (attempt - 1).min(3))
                    });
                tracing::warn!(attempt, status, backoff_ms = backoff.as_millis() as u64, "LLM 上游错误，退避重试");
                tokio::time::sleep(backoff).await;
                continue;
            }
            break (status, text);
        };

        if !(200..300).contains(&status) {
            return Err(LlmError::Api { status, body: text });
        }

        let v: Value = serde_json::from_str(&text)
            .map_err(|e| LlmError::InvalidResponse(e.to_string()))?;

        let message = &v["choices"][0]["message"];
        let content = message["content"].as_str().map(|s| s.to_string());
        let mut tool_calls = Vec::new();
        if let Some(tcs) = message["tool_calls"].as_array() {
            for tc in tcs {
                let raw_name = tc["function"]["name"].as_str().unwrap_or("");
                // 模型回传的是清洗名，还原为注册表名（如 net_http → net.http）。
                let name = name_to_orig
                    .get(raw_name)
                    .cloned()
                    .unwrap_or_else(|| raw_name.to_string());
                tool_calls.push(ToolCall {
                    id: tc["id"].as_str().unwrap_or("").to_string(),
                    name,
                    arguments: tc["function"]["arguments"].as_str().unwrap_or("{}").to_string(),
                });
            }
        }

        let usage = CompletionUsage {
            prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
        };

        Ok(Completion {
            content,
            tool_calls,
            usage,
        })
    }

    fn as_streamer(&self) -> Option<&dyn LlmStreaming> {
        Some(self)
    }
}

/// 流式扩展接口（R4-2，dsh 思考卡片对齐）：支持 SSE 增量的 provider 实现。
/// 不支持流式的 provider 走 [`LlmProvider::complete`] 降级路径。
#[async_trait]
pub trait LlmStreaming: Send + Sync + 'static {
    async fn stream_complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        // 显式 HRTB：async_trait 会把 `Fn(&str)` 固化为具体生命周期，
        // 转发给 feed_sse_line 的 for<'s> Fn 时类型不匹配（E0308）
        sink: &(dyn for<'s> Fn(&'s str) + Send + Sync),
    ) -> Result<Completion, LlmError>;
}

#[async_trait]
impl LlmStreaming for OpenAiCompatibleProvider {
    async fn stream_complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        sink: &(dyn for<'s> Fn(&'s str) + Send + Sync),
    ) -> Result<Completion, LlmError> {
        let (mut body, name_to_orig) = self.build_request_body(messages, tools);
        // OpenAI 流式：增量 chunk；include_usage 让最后一个 chunk 携带 usage
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });

        let mut request = self.client.post(self.chat_url()).json(&body);
        if let Some(k) = &self.cfg.api_key {
            request = request.bearer_auth(k);
        }
        let mut resp = request.send().await.map_err(|e| {
            if e.is_timeout() {
                LlmError::Timeout(e.to_string())
            } else {
                LlmError::Http(e.to_string())
            }
        })?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status, body: text });
        }

        // SSE 逐行解析 + delta 聚合（R4-2）。
        // 注：用 resp.chunk() 手动按 \n 切行——reqwest 未开 stream feature，
        // bytes_stream 不可用；chunk() 无 feature 要求。
        let mut agg = DeltaAggregator::default();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await.map_err(|e| LlmError::Http(e.to_string()))? {
                Some(bytes) => buf.extend_from_slice(&bytes),
                None => break, // 流结束
            }
            // 处理缓冲中的完整行（含结尾 \n）
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                if !feed_sse_line(&line_bytes, &mut agg, sink) {
                    return agg.finish(name_to_orig); // [DONE]
                }
            }
        }
        // 流结束后的残余行（无结尾换行）
        if !buf.is_empty() {
            let _ = feed_sse_line(&buf, &mut agg, sink);
        }
        agg.finish(name_to_orig)
    }
}

/// SSE `data:` 行提取（R4-2）：返回载荷（`[DONE]` 原样返回）；非 data 行返回 None。
fn parse_sse_data(line: &str) -> Option<&str> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let payload = trimmed.strip_prefix("data:")?;
    Some(payload.trim())
}

/// R4-2：喂一行 SSE 到聚合器。返回 false 表示遇到 `[DONE]`（流正常终止）。
/// sink 用显式 HRTB（`for<'s> Fn(&'s str)`）——async_trait 会把非 HRTB 的
/// `Fn(&str)` 固化为具体生命周期，转发时类型不再匹配（E0308/lifetime 错）。
fn feed_sse_line(
    line_bytes: &[u8],
    agg: &mut DeltaAggregator,
    sink: &(dyn for<'s> Fn(&'s str) + Send + Sync),
) -> bool {
    let line = String::from_utf8_lossy(line_bytes);
    let Some(payload) = parse_sse_data(&line) else { return true };
    if payload == "[DONE]" {
        return false;
    }
    let Ok(chunk) = serde_json::from_str::<Value>(payload) else { return true };
    // 服务端不支持流式时可能整包返回非增量 JSON（无 choices 特征）——跳过
    if chunk.get("choices").and_then(Value::as_array).is_none() {
        return true;
    }
    if let Some(delta) = chunk["choices"][0]["delta"].as_object() {
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                agg.push_content(text);
                sink(text);
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                agg.push_tool_delta(
                    tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                    tc.get("id").and_then(Value::as_str),
                    tc.get("function").and_then(|f| f.get("name")).and_then(Value::as_str),
                    tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str),
                );
            }
        }
    }
    if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
        agg.set_usage(
            u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
        );
    }
    true
}

/// R4-2（dsh 对齐）：流式 delta 聚合器——content 增量拼接、tool_calls 分片重组。
///
/// OpenAI 流式协议中 tool_calls 按增量下发：首个分片携带 id/name，
/// 后续分片仅带 `index` 与 `arguments` 增量，需按 index 缓冲拼接。
#[derive(Default)]
pub struct DeltaAggregator {
    content: String,
    tools: std::collections::BTreeMap<usize, ToolCall>,
    prompt_tokens: u32,
    completion_tokens: u32,
}

impl DeltaAggregator {
    pub fn push_content(&mut self, text: &str) {
        self.content.push_str(text);
    }

    pub fn push_tool_delta(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args_delta: Option<&str>,
    ) {
        let entry = self.tools.entry(index).or_insert_with(|| ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        });
        if let Some(v) = id {
            entry.id.push_str(v);
        }
        if let Some(v) = name {
            entry.name.push_str(v);
        }
        if let Some(v) = args_delta {
            entry.arguments.push_str(v);
        }
    }

    pub fn set_usage(&mut self, prompt: u32, completion: u32) {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
    }

    /// 产出最终 Completion（tool 名还原为注册表名）。
    pub fn finish(self, name_to_orig: HashMap<String, String>) -> Result<Completion, LlmError> {
        let tool_calls: Vec<ToolCall> = self
            .tools
            .into_values()
            .map(|mut tc| {
                tc.name = name_to_orig
                    .get(&tc.name)
                    .cloned()
                    .unwrap_or_else(|| std::mem::take(&mut tc.name));
                tc
            })
            .collect();
        if self.content.is_empty() && tool_calls.is_empty() {
            return Err(LlmError::InvalidResponse(
                "stream finished with empty content and no tool_calls".into(),
            ));
        }
        Ok(Completion {
            content: (!self.content.is_empty()).then(|| self.content.clone()),
            tool_calls,
            usage: CompletionUsage {
                prompt_tokens: self.prompt_tokens,
                completion_tokens: self.completion_tokens,
            },
        })
    }
}

/// 组件适配：把 provider 作为可插接组件。
pub struct LlmComponent {
    provider: Box<dyn LlmProvider>,
    /// provider 静态信息缓存
    name: String,
    model_name: String,
}

impl LlmComponent {
    pub fn new(provider: Box<dyn LlmProvider>) -> Self {
        let name = provider.provider_name().to_string();
        let model_name = provider.model().to_string();
        Self {
            provider,
            name,
            model_name,
        }
    }

    pub fn provider(&self) -> &dyn LlmProvider {
        self.provider.as_ref()
    }
}

impl Component for LlmComponent {
    fn id(&self) -> &str {
        &self.model_name
    }
    fn kind(&self) -> ComponentKind {
        ComponentKind::Llm
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> String {
        format!("llm provider `{}` model `{}`", self.name, self.model_name)
    }
    fn status(&self) -> ComponentStatus {
        ComponentStatus::Running
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// 便捷：构造默认 provider 的组件。
pub fn default_component(cfg: ProviderConfig) -> Result<LlmComponent, LlmError> {
    Ok(LlmComponent::new(build_provider(cfg)?))
}

/// 支持的驱动名（UI 下拉用；顺序即推荐顺序）。
pub const DRIVERS: &[&str] = &["openai", "anthropic", "gemini"];

/// 驱动名归一化。
///
/// **未知驱动必须报错**，不能悄悄回落到 OpenAI —— 用户把 `driver` 拼错成
/// `antropic` 时，静默回落会让请求打到 OpenAI 兼容端点并返回 404，
/// 报错信息完全指不出真正原因。
pub fn canonical_driver(driver: &str) -> Result<&'static str, String> {
    let d = driver.trim().to_ascii_lowercase();
    match d.as_str() {
        "" | "openai" | "openai-compatible" | "openai_compatible" | "openai-compatible-v1" => {
            Ok("openai")
        }
        "anthropic" | "claude" => Ok("anthropic"),
        "gemini" | "google" | "google-gemini" => Ok("gemini"),
        other => Err(format!(
            "未知 LLM 驱动 {:?}；支持：{}（OpenAI 兼容端点请用 openai）",
            other,
            DRIVERS.join(" / ")
        )),
    }
}

/// 按驱动名构造 provider。
pub fn build_provider(cfg: ProviderConfig) -> Result<Box<dyn LlmProvider>, LlmError> {
    let driver = canonical_driver(&cfg.driver).map_err(LlmError::Config)?;
    match driver {
        "anthropic" => Ok(Box::new(crate::anthropic::AnthropicProvider::new(cfg)?)),
        "gemini" => Ok(Box::new(crate::gemini::GeminiProvider::new(cfg)?)),
        _ => Ok(Box::new(OpenAiCompatibleProvider::new(cfg)?)),
    }
}

/// 启动预检（问题2）：发一个带最小 tool 的最小请求，验证 API 连通性与
/// agent（tools）能力是否真实可用。任何一个环节失败都返回 Err，调用方据此降级。
/// chat 模式下 provider 内部会忽略 tools，只验证连通性——符合预期。
pub async fn probe_provider(provider: &dyn LlmProvider) -> Result<String, String> {
    let tools = vec![ToolSchema {
        name: "probe.ping".into(),
        description: "连通性探测用工具，正常无需调用".into(),
        parameters: json!({"type": "object", "properties": {}}),
    }];
    let msgs = vec![user_message("请只回复 OK")];
    match provider.complete(&msgs, &tools).await {
        Ok(c) => Ok(c.content.unwrap_or_default().chars().take(80).collect()),
        Err(e) => Err(e.to_string()),
    }
}

/// 便捷：构造 system 消息。
pub fn system_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::System,
        content: content.into(),
        tool_calls: None,
        tool_call_id: None,
    }
}

/// 便捷：构造 user 消息。
pub fn user_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        content: content.into(),
        tool_calls: None,
        tool_call_id: None,
    }
}

/// 便捷：构造 assistant 消息。
pub fn assistant_message(content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Assistant,
        content: content.into(),
        tool_calls: None,
        tool_call_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_tool_names_match_openai_pattern() {
        // DeepSeek / OpenAI: ^[a-zA-Z0-9_-]+$
        let valid = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        };
        for name in [
            "net.http", "net.dns", "scan.port", "scan.web-vuln-internal",
            "llm.profiles", "ws.memory.append", "shell.run", "util.echo",
        ] {
            let cleaned = sanitize_tool_name(name);
            assert!(valid(&cleaned), "清洗后仍非法: {name} -> {cleaned}");
        }
        assert_eq!(sanitize_tool_name("net.http"), "net_http");
        assert_eq!(sanitize_tool_name("scan.web-vuln-internal"), "scan_web-vuln-internal");
        assert_eq!(sanitize_tool_name("already_ok-1"), "already_ok-1");
    }

    #[test]
    fn reverse_map_restores_original_names() {
        // 模拟 complete 内的双向映射：清洗名唯一映射回原始名。
        let tools = [
            ToolSchema {
                name: "net.http".into(),
                description: "".into(),
                parameters: json!({}),
            },
            ToolSchema {
                name: "scan.port".into(),
                description: "".into(),
                parameters: json!({}),
            },
        ];
        let map: HashMap<String, String> = tools
            .iter()
            .map(|t| (sanitize_tool_name(&t.name), t.name.clone()))
            .collect();
        assert_eq!(map.get("net_http").map(String::as_str), Some("net.http"));
        assert_eq!(map.get("scan_port").map(String::as_str), Some("scan.port"));
        // 未知清洗名原样保留，不 panic。
        assert_eq!(
            map.get("ghost_tool").cloned().unwrap_or_else(|| "ghost_tool".into()),
            "ghost_tool"
        );
    }

    #[test]
    fn chat_mode_disables_tools() {
        let provider = OpenAiCompatibleProvider::new(ProviderConfig {
            driver: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            api_key: None,
            temperature: None,
            max_tokens: None,
            mode: "chat".into(),
            max_retries: 3,
        })
        .unwrap();
        assert!(!provider.is_agent_mode());
        let agent = OpenAiCompatibleProvider::new(ProviderConfig {
            driver: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            api_key: None,
            temperature: None,
            max_tokens: None,
            mode: "agent".into(),
            max_retries: 3,
        })
        .unwrap();
        assert!(agent.is_agent_mode());
    }

    /// R4-3：错误分类正确（限流/鉴权/上游/协议）。
    #[test]
    fn error_kind_classification() {
        let rate = LlmError::Api { status: 429, body: "rate".into() };
        assert_eq!(rate.kind(), LlmErrorKind::RateLimit);
        assert!(rate.is_retryable());
        let auth = LlmError::Api { status: 401, body: "x".into() };
        assert_eq!(auth.kind(), LlmErrorKind::Auth);
        assert!(!auth.is_retryable());
        let upstream = LlmError::Api { status: 503, body: "x".into() };
        assert_eq!(upstream.kind(), LlmErrorKind::Upstream);
        assert!(upstream.is_retryable());
        let bad = LlmError::Api { status: 400, body: "x".into() };
        assert_eq!(bad.kind(), LlmErrorKind::BadRequest);
        assert!(!bad.is_retryable());
        assert_eq!(LlmError::InvalidResponse("x".into()).kind(), LlmErrorKind::Protocol);
        assert_eq!(LlmError::Http("conn".into()).kind(), LlmErrorKind::Network);
        assert_eq!(LlmError::Timeout("t".into()).kind(), LlmErrorKind::Timeout);
    }

    /// R4-2：SSE data 行提取（含 [DONE] 与非 data 行）。
    #[test]
    fn parse_sse_data_extracts_payload() {
        assert_eq!(super::parse_sse_data("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(super::parse_sse_data("data: [DONE]"), Some("[DONE]"));
        assert_eq!(super::parse_sse_data("data:[DONE]"), Some("[DONE]"));
        assert_eq!(super::parse_sse_data(": keep-alive comment"), None);
        assert_eq!(super::parse_sse_data("event: message"), None);
        assert_eq!(super::parse_sse_data(""), None);
    }

    /// R4-2：流式 delta 聚合——content 增量 + tool_calls 分片重组 + 名字还原。
    #[test]
    fn delta_aggregator_reassembles_tool_calls() {
        let mut agg = DeltaAggregator::default();
        agg.push_content("正在");
        agg.push_content("思考…");
        // 首个分片：id + 完整名
        agg.push_tool_delta(0, Some("call-1"), Some("net_http"), Some("{\"u"));
        // 后续分片：仅 index + arguments 增量
        agg.push_tool_delta(0, None, None, Some("rl\":\"http"));
        agg.push_tool_delta(0, None, None, Some("://x\"}"));
        // 第二个工具
        agg.push_tool_delta(1, Some("call-2"), Some("scan_port"), Some("{}"));
        agg.set_usage(100, 42);

        let mut map = HashMap::new();
        map.insert("net_http".to_string(), "net.http".to_string());
        map.insert("scan_port".to_string(), "scan.port".to_string());
        let c = agg.finish(map).unwrap();
        assert_eq!(c.content.as_deref(), Some("正在思考…"));
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].id, "call-1");
        assert_eq!(c.tool_calls[0].name, "net.http"); // 清洗名已还原
        assert_eq!(c.tool_calls[0].arguments, "{\"url\":\"http://x\"}");
        assert_eq!(c.tool_calls[1].name, "scan.port");
        assert_eq!(c.usage.completion_tokens, 42);
    }

    /// R4-2：空流（无 content 无 tool_calls）→ InvalidResponse（触发上层降级）。
    #[test]
    fn delta_aggregator_empty_stream_is_error() {
        let agg = DeltaAggregator::default();
        assert!(matches!(
            agg.finish(HashMap::new()),
            Err(LlmError::InvalidResponse(_))
        ));
    }
}
