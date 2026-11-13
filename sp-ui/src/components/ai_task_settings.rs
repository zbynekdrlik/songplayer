//! #321: Nastavenia "Textové AI (OpenRouter)" — per text AI task (the
//! translation, the register check, the metadata, the description, the
//! cleanup, the Spotify search) its OpenRouter model (blank = the newest
//! Gemini, `sp_core::ai_tasks::DEFAULT_OPENROUTER_MODEL`). Every task goes
//! to OpenRouter: the Claude proxy is deleted.
//!
//! The settings form owns the values (`values`: setting name → the shown
//! value, `sp_core::ai_tasks::form_values`) and saves them with its own
//! Save, each only when changed (`settings_to_send`). The server reads them
//! live at every call: a change needs no restart.

use std::collections::HashMap;

use leptos::prelude::*;
use sp_core::ai_tasks::{AiTask, DEFAULT_OPENROUTER_MODEL};

/// One task's row: its model field.
fn task_row(task: AiTask, values: RwSignal<HashMap<String, String>>) -> impl IntoView {
    let model_key = task.model_setting();
    let model = {
        let key = model_key.clone();
        move || values.with(|v| v.get(&key).cloned().unwrap_or_default())
    };
    let on_model = move |ev| {
        let value = event_target_value(&ev);
        values.update(|v| {
            v.insert(model_key.clone(), value);
        })
    };
    let id = task.as_str();
    view! {
        <div class="ai-task-row" data-testid=format!("settings-ai-task-{id}")>
            <label title="Model na OpenRouter (napr. google/gemini-4-carbon). Prázdne = najnovší Gemini.">
                {task.label_sk()}
                <input
                    type="text"
                    autocomplete="off"
                    data-testid=format!("settings-ai-model-{id}")
                    placeholder=DEFAULT_OPENROUTER_MODEL
                    prop:value=model
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
                "Model OpenRouter pre každú textovú úlohu AI. Platí od ďalšieho volania, bez reštartu. Používa kľúč openrouter_api_key a beží len so zapnutým plateným AI."
            </span>
            {AiTask::ALL.into_iter().map(|task| task_row(task, values)).collect_view()}
        </fieldset>
    }
}
