//! AI infrastructure: CLIProxyAPI proxy manager + OpenAI-compatible client.

pub mod proxy;

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
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        }
    }
}
