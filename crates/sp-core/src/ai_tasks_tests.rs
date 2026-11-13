use std::collections::HashMap;

use super::{
    AiProvider, AiTask, DEFAULT_OPENROUTER_MODEL, checked_provider, form_values, openrouter_model,
    settings_to_send,
};

#[test]
fn each_task_has_its_own_two_settings() {
    let names: Vec<(String, String)> = AiTask::ALL
        .into_iter()
        .map(|t| (t.provider_setting(), t.model_setting()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("ai_provider_translate".into(), "ai_model_translate".into()),
            ("ai_provider_register".into(), "ai_model_register".into()),
            ("ai_provider_metadata".into(), "ai_model_metadata".into()),
            (
                "ai_provider_description".into(),
                "ai_model_description".into()
            ),
            ("ai_provider_cleanup".into(), "ai_model_cleanup".into()),
            ("ai_provider_spotify".into(), "ai_model_spotify".into()),
        ]
    );
}

#[test]
fn every_task_has_a_slovak_label_of_its_own() {
    let labels: Vec<&str> = AiTask::ALL.into_iter().map(AiTask::label_sk).collect();
    assert_eq!(
        labels,
        vec![
            "Preklad EN → SK",
            "Kontrola textu (register)",
            "Názov a interpret",
            "Text z popisu videa",
            "Čistenie stiahnutého textu",
            "Hľadanie na Spotify",
        ]
    );
}

#[test]
fn a_provider_setting_names_its_task_and_nothing_else_does() {
    for task in AiTask::ALL {
        assert_eq!(
            AiTask::of_provider_setting(&task.provider_setting()),
            Some(task)
        );
        assert_eq!(AiTask::of_provider_setting(&task.model_setting()), None);
    }
    assert_eq!(AiTask::of_provider_setting("ai_model"), None);
}

#[test]
fn unset_or_blank_is_the_proxy_and_openrouter_is_named() {
    assert_eq!(AiProvider::parse(None), Some(AiProvider::ClaudeProxy));
    assert_eq!(AiProvider::parse(Some("  ")), Some(AiProvider::ClaudeProxy));
    assert_eq!(
        AiProvider::parse(Some("claude_proxy")),
        Some(AiProvider::ClaudeProxy)
    );
    assert_eq!(
        AiProvider::parse(Some(" OpenRouter ")),
        Some(AiProvider::OpenRouter)
    );
    assert_eq!(AiProvider::parse(Some("gemini")), None);
    assert_eq!(AiProvider::default(), AiProvider::ClaudeProxy);
}

#[test]
fn an_unknown_stored_provider_routes_to_the_proxy() {
    assert_eq!(AiProvider::read(Some("gemini")), AiProvider::ClaudeProxy);
    assert_eq!(AiProvider::read(Some("openrouter")), AiProvider::OpenRouter);
    assert_eq!(AiProvider::OpenRouter.as_str(), "openrouter");
    assert_eq!(AiProvider::ClaudeProxy.as_str(), "claude_proxy");
}

#[test]
fn a_blank_model_is_the_newest_gemini() {
    assert_eq!(DEFAULT_OPENROUTER_MODEL, "google/gemini-4-hydrogen");
    assert_eq!(openrouter_model(None), DEFAULT_OPENROUTER_MODEL);
    assert_eq!(openrouter_model(Some(" ")), DEFAULT_OPENROUTER_MODEL);
    assert_eq!(
        openrouter_model(Some(" google/gemini-4-carbon ")),
        "google/gemini-4-carbon"
    );
}

#[test]
fn the_patch_takes_only_a_known_provider_in_its_canonical_word() {
    assert_eq!(
        checked_provider("ai_provider_translate", " OPENROUTER "),
        Ok("openrouter".to_string())
    );
    assert_eq!(
        checked_provider("ai_provider_cleanup", "  "),
        Ok(String::new())
    );
    let err = checked_provider("ai_provider_metadata", "gemini").unwrap_err();
    assert_eq!(
        err,
        "ai_provider_metadata must be claude_proxy or openrouter"
    );
    assert_eq!(
        checked_provider("ai_model_register", "  x/y  "),
        Ok("x/y".to_string())
    );
    assert_eq!(
        checked_provider("ai_model", "  keep  "),
        Ok("  keep  ".into())
    );
}

fn stored(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn the_form_shows_each_provider_as_the_server_reads_it() {
    let values = form_values(stored(&[
        ("ai_provider_translate", "OpenRouter "),
        ("ai_provider_register", "nonsense"),
        ("ai_model_translate", " google/x "),
    ]));
    assert_eq!(values.len(), 12);
    assert_eq!(values["ai_provider_translate"], "openrouter");
    // A hand-edited value routes to the proxy, so the form says so.
    assert_eq!(values["ai_provider_register"], "claude_proxy");
    assert_eq!(values["ai_provider_spotify"], "claude_proxy");
    assert_eq!(values["ai_model_translate"], "google/x");
    assert_eq!(values["ai_model_cleanup"], "");
}

#[test]
fn a_save_sends_only_a_changed_value() {
    let loaded = form_values(stored(&[("ai_model_metadata", "m")]));
    // Untouched: nothing — a blank provider stays unset (never written as
    // `claude_proxy` by a save that changed nothing).
    assert!(settings_to_send(&loaded, &loaded).is_empty());
    let mut edited = loaded.clone();
    edited.insert("ai_provider_translate".into(), "openrouter".into());
    edited.insert(
        "ai_model_translate".into(),
        " google/gemini-4-carbon ".into(),
    );
    edited.insert("ai_model_metadata".into(), " m ".into());
    edited.insert("ai_model_spotify".into(), "".into());
    let mut sent = settings_to_send(&loaded, &edited);
    sent.sort();
    assert_eq!(
        sent,
        vec![
            (
                "ai_model_translate".to_string(),
                "google/gemini-4-carbon".to_string()
            ),
            (
                "ai_provider_translate".to_string(),
                "openrouter".to_string()
            ),
        ]
    );
    // A model cleared back to blank is sent as blank (= the default).
    let mut cleared = loaded.clone();
    cleared.insert("ai_model_metadata".into(), "  ".into());
    assert_eq!(
        settings_to_send(&loaded, &cleared),
        vec![("ai_model_metadata".to_string(), String::new())]
    );
    // A key missing from the edit is the loaded value: nothing sent.
    assert!(settings_to_send(&loaded, &HashMap::new()).is_empty());
}
