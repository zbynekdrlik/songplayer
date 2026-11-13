use std::collections::HashMap;

use super::{
    AiTask, DEFAULT_OPENROUTER_MODEL, checked_setting, form_values, openrouter_model,
    settings_to_send,
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
    assert_eq!(checked_setting("ai_model_register", "  x/y  "), "x/y");
    assert_eq!(checked_setting("ai_model_spotify", "  "), "");
    assert_eq!(checked_setting("ai_model", "  keep  "), "  keep  ");
    assert_eq!(
        checked_setting("ai_provider_translate", " openrouter "),
        " openrouter "
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
