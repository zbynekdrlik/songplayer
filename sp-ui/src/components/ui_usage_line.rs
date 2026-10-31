//! #256: Nastavenia's one line about the usage log — how many events this
//! node keeps over how many days (`sp_core::ui_usage::settings_line`), and
//! a link to the whole summary as JSON (`GET /api/v1/ui-usage/summary`,
//! every control's count, the days it was used, its last use and its uses
//! while a playlist was on air).

use leptos::prelude::*;
use sp_core::ui_usage::{self, UsageSummary};

#[component]
pub fn UiUsageLine() -> impl IntoView {
    let line = RwSignal::new(ui_usage::LINE_LOADING.to_string());
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            let text = match crate::api::get::<UsageSummary>(ui_usage::SUMMARY_LINK).await {
                Ok(summary) => ui_usage::settings_line(&summary),
                Err(_) => ui_usage::LINE_UNAVAILABLE.to_string(),
            };
            // The page may be gone by now (a navigation): nothing to show then.
            let _ = line.try_set(text);
        });
    });
    view! {
        <p class="ui-usage-line">
            <span data-testid="ui-usage-line">{move || line.get()}</span>
            " · "
            <a
                data-testid="ui-usage-summary-link"
                href=ui_usage::SUMMARY_LINK
                target="_blank"
                rel="noopener"
            >
                "súhrn (JSON)"
            </a>
        </p>
    }
}
