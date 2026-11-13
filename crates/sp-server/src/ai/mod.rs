//! AI infrastructure: CLIProxyAPI proxy manager + OpenAI-compatible client.

pub mod client;
pub mod openrouter; // #321: the text-LLM client over OpenRouter's chat API
pub mod openrouter_probe; // #321: the OpenRouter path's live gate
pub mod proxy;
pub mod retry; // #145: the waits of a refused call, against the proxy's cooldown
pub mod translate_ab; // #321: the blind A/B of the translation

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiSettings {
    pub api_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub system_prompt_extra: Option<String>,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            api_url: "http://localhost:18787/v1".into(),
            api_key: None,
            model: sp_core::config::DEFAULT_AI_MODEL.into(),
            system_prompt_extra: None,
        }
    }
}
