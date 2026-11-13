//! #321: the text AI tasks and where each one goes.
//!
//! Every text task (the EN→SK translation, the register check, the
//! metadata, the description extraction, the scraped-lyrics cleanup, the
//! Spotify resolution) has two settings, read live at every call by
//! sp-server's `ai::route`:
//!
//! - `ai_provider_<task>`: `claude_proxy` (unset or blank = the default:
//!   Claude through CLIProxyAPI, the behaviour before #321) or `openrouter`
//!   (OpenRouter's OpenAI-compatible chat API with `openrouter_api_key`);
//! - `ai_model_<task>`: the OpenRouter model of that task; blank =
//!   [`DEFAULT_OPENROUTER_MODEL`]. It applies to the OpenRouter path only:
//!   the proxy keeps `ai_model`, so an id the proxy does not list never
//!   reaches it.
//!
//! The owner's ruling (#232, 13.11.2026): the text tasks move to OpenRouter,
//! the model a setting per task, default the newest Gemini; the default
//! provider flips only after a blind translation A/B. WASM-safe: the
//! dashboard names the same tasks and checks the same values.

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

    /// The task's provider setting, `ai_provider_<task>`.
    pub fn provider_setting(self) -> String {
        format!("ai_provider_{}", self.as_str())
    }

    /// The task's OpenRouter model setting, `ai_model_<task>`.
    pub fn model_setting(self) -> String {
        format!("ai_model_{}", self.as_str())
    }

    /// The task whose provider setting is `key`, if any.
    pub fn of_provider_setting(key: &str) -> Option<AiTask> {
        Self::ALL.into_iter().find(|t| t.provider_setting() == key)
    }
}

/// Where a task's calls go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiProvider {
    /// Claude through CLIProxyAPI: the behaviour before #321.
    #[default]
    ClaudeProxy,
    /// OpenRouter's chat API.
    OpenRouter,
}

impl AiProvider {
    /// The setting's word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeProxy => "claude_proxy",
            Self::OpenRouter => "openrouter",
        }
    }

    /// A stored value: unset, blank or `claude_proxy` = the proxy,
    /// `openrouter` = OpenRouter (trimmed, any case); anything else is
    /// `None` (a settings PATCH refuses it, see [`checked_provider`]).
    pub fn parse(raw: Option<&str>) -> Option<AiProvider> {
        let v = raw.unwrap_or("").trim().to_ascii_lowercase();
        match v.as_str() {
            "" | "claude_proxy" => Some(Self::ClaudeProxy),
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    /// A stored value read for routing: one that does not parse (a hand
    /// edit) stays on the proxy, the behaviour before #321.
    pub fn read(raw: Option<&str>) -> AiProvider {
        Self::parse(raw).unwrap_or_default()
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

/// A settings PATCH value for `key`: a provider setting must parse
/// ([`AiProvider::parse`]) and is stored as its canonical word (`""` stays
/// `""`); a model setting is stored trimmed; any other key passes as it is.
pub fn checked_provider(key: &str, value: &str) -> Result<String, String> {
    if AiTask::of_provider_setting(key).is_some() {
        if value.trim().is_empty() {
            return Ok(String::new());
        }
        return match AiProvider::parse(Some(value)) {
            Some(p) => Ok(p.as_str().to_string()),
            None => Err(format!("{key} must be claude_proxy or openrouter")),
        };
    }
    if AiTask::ALL.into_iter().any(|t| t.model_setting() == key) {
        return Ok(value.trim().to_string());
    }
    Ok(value.to_string())
}

/// What Nastavenia's "Textové AI" section shows for the stored settings
/// `stored(key)`: per task its provider as the server reads it (a blank or
/// hand-edited value = `claude_proxy`) and its model as stored (blank =
/// the default, shown as the field's placeholder).
pub fn form_values(
    stored: impl Fn(&str) -> Option<String>,
) -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    for task in AiTask::ALL {
        let provider = AiProvider::read(stored(&task.provider_setting()).as_deref());
        values.insert(task.provider_setting(), provider.as_str().to_string());
        let model = stored(&task.model_setting()).unwrap_or_default();
        values.insert(task.model_setting(), model.trim().to_string());
    }
    values
}

/// What a Nastavenia save sends of the section: only each value that
/// differs from what the page loaded (`form_values` of the loaded
/// settings), trimmed — a tab opened before a change elsewhere never puts
/// the old value back, and an untouched blank provider stays unset.
pub fn settings_to_send(
    loaded: &std::collections::HashMap<String, String>,
    edited: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for task in AiTask::ALL {
        for key in [task.provider_setting(), task.model_setting()] {
            let before = loaded.get(&key).map(|v| v.trim()).unwrap_or("");
            let after = edited.get(&key).map(|v| v.trim()).unwrap_or(before);
            if after != before {
                out.push((key, after.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "ai_tasks_tests.rs"]
mod tests;
