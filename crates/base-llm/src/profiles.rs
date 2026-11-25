//! LLM 设置插件（llm-settings）：多 profile 管理 + 国内主流 API 预设 + 联通性测试。
//!
//! 一切皆插件：ComponentKind::Llm/Plugin 登记；Tool llm.profiles / llm.test 供 LLM 调用。
//! 统一走 OpenAI 兼容协议（DeepSeek/通义/豆包/智谱/Kimi/MiniMax/文心/混元/Ollama/vLLM）。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;

use async_trait::async_trait;
use kern_core::{Component, ComponentKind, ComponentStatus};
use kern_core::model::{ExecutionContext, RiskLevel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::types::{user_message, ProviderConfig, ToolSchema};

/// 单个 LLM 配置档案。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmProfile {
    pub name: String,
    /// 驱动名。缺省 `openai`（OpenAI 兼容），保证旧配置/手写配置不带该字段也能解析。
    #[serde(default = "default_driver")]
    pub driver: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 工作模式：agent（携带 tools 执行工具任务）/ chat（纯对话）。默认 agent。
    #[serde(default = "default_mode")]
    pub mode: String,
    /// 上下文窗口（tokens）。缺省 = 32k（保守值），用于上下文预算分配。
    #[serde(default)]
    pub context_window: Option<u32>,
    /// 是否支持 function calling。缺省 = true；显式 false 时**不下发工具面**。
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// 是否支持视觉输入。缺省 = false。
    #[serde(default)]
    pub supports_vision: Option<bool>,
    /// 计价（可选，元/1k tokens），用于用量估算展示。
    #[serde(default)]
    pub cost_per_1k_in: Option<f32>,
    #[serde(default)]
    pub cost_per_1k_out: Option<f32>,
}

fn default_true() -> bool { true }
fn default_mode() -> String { "agent".into() }
fn default_driver() -> String { "openai".into() }

impl LlmProfile {
    /// 上下文窗口（缺省 32k）。
    pub fn context_window_or_default(&self) -> u32 {
        self.context_window.unwrap_or(32_000)
    }

    /// 是否支持工具调用（缺省支持）。
    pub fn supports_tools(&self) -> bool {
        self.supports_tools.unwrap_or(true)
    }

    /// 是否支持视觉（缺省不支持）。
    pub fn supports_vision(&self) -> bool {
        self.supports_vision.unwrap_or(false)
    }

    /// 该档案下是否应下发工具面：模式为 agent **且** 支持 function calling。
    pub fn tools_enabled(&self) -> bool {
        self.mode != "chat" && self.supports_tools()
    }

    pub fn to_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            driver: self.driver.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            mode: self.mode.clone(),
            max_retries: 3,
        }
    }

    /// 脱敏展示（api_key 仅留前 4 后 4）。
    pub fn redacted(&self) -> Value {
        let key = self.api_key.as_ref().map(|k| {
            if k.len() <= 8 { "****".to_string() }
            else { format!("{}****{}", &k[..4], &k[k.len()-4..]) }
        });
        json!({
            "name": self.name, "driver": self.driver, "base_url": self.base_url,
            "model": self.model, "api_key": key, "temperature": self.temperature,
            "max_tokens": self.max_tokens, "enabled": self.enabled, "mode": self.mode,
            "context_window": self.context_window_or_default(),
            "supports_tools": self.supports_tools(),
            "supports_vision": self.supports_vision(),
            "cost_per_1k_in": self.cost_per_1k_in,
            "cost_per_1k_out": self.cost_per_1k_out,
        })
    }
}

