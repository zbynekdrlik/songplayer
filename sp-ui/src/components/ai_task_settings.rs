//! #321: Nastavenia "Textové AI (OpenRouter)" — per text AI task (the
//! translation, the register check, the metadata, the description, the
//! cleanup, the Spotify search) where its calls go: Claude through the proxy
//! (the default) or OpenRouter, and the OpenRouter model (blank = the
//! newest Gemini, `sp_core::ai_tasks::DEFAULT_OPENROUTER_MODEL`).
//!
//! The settings form owns the values (`values`: setting name → the shown
//! value, `sp_core::ai_tasks::form_values`) and saves them with its own
//! Save, each only when changed (`settings_to_send`). The server reads them
//! live at every call: a switch needs no restart.

use std::collections::HashMap;

use leptos::prelude::*;
use sp_core::ai_tasks::{AiProvider, AiTask, DEFAULT_OPENROUTER_MODEL};

/// The provider choices with their Slovak labels.
const PROVIDERS: [(AiProvider, &str); 2] = [
    (AiProvider::ClaudeProxy, "Claude (cez proxy)"),
    (AiProvider::OpenRouter, "OpenRouter"),
];

/// One task's row: its provider select and its model field.
fn task_row(task: AiTask, values: RwSignal<HashMap<String, String>>) -> impl IntoView {
    let provider_key = task.provider_setting();
    let model_key = task.model_setting();
    let read = move |key: &str| values.with(|v| v.get(key).cloned().unwrap_or_default());
    let provider = {
        let key = provider_key.clone();
        move || read(&key)
    };
    let set = move |key: String, value: String| {
        values.update(|v| {
            v.insert(key, value);
        })
    };
    let options = {
        let provider = provider.clone();
        move || {
            let current = provider();
            PROVIDERS
                .iter()
                .map(|(p, label)| {
                    let value = p.as_str();
                    let selected = value == current;
                    view! { <option value=value selected=selected>{*label}</option> }
                })
                .collect_view()
        }
    };
    let on_provider = {
        let key = provider_key.clone();
        move |ev| set(key.clone(), event_target_value(&ev))
    };
    let model = {
        let key = model_key.clone();
        move || read(&key)
    };
    let on_model = {
        let key = model_key.clone();
        move |ev| set(key.clone(), event_target_value(&ev))
    };
    let off_openrouter = {
        let provider = provider.clone();
        move || provider() != AiProvider::OpenRouter.as_str()
    };
    let id = task.as_str();
    view! {
        <div class="ai-task-row" data-testid=format!("settings-ai-task-{id}")>
            <label>
                {task.label_sk()}
                <select
                    data-testid=format!("settings-ai-provider-{id}")
                    prop:value=provider
                    on:change=on_provider
                >
                    {options}
                </select>
            </label>
            <label title="Model na OpenRouter (napr. google/gemini-4-carbon). Prázdne = najnovší Gemini.">
                "Model (OpenRouter)"
                <input
                    type="text"
                    autocomplete="off"
                    data-testid=format!("settings-ai-model-{id}")
                    placeholder=DEFAULT_OPENROUTER_MODEL
                    prop:value=model
                    prop:disabled=off_openrouter
                    on:input=on_model
                />
            </label>
        </div>
    }
}

/// The fieldset.
#[component]
pub fn AiTaskSettings(values: RwSignal<HashMap<String, String>>) -> impl IntoView {
    view! {
        <fieldset data-testid="settings-ai-tasks">
            <legend>"Textové AI (OpenRouter)"</legend>
            <span class="settings-hint">
                "Kam ide každá textová úloha AI. Platí od ďalšieho volania, bez reštartu. OpenRouter používa kľúč openrouter_api_key a beží len so zapnutým plateným AI."
            </span>
            {AiTask::ALL.into_iter().map(|task| task_row(task, values)).collect_view()}
        </fieldset>
    }
}
