//! Embedding（文本 → 向量）调用：知识向量库的唯一向量来源。
//!
//! 为什么不用本地哈希"假装"向量：那种向量没有语义，检索结果看起来能用、
//! 实际召回的全是字面巧合，比直接报错更坏。所以**没有可用模型时一律报错**，
//! 让用户知道"向量库现在用不了"，而不是给一堆假结果。
//!
//! 支持范围：
//! - OpenAI 兼容（`/embeddings`）：openai / deepseek / qwen(dashscope 兼容) /
//!   zhipu / siliconflow / ollama 的 `/v1` 等；
//! - Gemini 原生 `batchEmbedContents`；
//! - Anthropic 没有 embedding 接口 ⇒ 明确报错。

use serde_json::{json, Value};

use crate::profiles::LlmProfile;

/// 单次请求最多带多少条文本（避免超长 body 被网关拒）。
const BATCH: usize = 32;

/// 把一批文本转成向量。返回顺序与 `texts` 一一对应。
pub async fn embed_texts(profile: &LlmProfile, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if profile.base_url.trim().is_empty() {
        return Err("当前模型档案缺 base_url，无法调用 embedding 接口".into());
    }
    match profile.driver.as_str() {
        "gemini" => {
            let mut out = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(BATCH) {
                out.extend(gemini_embed(profile, chunk).await?);
            }
            Ok(out)
        }
        "anthropic" => Err(
            "Anthropic 驱动没有 embedding 接口；向量库请使用 OpenAI 兼容或 Gemini 档案".into(),
        ),
        _ => {
            let mut out = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(BATCH) {
                out.extend(openai_embed(profile, chunk).await?);
            }
            Ok(out)
        }
    }
}

/// 该档案是否可能支持 embedding（用于工具侧提前给出可读提示）。
pub fn supports_embedding(profile: &LlmProfile) -> bool {
    !matches!(profile.driver.as_str(), "anthropic")
}

async fn openai_embed(profile: &LlmProfile, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let url = format!("{}/embeddings", profile.base_url.trim_end_matches('/'));
    let body = json!({ "model": profile.model, "input": texts });
    let resp = post_json(&url, body, profile.api_key.as_deref(), None).await?;
    let arr = resp
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("embedding 响应缺少 data 字段：{}", clip(&resp.to_string())))?;
    let mut out: Vec<(usize, Vec<f32>)> = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let idx = item.get("index").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(i);
        let vec = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| "embedding 条目缺少 vector".to_string())?;
        out.push((idx, to_f32s(vec)));
    }
    out.sort_by_key(|(i, _)| *i);
    if out.len() != texts.len() {
        return Err(format!(
            "embedding 回包条数不符：请求 {} 条，返回 {} 条",
            texts.len(),
            out.len()
        ));
    }
    Ok(out.into_iter().map(|(_, v)| v).collect())
}

async fn gemini_embed(profile: &LlmProfile, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let url = format!(
        "{}/models/{}:batchEmbedContents",
        profile.base_url.trim_end_matches('/'),
        profile.model
    );
    let requests: Vec<Value> = texts
        .iter()
        .map(|t| {
            json!({
                "model": format!("models/{}", profile.model),
                "content": { "parts": [{ "text": t }] }
            })
        })
        .collect();
    let body = json!({ "requests": requests });
    let key = profile.api_key.clone().unwrap_or_default();
    let resp = post_json(&url, body, None, Some(("x-goog-api-key", key))).await?;
    let arr = resp
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("embedding 响应缺少 embeddings：{}", clip(&resp.to_string())))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let vec = item
            .get("values")
            .and_then(Value::as_array)
            .ok_or_else(|| "embedding 条目缺少 values".to_string())?;
        out.push(to_f32s(vec));
    }
    if out.len() != texts.len() {
        return Err(format!(
            "embedding 回包条数不符：请求 {} 条，返回 {} 条",
            texts.len(),
            out.len()
        ));
    }
    Ok(out)
}

async fn post_json(
    url: &str,
    body: Value,
    bearer: Option<&str>,
    extra: Option<(&str, String)>,
) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;
    let mut req = client.post(url).json(&body);
    if let Some(k) = bearer.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(k);
    }
    if let Some((name, value)) = extra {
        req = req.header(name, value);
    }
    let resp = req.send().await.map_err(|e| format!("请求 embedding 接口失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("读取 embedding 响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!("embedding 接口返回 {status}：{}", clip(&text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("embedding 响应不是合法 JSON：{e}"))
}

fn to_f32s(v: &[Value]) -> Vec<f32> {
    v.iter()
        .filter_map(|x| x.as_f64().map(|f| f as f32))
        .collect()
}

fn clip(s: &str) -> String {
    let t: String = s.chars().take(300).collect();
    if s.chars().count() > 300 { format!("{t}…") } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(driver: &str) -> LlmProfile {
        LlmProfile {
            name: "t".into(),
            driver: driver.into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "m".into(),
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
    fn anthropic_is_explicitly_unsupported() {
        assert!(!supports_embedding(&profile("anthropic")));
        assert!(supports_embedding(&profile("openai")));
        assert!(supports_embedding(&profile("gemini")));
    }
}