/// 主流 API 预设（端点 + 典型上下文窗口 + 正确的协议驱动）。
///
/// `driver` 按**协议**填，不按厂商填：只有 Anthropic 与 Google 两家不是 OpenAI 兼容，
/// 其余（含国产大厂与本地推理）全部走 `openai` 驱动。
pub fn builtin_profiles() -> Vec<LlmProfile> {
    let p = |name: &str, driver: &str, url: &str, model: &str, ctx: u32| LlmProfile {
        name: name.into(), driver: driver.into(), base_url: url.into(),
        model: model.into(), api_key: None, temperature: Some(0.2), max_tokens: Some(4096), enabled: true,
        mode: "agent".into(),
        context_window: Some(ctx),
        supports_tools: Some(true),
        supports_vision: Some(false),
        cost_per_1k_in: None,
        cost_per_1k_out: None,
    };
    vec![
        // —— 三家原生协议 ——
        p("openai", "openai", "https://api.openai.com/v1", "gpt-4.1-mini", 128_000),
        p("anthropic", "anthropic", "https://api.anthropic.com", "claude-sonnet-4-5", 200_000),
        p(
            "gemini",
            "gemini",
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-2.5-pro",
            1_000_000,
        ),
        // —— 国内主流（OpenAI 兼容）——
        p("deepseek", "openai", "https://api.deepseek.com/v1", "deepseek-chat", 65_536),
        p("qwen", "openai", "https://dashscope.aliyuncs.com/compatible-mode/v1", "qwen-plus", 131_072),
        p("doubao", "openai", "https://ark.cn-beijing.volces.com/api/v3", "doubao-pro-32k", 32_768),
        p("zhipu", "openai", "https://open.bigmodel.cn/api/paas/v4", "glm-4-plus", 131_072),
        p("kimi", "openai", "https://api.moonshot.cn/v1", "moonshot-v1-32k", 32_768),
        p("minimax", "openai", "https://api.minimaxi.com/v1", "MiniMax-Text-01", 1_000_000),
        p("wenxin", "openai", "https://qianfan.baidubce.com/v2", "ernie-4.0-8k", 8_192),
        p("hunyuan", "openai", "https://api.hunyuan.cloud.tencent.com/v1", "hunyuan-turbo", 32_768),
        // —— 聚合与本地 ——
        p("openrouter", "openai", "https://openrouter.ai/api/v1", "openai/gpt-4.1-mini", 128_000),
        p("siliconflow", "openai", "https://api.siliconflow.cn/v1", "Qwen/Qwen2.5-72B-Instruct", 32_768),
        p("ollama", "openai", "http://127.0.0.1:11434/v1", "qwen2.5:7b-instruct", 32_768),
        p("vllm", "openai", "http://127.0.0.1:8000/v1", "local-model", 32_768),
        p("lmstudio", "openai", "http://127.0.0.1:1234/v1", "local-model", 32_768),
    ]
}

/// Profile 注册表（内存；TOML 持久化由 app 配置层负责）。
#[derive(Debug, Default)]
pub struct ProfileRegistry {
    profiles: RwLock<HashMap<String, LlmProfile>>,
    active: RwLock<String>,
}

impl ProfileRegistry {
    pub fn new() -> Self { Self::default() }

    /// 载入内置预设（不覆盖已有）。
    pub fn ensure_builtin(&self) {
        let mut map = self.profiles.write().expect("profile lock poisoned");
        for p in builtin_profiles() {
            map.entry(p.name.clone()).or_insert(p);
        }
        if self.active.read().expect("lock").is_empty() {
            *self.active.write().expect("lock") = "deepseek".into();
        }
    }

    pub fn upsert(&self, p: LlmProfile) {
        self.profiles.write().expect("lock").insert(p.name.clone(), p);
    }

