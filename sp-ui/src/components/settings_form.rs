//! Settings form for OBS, Gemini, and cache configuration.

use leptos::prelude::*;
use sp_core::config;
use sp_core::models::Setting;

use crate::api;
use crate::store::DashboardStore;

/// Helper: find a setting value or return a default.
fn setting_value(settings: &[Setting], key: &str, default: &str) -> String {
    settings
        .iter()
        .find(|s| s.key == key)
        .map(|s| s.value.clone())
        .unwrap_or_else(|| default.to_string())
}

#[component]
pub fn SettingsForm() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    let obs_url = RwSignal::new(String::new());
    let obs_password = RwSignal::new(String::new());
    let gemini_key = RwSignal::new(String::new());
    let gemini_model = RwSignal::new(String::new());
    let cache_dir = RwSignal::new(String::new());
    let save_status = RwSignal::new(String::new());

    // Populate fields from store settings when they change.
    let _sync = Effect::new(move |_| {
        let settings = store.settings.get();
        obs_url.set(setting_value(
            &settings,
            config::SETTING_OBS_WEBSOCKET_URL,
            config::DEFAULT_OBS_WEBSOCKET_URL,
        ));
        obs_password.set(setting_value(
            &settings,
            config::SETTING_OBS_WEBSOCKET_PASSWORD,
            "",
        ));
        gemini_key.set(setting_value(&settings, config::SETTING_GEMINI_API_KEY, ""));
        gemini_model.set(setting_value(
            &settings,
            config::SETTING_GEMINI_MODEL,
            config::DEFAULT_GEMINI_MODEL,
        ));
        cache_dir.set(setting_value(
            &settings,
            config::SETTING_CACHE_DIR,
            config::DEFAULT_CACHE_DIR,
        ));
    });

    let on_save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let settings = vec![
            Setting {
                key: config::SETTING_OBS_WEBSOCKET_URL.into(),
                value: obs_url.get(),
            },
            Setting {
                key: config::SETTING_OBS_WEBSOCKET_PASSWORD.into(),
                value: obs_password.get(),
            },
            Setting {
                key: config::SETTING_GEMINI_API_KEY.into(),
                value: gemini_key.get(),
            },
            Setting {
                key: config::SETTING_GEMINI_MODEL.into(),
                value: gemini_model.get(),
            },
            Setting {
                key: config::SETTING_CACHE_DIR.into(),
                value: cache_dir.get(),
            },
        ];

        leptos::task::spawn_local(async move {
            save_status.set("Saving...".into());
            let mut ok = true;
            for s in &settings {
                if api::put_json::<Setting, Setting>(&format!("/api/v1/settings/{}", s.key), s)
                    .await
                    .is_err()
                {
                    ok = false;
                }
            }
            if ok {
                save_status.set("Saved".into());
                store.settings.set(settings);
            } else {
                save_status.set("Error saving some settings".into());
            }
        });
    };

    view! {
        <form class="settings-form" on:submit=on_save>
            <fieldset>
                <legend>"OBS WebSocket"</legend>
                <label>
                    "URL"
                    <input
                        type="text"
                        prop:value=move || obs_url.get()
                        on:input=move |ev| obs_url.set(event_target_value(&ev))
                    />
                </label>
                <label>
                    "Password"
                    <input
                        type="password"
                        prop:value=move || obs_password.get()
                        on:input=move |ev| obs_password.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset>
                <legend>"Google Gemini"</legend>
                <label>
                    "API Key"
                    <input
                        type="password"
                        prop:value=move || gemini_key.get()
                        on:input=move |ev| gemini_key.set(event_target_value(&ev))
                    />
                </label>
                <label>
                    "Model"
                    <input
                        type="text"
                        prop:value=move || gemini_model.get()
                        on:input=move |ev| gemini_model.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset>
                <legend>"Cache"</legend>
                <label>
                    "Directory"
                    <input
                        type="text"
                        prop:value=move || cache_dir.get()
                        on:input=move |ev| cache_dir.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <div class="form-actions">
                <button type="submit">"Save Settings"</button>
                <span class="save-status">{move || save_status.get()}</span>
            </div>
        </form>
    }
}
