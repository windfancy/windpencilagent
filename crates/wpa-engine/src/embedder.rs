//! Embedding 提供者：把"当前模型档案"适配成 `data_core::vector::Embedder`。
//!
//! 为什么用共享可写槽而不是在装配时拷一份档案：模型切换（`UseModel`）只重建会话、
//! 不重新装配工具，若拷死在工具里，用户换了模型后向量库还在调旧档案——
//! 又回到"改了不生效，必须重启"。这里每次调用都读最新档案，切换即时生效。

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use data_core::vector::Embedder;

use crate::config::AppConfig;

/// 共享的 embedding 档案槽（`None` = 当前没有可用模型）。
pub struct SharedEmbedder {
    profile: RwLock<Option<base_llm::LlmProfile>>,
}

impl SharedEmbedder {
    pub fn new(cfg: &AppConfig) -> Arc<Self> {
        let me = Self { profile: RwLock::new(pick(cfg)) };
        Arc::new(me)
    }

    /// 配置变化后刷新（模型启用/切换/改 key 都走这里）。
    pub fn update_from(&self, cfg: &AppConfig) {
        if let Ok(mut slot) = self.profile.write() {
            *slot = pick(cfg);
        }
    }

    /// 当前用于 embedding 的模型名（无 ⇒ `None`）。
    pub fn model(&self) -> Option<String> {
        self.profile.read().ok().and_then(|g| g.as_ref().map(|p| p.model.clone()))
    }
}

/// 从模型链里挑第一个"能 embedding 且字段齐全"的档案。
fn pick(cfg: &AppConfig) -> Option<base_llm::LlmProfile> {
    cfg.llm
        .resolve_chain(None)
        .into_iter()
        .find(|p| base_llm::embed::supports_embedding(p))
}

#[async_trait]
impl Embedder for SharedEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let profile = self
            .profile
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| {
                "当前没有可用于 embedding 的模型档案（请在「配置 · 模型」启用一个 OpenAI 兼容或 Gemini 档案）"
                    .to_string()
            })?;
        base_llm::embed::embed_texts(&profile, texts).await
    }

    fn model(&self) -> String {
        self.model().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_has_no_embedder_profile() {
        let cfg = AppConfig::default();
        let e = SharedEmbedder::new(&cfg);
        assert!(e.model().is_none(), "默认配置没有可用模型");
    }
}