    pub fn remove(&self, name: &str) -> bool {
        self.profiles.write().expect("lock").remove(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<LlmProfile> {
        self.profiles.read().expect("lock").get(name).cloned()
    }

    pub fn active(&self) -> Option<LlmProfile> {
        let name = self.active.read().expect("lock").clone();
        self.get(&name)
    }

    pub fn set_active(&self, name: &str) -> bool {
        if self.get(name).is_some() {
            *self.active.write().expect("lock") = name.to_string();
            true
        } else { false }
    }

    pub fn all(&self) -> Vec<LlmProfile> {
        let mut v: Vec<_> = self.profiles.read().expect("lock").values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// 批量合并（从配置载入）。
    pub fn merge(&self, profiles: Vec<LlmProfile>, active: Option<&str>) {
        for p in profiles { self.upsert(p); }
        if let Some(a) = active { self.set_active(a); }
    }

    /// 联通性测试（问题1/2）：带一个最小 tool 探测 agent 能力，
    /// 不再只测纯 chat——否则"测试通过但工具调用报 function.name 错"。
    /// chat 模式的 profile 由 provider 内部忽略 tools，只验证连通性。
    pub async fn test(&self, name: &str) -> Result<Value, String> {
        let profile = self.get(name).ok_or_else(|| format!("未找到 profile `{name}`"))?;
        let provider = crate::types::build_provider(profile.to_provider_config())
            .map_err(|e| e.to_string())?;
        let probe = ToolSchema {
            name: "probe.ping".into(),
            description: "连通性探测用工具，正常无需调用".into(),
            parameters: json!({"type": "object", "properties": {}}),
        };
        let started = Instant::now();
        let result = provider.complete(
            &[user_message("请只回复 OK 两个字母")],
            &[probe],
        ).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(c) => Ok(json!({
                "profile": name,
                "ok": true,
                "latency_ms": latency_ms,
                "model": profile.model,
                "reply": c.content.unwrap_or_default().chars().take(80).collect::<String>(),
                "usage": c.usage,
            })),
            Err(e) => Ok(json!({
                "profile": name,
                "ok": false,
                "latency_ms": latency_ms,
                "model": profile.model,
                "error": format!("{e}"),
            })),
        }
    }
}

/// Component: llm-settings
pub struct LlmSettingsComponent {
    pub registry: std::sync::Arc<ProfileRegistry>,
}

impl Component for LlmSettingsComponent {
    fn id(&self) -> &str { "llm-settings" }
    fn kind(&self) -> ComponentKind { ComponentKind::Plugin }
    fn version(&self) -> &str { "1.0.0" }
    fn description(&self) -> String {
        "LLM 设置插件：多 profile（国内主流 API + 本地模型）+ 联通性测试".into()
    }
    fn status(&self) -> ComponentStatus { ComponentStatus::Loaded }
    fn as_any(&self) -> &dyn std::any::Any { self }
}

// ---- Tools（base-llm 依赖 toolkit-core）----
use toolkit_core::tools::{Tool, ToolResult};

pub struct LlmProfilesTool(pub std::sync::Arc<ProfileRegistry>);

#[async_trait]
impl Tool for LlmProfilesTool {
    fn name(&self) -> &str { "llm.profiles" }
    fn description(&self) -> String {
        "LLM 配置管理：列出/新增/更新/删除/激活模型 profile（支持国内主流 API 与本地模型）。\
操作由 action 指定：list/get/upsert/remove/activate。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["action"],"properties":{
            "action":{"type":"string","enum":["list","get","upsert","remove","activate"]},
            "profile":{"type":"object"},
            "name":{"type":"string"}
        }})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Medium }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        match action {
            "list" => {
                let list: Vec<Value> = self.0.all().iter().map(|p| p.redacted()).collect();
                ToolResult::json(
                    format!("LLM profile {} 个", list.len()),
                    json!({"profiles": list, "active": self.0.active.read().expect("lock").clone()}), ms())
            }
            "get" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                match self.0.get(name) {
                    Some(p) => ToolResult::json(format!("profile {name}"), p.redacted(), ms()),
                    None => ToolResult::error(format!("未找到 profile {name}"), ms()),
                }
            }
            "upsert" => {
                let p: LlmProfile = match serde_json::from_value(
                    args.get("profile").cloned().unwrap_or(Value::Null)) {
                    Ok(v) => v,
                    Err(e) => return ToolResult::error(format!("profile 解析失败: {e}"), ms()),
                };
                if p.base_url.is_empty() || p.model.is_empty() {
                    return ToolResult::error("base_url 与 model 必填", ms());
                }
                self.0.upsert(p.clone());
                ToolResult::json(format!("profile {} 已保存", p.name), p.redacted(), ms())
            }
            "remove" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                if self.0.remove(name) {
                    ToolResult::json(format!("profile {name} 已删除"), json!({"name": name}), ms())
                } else {
                    ToolResult::error(format!("未找到 profile {name}"), ms())
                }
            }
            "activate" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                if self.0.set_active(name) {
                    ToolResult::json(format!("已激活 profile {name}"), json!({"active": name}), ms())
                } else {
                    ToolResult::error(format!("未找到 profile {name}"), ms())
                }
            }
            other => ToolResult::error(format!("未知 action: {other}"), ms()),
        }
    }
}

