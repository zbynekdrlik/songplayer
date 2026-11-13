use super::{
    AiProvider, AiTask, DEFAULT_OPENROUTER_MODEL, checked_provider, openrouter_model, to_send,
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
    assert_eq!(checked_provider("ai_model", "  keep  "), Ok("  keep  ".into()));
}

#[test]
fn a_save_sends_only_a_changed_value() {
    assert_eq!(to_send("openrouter", "openrouter"), None);
    assert_eq!(to_send("a", " a "), None);
    assert_eq!(to_send("", "openrouter"), Some("openrouter".to_string()));
    assert_eq!(to_send("x", ""), Some(String::new()));
}
