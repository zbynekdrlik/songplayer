//! Paste-a-YouTube-URL box. Feeds the worship-training bootstrap flow:
//! operator pastes a bare YouTube URL, server runs `yt-dlp --dump-json`
//! and inserts a row; the set-list refreshes so the operator can then
//! add the new video to their live setlist.

use leptos::prelude::*;

#[component]
pub fn ImportUrlBox(
    playlist_id: i64,
    #[prop(into)] on_imported: Callback<(i64, String)>,
) -> impl IntoView {
    let url = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    let submit = move || {
        let current = url.get_untracked();
        let trimmed = current.trim().to_string();
        if trimmed.is_empty() {
            status.set("paste a YouTube URL first".to_string());
            return;
        }
        busy.set(true);
        status.set(String::new());
        leptos::task::spawn_local(async move {
            match crate::api::import_video(trimmed.clone(), playlist_id).await {
                Ok(resp) => {
                    status.set(format!("imported \"{}\"", resp.title));
                    url.set(String::new());
                    on_imported.run((resp.video_id, resp.title.clone()));
                }
                Err(e) => status.set(format!("import failed: {e}")),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="import-url-box">
            <input
                type="text"
                class="import-url-input"
                placeholder="Paste YouTube URL (e.g. https://youtu.be/…)"
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
                class="import-url-btn"
                on:click=move |_| submit()
                prop:disabled=move || busy.get()
            >
                {move || if busy.get() { "Importing…" } else { "Import" }}
            </button>
            <div class="import-url-status">{move || status.get()}</div>
        </div>
    }
}