pub struct LlmTestTool(pub std::sync::Arc<ProfileRegistry>);

#[async_trait]
impl Tool for LlmTestTool {
    fn name(&self) -> &str { "llm.test" }
    fn description(&self) -> String {
        "LLM 联通性测试：对指定 profile 发最小 chat 请求，返回 HTTP 状态、延迟与模型回显。\
不修改任何配置。".into()
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["name"],"properties":{"name":{"type":"string"}}})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Low }

    async fn call(&self, _ctx: &ExecutionContext, args: Value) -> ToolResult {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let name = if name.is_empty() {
            self.0.active.read().expect("lock").clone()
        } else { name.to_string() };
        match self.0.test(&name).await {
            Ok(r) => {
                let ok = r["ok"].as_bool().unwrap_or(false);
                let summary = format!(
                    "LLM {name} {}（{}ms）{}",
                    if ok { "连通" } else { "失败" },
                    r["latency_ms"],
                    if ok { format!("· {}", r["model"]) } else { String::new() }
                );
                if ok { ToolResult::json(summary, r, ms()) } else { ToolResult::error(summary, ms()) }
            }
            Err(e) => ToolResult::error(format!("LLM 测试失败: {e}"), ms()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_presets() {
        let ps = builtin_profiles();
        assert!(ps.len() >= 10);
        assert!(ps.iter().any(|p| p.name == "deepseek" && p.base_url.contains("deepseek")));
        assert!(ps.iter().any(|p| p.name == "hunyuan"));
    }

    #[test]
    fn registry_crud() {
        let r = ProfileRegistry::new();
        r.ensure_builtin();
        assert!(r.get("deepseek").is_some());
        assert_eq!(r.active().unwrap().name, "deepseek");
        let custom = LlmProfile {
            name: "my-local".into(), driver: "openai-compatible".into(),
            base_url: "http://127.0.0.1:9999/v1".into(), model: "test".into(),
            api_key: Some("sk-1234567890".into()), temperature: None, max_tokens: None, enabled: true,
            mode: "agent".into(),
            context_window: Some(65_536), supports_tools: Some(true), supports_vision: Some(false),
            cost_per_1k_in: None, cost_per_1k_out: None,
        };
        r.upsert(custom);
        assert!(r.set_active("my-local"));
        assert_eq!(r.active().unwrap().name, "my-local");
        assert!(r.remove("my-local"));
        assert!(!r.set_active("my-local"));
    }

    #[test]
    fn redact_key() {
        let p = LlmProfile {
            name: "x".into(), driver: "openai".into(), base_url: "u".into(), model: "m".into(),
            api_key: Some("sk-abcdefgh".into()), temperature: None, max_tokens: None, enabled: true,
            mode: "agent".into(),
            context_window: None, supports_tools: None, supports_vision: None,
            cost_per_1k_in: None, cost_per_1k_out: None,
        };
        let v = p.redacted();
        let key = v["api_key"].as_str().unwrap();
        assert!(key.contains("****"));
        assert!(!key.contains("abcdefgh"));
        // 能力字段缺省值显性可见（B-8）
        assert_eq!(v["context_window"], json!(32_000));
        assert_eq!(v["supports_tools"], json!(true));
        assert_eq!(v["supports_vision"], json!(false));
    }

    #[test]
    fn component_meta() {
        let c = LlmSettingsComponent { registry: std::sync::Arc::new(ProfileRegistry::new()) };
        assert_eq!(c.id(), "llm-settings");
        assert_eq!(c.kind(), ComponentKind::Plugin);
    }
}
