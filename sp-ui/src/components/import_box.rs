//! #194 ROUND 2: the ONE paste-a-URL import box.
//!
//! Replaces `import_url_box.rs` (Live, English "Import") and the inline box in
//! `pages/dabing.rs` (Slovak "Pridať"). One Slovak label set, one status line,
//! parameterised only by the import target (a normal playlist vs the Dabing
//! chain). Same markup + testids on every page.

use leptos::prelude::*;

/// Where a pasted URL is imported to.
#[derive(Clone, Copy)]
pub enum ImportTarget {
    /// Import into a playlist (`POST /api/v1/playlists/{id}/import`).
    Playlist(i64),
    /// Import into the Dabing chain (`POST /api/v1/dabing`).
    Dabing,
}

#[component]
pub fn ImportBox(
    target: ImportTarget,
    /// Called with `(video_id, title)` after a successful import, so the page
    /// can refresh its set list / dabing list.
    #[prop(optional)]
    on_imported: Option<Callback<(i64, String)>>,
) -> impl IntoView {
    let url = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    let submit = move || {
        let trimmed = url.get_untracked().trim().to_string();
        if trimmed.is_empty() {
            status.set("Vlož najprv URL videa".to_string());
            return;
        }
        busy.set(true);
        status.set(String::new());
        leptos::task::spawn_local(async move {
            let result = match target {
                ImportTarget::Playlist(pid) => crate::api::import_video(trimmed.clone(), pid).await,
                ImportTarget::Dabing => crate::api::import_dabing(trimmed.clone()).await,
            };
            match result {
                Ok(resp) => {
                    status.set(format!("pridané: {}", resp.title));
                    url.set(String::new());
                    if let Some(cb) = on_imported {
                        cb.run((resp.video_id, resp.title.clone()));
                    }
                }
                Err(e) => status.set(format!("import zlyhal: {e}")),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="import-box" data-testid="import-box">
            <input
                type="text"
                class="import-input"
                data-testid="import-input"
                placeholder="Vlož URL videa (napr. https://youtu.be/…)"
                prop:value=move || url.get()
                on:input=move |ev| url.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if ev.key() == "Enter" && !busy.get_untracked() {
                        submit();
                    }
                }
                prop:disabled=move || busy.get()
            />
            <button
                type="button"
                class="import-btn"
                data-testid="import-btn"
                on:click=move |_| submit()
                prop:disabled=move || busy.get()
            >
                {move || if busy.get() { "Pridávam…" } else { "Pridať" }}
            </button>
            <div class="import-status" data-testid="import-status">
                {move || status.get()}
            </div>
        </div>
    }
}
