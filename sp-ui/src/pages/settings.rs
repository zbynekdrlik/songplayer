//! Settings page with OBS, Resolume, and Gemini configuration.

use leptos::prelude::*;
use sp_core::models::Setting;

use crate::api;
use crate::components::{resolume_hosts, settings_form};
use crate::store::DashboardStore;

#[component]
pub fn SettingsPage() -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");

    // Load settings on mount.
    let _load = Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(settings) = api::get::<Vec<Setting>>("/api/v1/settings").await {
                store.settings.set(settings);
            }
        });
    });

    view! {
        <div class="settings-page">
            <h1>"Settings"</h1>
            <settings_form::SettingsForm />
            <hr />
            <h2>"Resolume Hosts"</h2>
            <resolume_hosts::ResolumeHosts />
        </div>
    }
}
