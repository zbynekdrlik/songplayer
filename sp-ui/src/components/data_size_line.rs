//! #261: Nastavenia's one line about the data dir's size, read once from
//! `GET /api/v1/status` (`sp_core::data_size::DataSize`, measured by the
//! server's daily cache sweep): `Dáta na disku: 123,4 GB (cache 120,0 GB),
//! merané 17.10. 14:05`, the time in the browser's own zone. Not on the
//! HealthBar: it is read rarely.

use leptos::prelude::*;
use sp_core::data_size::{self, DataSize};

use crate::components::audio_faults::local;

#[component]
pub fn DataSizeLine() -> impl IntoView {
    let line = RwSignal::new(data_size::LINE_LOADING.to_string());
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            let text = match crate::api::get::<DataSize>("/api/v1/status").await {
                Ok(size) => data_size::settings_line(&size, size.data_dir_measured_ms.map(local)),
                Err(_) => data_size::LINE_UNAVAILABLE.to_string(),
            };
            // The page may be gone by now (a navigation): nothing to show then.
            let _ = line.try_set(text);
        });
    });
    view! {
        <p class="ui-usage-line">
            <span data-testid="data-size-line">{move || line.get()}</span>
        </p>
    }
}
