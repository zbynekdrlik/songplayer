use std::collections::HashMap;

use super::{
    AiTask, DEFAULT_OPENROUTER_MODEL, RETIRED_PROVIDER_PREFIX, RETIRED_SETTINGS, checked_setting,
    form_values, is_retired_setting, openrouter_model, settings_to_send,
};

#[test]
fn each_task_has_its_own_model_setting() {
    let names: Vec<String> = AiTask::ALL.into_iter().map(AiTask::model_setting).collect();
    assert_eq!(
        names,
        vec![
            "ai_model_translate",
            "ai_model_register",
            "ai_model_metadata",
            "ai_model_description",
            "ai_model_cleanup",
            "ai_model_spotify",
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
fn the_patch_trims_a_tasks_model_and_passes_anything_else() {
    assert_eq!(
        checked_setting("ai_model_register", "  x/y  ").unwrap(),
        "x/y"
    );
    assert_eq!(checked_setting("ai_model_spotify", "  ").unwrap(), "");
    assert_eq!(
        checked_setting("ai_models", "  keep  ").unwrap(),
        "  keep  "
    );
    assert_eq!(
        checked_setting("gemini_model", " keep ").unwrap(),
        " keep ",
        "a key that only looks like a task's model passes as sent"
    );
}

/// #321: the deleted proxy path's settings are refused, whatever the value
/// (a blank one too: it would re-create the row), naming the key, never the
/// value; a task's model and look-alike keys are not.
#[test]
fn the_patch_refuses_every_retired_text_ai_setting() {
    for key in [
        "ai_model",
        "ai_api_url",
        "ai_provider_translate",
        "ai_provider_spotify",
        "ai_provider_",
        "ai_provider_anything",
    ] {
        assert!(is_retired_setting(key), "{key}");
        for value in ["openrouter-secret-example", ""] {
            let reason = checked_setting(key, value).unwrap_err();
            assert!(
                reason.starts_with(&format!("{key} is no longer a setting")),
                "{reason}"
            );
            assert!(reason.contains("ai_model_<task>"), "{reason}");
            if !value.is_empty() {
                assert!(
                    !reason.contains(value),
                    "the value is never echoed: {reason}"
                );
            }
        }
    }
    for key in [
        "ai_model_translate",
        "ai_models",
        "ai_api_urls",
        "ai_provider",
        "xai_model",
        "openrouter_api_key",
    ] {
        assert!(!is_retired_setting(key), "{key}");
        assert!(checked_setting(key, "v").is_ok(), "{key}");
    }
    assert_eq!(RETIRED_SETTINGS, ["ai_model", "ai_api_url"]);
    assert_eq!(RETIRED_PROVIDER_PREFIX, "ai_provider_");
}

fn stored(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn the_form_shows_each_tasks_model_as_stored() {
    let values = form_values(stored(&[
        ("ai_provider_translate", "claude_proxy"),
        ("ai_model_translate", " google/x "),
    ]));
    assert_eq!(values.len(), 6);
    assert_eq!(values["ai_model_translate"], "google/x");
    assert_eq!(values["ai_model_cleanup"], "");
    assert!(!values.contains_key("ai_provider_translate"));
}

#[test]
fn a_save_sends_only_a_changed_model() {
    let loaded = form_values(stored(&[("ai_model_metadata", "m")]));
    assert!(settings_to_send(&loaded, &loaded).is_empty());
    let mut edited = loaded.clone();
    edited.insert(
        "ai_model_translate".into(),
        " google/gemini-4-carbon ".into(),
    );
    edited.insert("ai_model_metadata".into(), " m ".into());
    edited.insert("ai_model_spotify".into(), "".into());
    assert_eq!(
        settings_to_send(&loaded, &edited),
        vec![(
            "ai_model_translate".to_string(),
            "google/gemini-4-carbon".to_string()
        )]
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
