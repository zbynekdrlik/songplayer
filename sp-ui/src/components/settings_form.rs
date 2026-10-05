//! Settings form for OBS, Gemini, dub, VBAN (#210), the NDI input "OBS manuál"
//! (#212), the Companion remote control (#213), the program transition (#215;
//! #221 L5 deleted the OBS follow and the "podľa OBS" transition) and cache
//! configuration.

use std::collections::HashMap;

use leptos::prelude::*;
use sp_core::config;

use crate::api;
use crate::store::DashboardStore;

/// Helper: find a setting value or return a default.
fn setting_value(settings: &HashMap<String, String>, key: &str, default: &str) -> String {
    settings
        .get(key)
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// The dub voices: `speaker` (#184 round H step 2, the default — the speaker's
/// own voice, no pinned prebuilt voice) followed by the six catalogue voices
/// (#184 round C): the stored value with its Slovak descriptive label. Mirrors
/// the vetted catalogue `eval/dubbing/voices.py`.
const DUB_VOICES: &[(&str, &str)] = &[
    (config::DUB_VOICE_SPEAKER, "Hlas rečníka (odporúčané)"),
    ("Charon", "Charon — muž, vecný"),
    ("Orus", "Orus — muž, pevný"),
    ("Puck", "Puck — muž, energický"),
    ("Kore", "Kore — žena, pevná"),
    ("Aoede", "Aoede — žena, ľahká"),
    ("Leda", "Leda — žena, mladá"),
];

/// #215: the `program_transition` choices (the stored value) with their Slovak
/// labels. #221 L5: a fade is the default (the server fades when nothing, or
/// the retired `obs`, is stored).
const PROGRAM_TRANSITIONS: &[(&str, &str)] =
    &[("fade", "Prelínanie (odporúčané)"), ("cut", "Strih")];

/// #221 L5: the choice a stored `program_transition` means, as the server
/// reads it (`TransitionMode::parse`): `cut` is a cut, anything else the
/// default fade.
fn transition_choice(stored: &str) -> String {
    let choice = if stored.trim() == "cut" {
        "cut"
    } else {
        "fade"
    };
    choice.to_string()
}

/// #215: the fade length the server uses for the stored `program_transition_ms`
/// (`sp_core::config::program_transition_ms`, at most 10 s), so the field never
/// shows a value its `min` / `max` would refuse on save.
fn effective_transition_ms(stored: &str) -> String {
    config::program_transition_ms(Some(stored))
        .min(config::MAX_PROGRAM_TRANSITION_MS)
        .to_string()
}

#[component]
pub fn SettingsForm() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    let obs_url = RwSignal::new(String::new());
    let obs_password = RwSignal::new(String::new());
    let gemini_key = RwSignal::new(String::new());
    let gemini_model = RwSignal::new(String::new());
    let cache_dir = RwSignal::new(String::new());
    let dub_voice = RwSignal::new(config::DEFAULT_DUB_VOICE.to_string());
    let dub_model = RwSignal::new(config::DEFAULT_DUB_MODEL.to_string());
    // #210: the program's VBAN audio output (off by default).
    let vban_enabled = RwSignal::new(false);
    let vban_stream_name = RwSignal::new(config::DEFAULT_VBAN_STREAM_NAME.to_string());
    let vban_targets = RwSignal::new(String::new());
    // #212: the NDI input "OBS manuál" (off by default, no source).
    let ndi_input_enabled = RwSignal::new(false);
    let ndi_input_source = RwSignal::new(String::new());
    // #213: the Companion remote control (off by default, port 4456, no password).
    let remote_enabled = RwSignal::new(false);
    let remote_port = RwSignal::new(config::DEFAULT_REMOTE_WS_PORT.to_string());
    let remote_password = RwSignal::new(String::new());
    // #215: the program transition (a fade by default).
    let transition_mode = RwSignal::new("fade".to_string());
    let transition_ms = RwSignal::new(config::DEFAULT_PROGRAM_TRANSITION_MS.to_string());
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
        dub_voice.set(setting_value(
            &settings,
            config::SETTING_DUB_VOICE,
            config::DEFAULT_DUB_VOICE,
        ));
        dub_model.set(setting_value(
            &settings,
            config::SETTING_DUB_MODEL,
            config::DEFAULT_DUB_MODEL,
        ));
        vban_enabled.set(setting_value(&settings, config::SETTING_VBAN_ENABLED, "false") == "true");
        vban_stream_name.set(setting_value(
            &settings,
            config::SETTING_VBAN_STREAM_NAME,
            config::DEFAULT_VBAN_STREAM_NAME,
        ));
        vban_targets.set(setting_value(&settings, config::SETTING_VBAN_TARGETS, ""));
        ndi_input_enabled
            .set(setting_value(&settings, config::SETTING_NDI_INPUT_ENABLED, "false") == "true");
        ndi_input_source.set(setting_value(
            &settings,
            config::SETTING_NDI_INPUT_SOURCE,
            "",
        ));
        remote_enabled
            .set(setting_value(&settings, config::SETTING_REMOTE_WS_ENABLED, "false") == "true");
        remote_port.set(setting_value(
            &settings,
            config::SETTING_REMOTE_WS_PORT,
            &config::DEFAULT_REMOTE_WS_PORT.to_string(),
        ));
        remote_password.set(setting_value(
            &settings,
            config::SETTING_REMOTE_WS_PASSWORD,
            "",
        ));
        transition_mode.set(transition_choice(&setting_value(
            &settings,
            config::SETTING_PROGRAM_TRANSITION,
            "",
        )));
        transition_ms.set(effective_transition_ms(&setting_value(
            &settings,
            config::SETTING_PROGRAM_TRANSITION_MS,
            "",
        )));
    });

    let on_save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let mut settings = HashMap::new();
        settings.insert(
            config::SETTING_OBS_WEBSOCKET_URL.to_string(),
            obs_url.get(),
        );
        settings.insert(
            config::SETTING_OBS_WEBSOCKET_PASSWORD.to_string(),
            obs_password.get(),
        );
        settings.insert(
            config::SETTING_GEMINI_API_KEY.to_string(),
            gemini_key.get(),
        );
        settings.insert(
            config::SETTING_GEMINI_MODEL.to_string(),
            gemini_model.get(),
        );
        settings.insert(config::SETTING_CACHE_DIR.to_string(), cache_dir.get());
        settings.insert(config::SETTING_DUB_VOICE.to_string(), dub_voice.get());
        settings.insert(config::SETTING_DUB_MODEL.to_string(), dub_model.get());
        settings.insert(
            config::SETTING_VBAN_ENABLED.to_string(),
            vban_enabled.get().to_string(),
        );
        settings.insert(
            config::SETTING_VBAN_STREAM_NAME.to_string(),
            vban_stream_name.get(),
        );
        settings.insert(config::SETTING_VBAN_TARGETS.to_string(), vban_targets.get());
        settings.insert(
            config::SETTING_NDI_INPUT_ENABLED.to_string(),
            ndi_input_enabled.get().to_string(),
        );
        settings.insert(
            config::SETTING_NDI_INPUT_SOURCE.to_string(),
            ndi_input_source.get().trim().to_string(),
        );
        settings.insert(
            config::SETTING_REMOTE_WS_ENABLED.to_string(),
            remote_enabled.get().to_string(),
        );
        settings.insert(
            config::SETTING_REMOTE_WS_PORT.to_string(),
            remote_port.get().trim().to_string(),
        );
        settings.insert(
            config::SETTING_REMOTE_WS_PASSWORD.to_string(),
            remote_password.get(),
        );
        settings.insert(
            config::SETTING_PROGRAM_TRANSITION.to_string(),
            transition_mode.get(),
        );
        settings.insert(
            config::SETTING_PROGRAM_TRANSITION_MS.to_string(),
            transition_ms.get().trim().to_string(),
        );

        leptos::task::spawn_local(async move {
            save_status.set("Ukladám…".into());
            match api::patch_json::<HashMap<String, String>, HashMap<String, String>>(
                "/api/v1/settings",
                &settings,
            )
            .await
            {
                Ok(_) => {
                    save_status.set("Uložené".into());
                    store.settings.set(settings);
                }
                Err(_) => {
                    save_status.set("Chyba pri ukladaní".into());
                }
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
                    "Heslo"
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
                    "API kľúč"
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
                        data-testid="settings-gemini-model"
                        prop:value=move || gemini_model.get()
                        on:input=move |ev| gemini_model.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset>
                <legend>"Dabing"</legend>
                <label>
                    "Hlas dabingu"
                    <select
                        data-testid="settings-dub-voice"
                        prop:value=move || dub_voice.get()
                        on:change=move |ev| dub_voice.set(event_target_value(&ev))
                    >
                        {DUB_VOICES
                            .iter()
                            .map(|(value, label)| {
                                view! { <option value=*value>{*label}</option> }
                            })
                            .collect_view()}
                    </select>
                </label>
                <label>
                    "Model dabingu"
                    <input
                        type="text"
                        data-testid="settings-dub-model"
                        prop:value=move || dub_model.get()
                        on:input=move |ev| dub_model.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset data-testid="settings-vban">
                <legend>"Zvuk programu cez VBAN"</legend>
                <label>
                    <input
                        type="checkbox"
                        data-testid="settings-vban-enabled"
                        prop:checked=move || vban_enabled.get()
                        on:change=move |ev| vban_enabled.set(event_target_checked(&ev))
                    />
                    "Posielať zvuk programu (VBAN)"
                </label>
                <label>
                    "Názov streamu"
                    <input
                        type="text"
                        maxlength="16"
                        data-testid="settings-vban-stream-name"
                        prop:value=move || vban_stream_name.get()
                        on:input=move |ev| vban_stream_name.set(event_target_value(&ev))
                    />
                </label>
                <label>
                    "Ciele (host:port, oddelené čiarkou)"
                    <input
                        type="text"
                        data-testid="settings-vban-targets"
                        placeholder="dev1.lan:6980"
                        prop:value=move || vban_targets.get()
                        on:input=move |ev| vban_targets.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset data-testid="settings-ndi-input">
                <legend>"Vstup NDI „OBS manuál“"</legend>
                <label>
                    <input
                        type="checkbox"
                        data-testid="settings-ndi-input-enabled"
                        prop:checked=move || ndi_input_enabled.get()
                        on:change=move |ev| ndi_input_enabled.set(event_target_checked(&ev))
                    />
                    "Prijímať zdroj NDI a ponúknuť ho na program"
                </label>
                <label>
                    "Zdroj NDI (STROJ (stream))"
                    <input
                        type="text"
                        data-testid="settings-ndi-input-source"
                        placeholder="CG-OBS (manual)"
                        prop:value=move || ndi_input_source.get()
                        on:input=move |ev| ndi_input_source.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset data-testid="settings-remote">
                <legend>"Diaľkové ovládanie (Companion)"</legend>
                <label>
                    <input
                        type="checkbox"
                        data-testid="settings-remote-enabled"
                        prop:checked=move || remote_enabled.get()
                        on:change=move |ev| remote_enabled.set(event_target_checked(&ev))
                    />
                    "Ovládať program zo Stream Decku cez obs-websocket (Companion)"
                </label>
                <label>
                    "Port"
                    <input
                        type="number"
                        min="1"
                        max="65535"
                        data-testid="settings-remote-port"
                        prop:value=move || remote_port.get()
                        on:input=move |ev| remote_port.set(event_target_value(&ev))
                    />
                </label>
                <label>
                    "Heslo (nepovinné)"
                    <input
                        type="password"
                        data-testid="settings-remote-password"
                        prop:value=move || remote_password.get()
                        on:input=move |ev| remote_password.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset data-testid="settings-program-transition">
                <legend>"Prechody programu (SP-program)"</legend>
                <label>
                    "Prechod"
                    <select
                        data-testid="settings-program-transition-kind"
                        prop:value=move || transition_mode.get()
                        on:change=move |ev| transition_mode.set(event_target_value(&ev))
                    >
                        {PROGRAM_TRANSITIONS
                            .iter()
                            .map(|(value, label)| {
                                view! { <option value=*value>{*label}</option> }
                            })
                            .collect_view()}
                    </select>
                </label>
                <label>
                    "Dĺžka prelínania (ms)"
                    <input
                        type="number"
                        min="1"
                        max=config::MAX_PROGRAM_TRANSITION_MS.to_string()
                        data-testid="settings-program-transition-ms"
                        prop:value=move || transition_ms.get()
                        on:input=move |ev| transition_ms.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <fieldset>
                <legend>"Vyrovnávacia pamäť"</legend>
                <label>
                    "Priečinok"
                    <input
                        type="text"
                        prop:value=move || cache_dir.get()
                        on:input=move |ev| cache_dir.set(event_target_value(&ev))
                    />
                </label>
            </fieldset>

            <div class="form-actions">
                <button type="submit">"Uložiť nastavenia"</button>
                <span class="save-status">{move || save_status.get()}</span>
            </div>
        </form>
    }
}
