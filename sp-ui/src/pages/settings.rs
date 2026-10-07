//! Settings page with OBS, Resolume, and Gemini configuration.

use leptos::prelude::*;
use std::collections::HashMap;

use crate::api;
use crate::components::{audio_outputs, resolume_hosts, settings_form};
use crate::store::DashboardStore;

#[component]
pub fn SettingsPage() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Load settings on mount. #233: both sections save nothing until this
    // load landed (`loaded`): what they show before it is no stored value.
    let loaded = RwSignal::new(None::<bool>);
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match api::get::<HashMap<String, String>>("/api/v1/settings").await {
                Ok(settings) => {
                    store.settings.set(settings);
                    loaded.set(Some(true));
                }
                Err(_) => loaded.set(Some(false)),
            }
        });
    });

    view! {
        <div class="settings-page">
            <h1>"Nastavenia"</h1>
            <settings_form::SettingsForm loaded=loaded />
            <hr />
            <audio_outputs::AudioOutputs loaded=loaded />
            <hr />
            <h2>"Hostitelia Resolume"</h2>
            <resolume_hosts::ResolumeHosts />
        </div>
    }
}
