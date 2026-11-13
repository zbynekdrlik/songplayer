//! #321: the text AI tasks and their models.
//!
//! Every text task (the EN→SK translation, the register check, the
//! metadata, the description extraction, the scraped-lyrics cleanup, the
//! Spotify resolution) goes to OpenRouter's OpenAI-compatible chat API with
//! `openrouter_api_key`, and has one setting, read live at every call by
//! sp-server's `ai::client::route`:
//!
//! - `ai_model_<task>`: the OpenRouter model of that task; blank =
//!   [`DEFAULT_OPENROUTER_MODEL`].
//!
//! The owner's ruling (#232, 13.11.2026) and the blind translation A/B
//! (#321 comment 6856550942): every text task on OpenRouter, default the
//! newest Gemini; the Claude-through-CLIProxyAPI path is deleted (an old
//! `ai_provider_<task>` row is no longer read). WASM-safe: the dashboard
//! names the same tasks and checks the same values.

/// The OpenRouter model a task sends when its `ai_model_<task>` is blank:
/// the newest Gemini on OpenRouter's model list (13.11.2026, #321).
pub const DEFAULT_OPENROUTER_MODEL: &str = "google/gemini-4-hydrogen";

/// OpenRouter's OpenAI-compatible API root (`{root}/chat/completions`).
pub const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1";

/// The OpenRouter key's setting (a secret: masked by its `_key` suffix).
pub const SETTING_OPENROUTER_API_KEY: &str = "openrouter_api_key";

/// The text AI tasks (#321), in the order Nastavenia lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiTask {
    Translate,
    Register,
    Metadata,
    Description,
    Cleanup,
    Spotify,
}

impl AiTask {
    /// Every task, in Nastavenia's order.
    pub const ALL: [AiTask; 6] = [
        AiTask::Translate,
        AiTask::Register,
        AiTask::Metadata,
        AiTask::Description,
        AiTask::Cleanup,
        AiTask::Spotify,
    ];

    /// The task's word in its setting names and the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Translate => "translate",
            Self::Register => "register",
            Self::Metadata => "metadata",
            Self::Description => "description",
            Self::Cleanup => "cleanup",
            Self::Spotify => "spotify",
        }
    }

    /// Nastavenia's label.
    pub fn label_sk(self) -> &'static str {
        match self {
            Self::Translate => "Preklad EN → SK",
            Self::Register => "Kontrola textu (register)",
            Self::Metadata => "Názov a interpret",
            Self::Description => "Text z popisu videa",
            Self::Cleanup => "Čistenie stiahnutého textu",
            Self::Spotify => "Hľadanie na Spotify",
        }
    }

    /// The task's OpenRouter model setting, `ai_model_<task>`.
    pub fn model_setting(self) -> String {
        format!("ai_model_{}", self.as_str())
    }
}

/// The model a task sends on OpenRouter: its trimmed setting, else
/// [`DEFAULT_OPENROUTER_MODEL`].
pub fn openrouter_model(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => DEFAULT_OPENROUTER_MODEL.to_string(),
    }
}

/// A settings PATCH value for `key`: a task's model setting is stored
/// trimmed; any other key passes as it is.
pub fn checked_setting(key: &str, value: &str) -> String {
    if AiTask::ALL.into_iter().any(|t| t.model_setting() == key) {
        return value.trim().to_string();
    }
    value.to_string()
}

/// What Nastavenia's "Textové AI (OpenRouter)" section shows for the stored
/// settings `stored(key)`: per task its model as stored, trimmed (blank =
/// the default, shown as the field's placeholder).
pub fn form_values(
    stored: impl Fn(&str) -> Option<String>,
) -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    for task in AiTask::ALL {
        let model = stored(&task.model_setting()).unwrap_or_default();
        values.insert(task.model_setting(), model.trim().to_string());
    }
    values
}

/// What a Nastavenia save sends of the section: only each model that
/// differs from what the page loaded (`form_values` of the loaded
/// settings), trimmed — a tab opened before a change elsewhere never puts
/// the old value back.
pub fn settings_to_send(
    loaded: &std::collections::HashMap<String, String>,
    edited: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for task in AiTask::ALL {
        let key = task.model_setting();
        let before = loaded.get(&key).map(|v| v.trim()).unwrap_or("");
        let after = edited.get(&key).map(|v| v.trim()).unwrap_or(before);
        if after != before {
            out.push((key, after.to_string()));
        }
    }
    out
}

#[cfg(test)]
#[path = "ai_tasks_tests.rs"]
mod tests;
