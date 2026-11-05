//! The low-latency switch of Nastavenia "Zvukové výstupy" (#289): one
//! checkbox, saved with the section's "Uložiť výstupy", sent only when it
//! differs from the value the page loaded (`low_latency_to_send`).

use std::collections::HashMap;

use leptos::prelude::*;
use sp_core::audio_latency::{
    LABEL_SK, SETTING_AUDIO_LOW_LATENCY, TOOLTIP_SK, low_latency, low_latency_to_send,
};

use crate::store::DashboardStore;

/// The switch's state: the stored value (only its own key, a `Memo`) and the
/// checkbox.
#[derive(Clone, Copy)]
pub(super) struct LowLatency {
    stored: Memo<Option<String>>,
    checked: RwSignal<bool>,
}

impl LowLatency {
    /// Read from `store.settings`; the checkbox follows the stored value
    /// whenever it changes (a load, this section's save).
    pub(super) fn new(store: DashboardStore) -> Self {
        let stored = Memo::new(move |_| {
            store
                .settings
                .with(|s| s.get(SETTING_AUDIO_LOW_LATENCY).cloned())
        });
        let checked = RwSignal::new(false);
        Effect::new(move |_| {
            checked.set(stored.with(|v| low_latency(v.as_deref())));
        });
        Self { stored, checked }
    }

    /// Whether the checkbox still shows the stored value ("Uložené" shows
    /// only then).
    pub(super) fn unchanged(self) -> bool {
        self.checked.get() == self.stored.with(|v| low_latency(v.as_deref()))
    }

    /// What the save sends for the switch, read as the PATCH is built.
    pub(super) fn to_send(self) -> Option<String> {
        let loaded = self.stored.get_untracked();
        low_latency_to_send(loaded.as_deref(), self.checked.get_untracked())
    }
}

/// Put the switch into the save's body, when it changed.
pub(super) fn add(body: &mut HashMap<String, String>, value: Option<String>) {
    if let Some(value) = value {
        body.insert(SETTING_AUDIO_LOW_LATENCY.to_string(), value);
    }
}

#[component]
pub(super) fn LowLatencySwitch(state: LowLatency) -> impl IntoView {
    view! {
        <label title=TOOLTIP_SK>
            <input
                type="checkbox"
                data-testid="settings-audio-low-latency"
                prop:checked=move || state.checked.get()
                on:change=move |ev| state.checked.set(event_target_checked(&ev))
            />
            {LABEL_SK}
        </label>
    }
}
