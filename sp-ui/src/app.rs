//! Root App component with tab-based navigation.

use leptos::prelude::*;

use crate::pages;
use crate::store::DashboardStore;

/// Which page is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Live,
    Settings,
    Lyrics,
}

impl Default for Page {
    fn default() -> Self {
        Self::Dashboard
    }
}

impl Page {
    /// Path segment used in the browser URL. Kept short so the address bar
    /// stays readable and so reload restores the right tab.
    pub fn as_path(self) -> &'static str {
        match self {
            Self::Dashboard => "/",
            Self::Live => "/live",
            Self::Lyrics => "/lyrics",
            Self::Settings => "/settings",
        }
    }

    /// Parse the path segment of the current URL back to a `Page`. Unknown
    /// paths fall back to the default (`Dashboard`) so an old bookmark or a
    /// shared link can never wedge the UI.
    pub fn from_path(path: &str) -> Self {
        match path {
            "/live" => Self::Live,
            "/lyrics" => Self::Lyrics,
            "/settings" => Self::Settings,
            _ => Self::Dashboard,
        }
    }
}

/// Read the current browser pathname. Returns `"/"` outside the browser
/// (e.g. during unit tests), so the result is always a valid path.
fn current_path() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_else(|| "/".to_string())
}

/// Update the browser address bar without triggering a reload. Best-effort —
/// a missing `history` API just no-ops, which is acceptable degradation.
fn push_path(path: &str) {
    if let Some(win) = web_sys::window()
        && let Ok(history) = win.history()
    {
        let _ = history.push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(path));
    }
}

#[component]
pub fn App() -> impl IntoView {
    let store = DashboardStore::new();
    provide_context(store);

    // Restore the active tab from the current URL so reloads keep the user
    // on the page they were on (previously everything snapped back to
    // Dashboard because page state was an ephemeral RwSignal).
    let initial = Page::from_path(&current_path());
    let page = RwSignal::new(initial);
    provide_context(page);

    // When the user clicks a tab, also update the address bar so a
    // subsequent reload lands on the same page.
    let go = move |target: Page| {
        page.set(target);
        push_path(target.as_path());
    };

    // Start WebSocket connection.
    crate::ws::connect(store);

    view! {
        <nav class="navbar">
            <span class="logo">"SongPlayer"</span>
            <button
                class:active=move || page.get() == Page::Dashboard
                on:click=move |_| go(Page::Dashboard)
            >
                "Dashboard"
            </button>
            <button
                class:active=move || page.get() == Page::Live
                on:click=move |_| go(Page::Live)
            >
                "Live"
            </button>
            <button
                class:active=move || page.get() == Page::Lyrics
                on:click=move |_| go(Page::Lyrics)
            >
                "Lyrics"
            </button>
            <button
                class:active=move || page.get() == Page::Settings
                on:click=move |_| go(Page::Settings)
            >
                "Settings"
            </button>
            <span class="ws-indicator">
                {move || {
                    if store.ws_connected.get() {
                        "\u{1F7E2} WS"
                    } else {
                        "\u{1F534} WS"
                    }
                }}
            </span>
        </nav>
        <main class="content">
            {move || match page.get() {
                Page::Dashboard => pages::dashboard::DashboardPage().into_any(),
                Page::Live => pages::live::LivePage().into_any(),
                Page::Settings => pages::settings::SettingsPage().into_any(),
                Page::Lyrics => pages::lyrics::LyricsPage().into_any(),
            }}
        </main>
    }
}
