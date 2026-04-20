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

#[component]
pub fn App() -> impl IntoView {
    let store = DashboardStore::new();
    provide_context(store);

    let page = RwSignal::new(Page::Dashboard);
    provide_context(page);

    // Start WebSocket connection.
    crate::ws::connect(store);

    view! {
        <nav class="navbar">
            <span class="logo">"SongPlayer"</span>
            <button
                class:active=move || page.get() == Page::Dashboard
                on:click=move |_| page.set(Page::Dashboard)
            >
                "Dashboard"
            </button>
            <button
                class:active=move || page.get() == Page::Live
                on:click=move |_| page.set(Page::Live)
            >
                "Live"
            </button>
            <button
                class:active=move || page.get() == Page::Lyrics
                on:click=move |_| page.set(Page::Lyrics)
            >
                "Lyrics"
            </button>
            <button
                class:active=move || page.get() == Page::Settings
                on:click=move |_| page.set(Page::Settings)
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
