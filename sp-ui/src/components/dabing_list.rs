//! Dabing section list (#180). Renders every dub-requested video newest-first
//! (from `store.dabing`) with the chain-state glyph row
//! `stiahnuté → stemy → prepis → preklad → dabing → pripravené` (or
//! `chyba: <krok>`) and a **Prehrať** button that plays the video on its own
//! playlist output via the generic play-video endpoint.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::components::dub_mixer::DubMixer;

/// The ordered chain steps + their Slovak labels. The index is the step's
/// position; a row's `chain_state` resolves to the highest reached index.
const CHAIN_LABELS: [&str; 6] = [
    "stiahnuté",
    "stemy",
    "prepis",
    "preklad",
    "dabing",
    "pripravené",
];

/// Map a `chain_state` wire string to the reached step index (0..=5), or `None`
/// for `failed` (rendered as an error line instead of the glyph row).
fn reached_step(chain_state: &str) -> Option<usize> {
    match chain_state {
        "queued" => Some(0),
        "stems" => Some(1),
        "transcript" => Some(2),
        "translation" => Some(3),
        "synth" => Some(4),
        "ready" => Some(5),
        _ => None, // failed / unknown
    }
}

/// Owned inputs (no borrow of the row) so the returned view is `'static` and can
/// be embedded in the `<For>` children view (edition-2024 `impl Trait` would
/// otherwise capture a `&DubRow` lifetime → E0515).
fn chain_row(chain_state: String, dub_error: Option<String>) -> impl IntoView {
    let is_failed = chain_state == "failed";
    let reached = reached_step(&chain_state);
    let err = dub_error.unwrap_or_default();
    view! {
        <div class="dabing-chain" data-testid="dabing-chain">
            {if is_failed {
                let msg = if err.is_empty() {
                    "chyba".to_string()
                } else {
                    format!("chyba: {err}")
                };
                view! { <span class="dabing-chain-error">{msg}</span> }.into_any()
            } else {
                view! {
                    <span class="dabing-chain-steps">
                        {CHAIN_LABELS
                            .iter()
                            .enumerate()
                            .map(|(i, label)| {
                                let done = reached.map(|r| i <= r).unwrap_or(false);
                                let sep = if i > 0 { " → " } else { "" };
                                view! {
                                    <span>{sep}</span>
                                    <span class=("dabing-step-done", done)>{*label}</span>
                                }
                            })
                            .collect_view()}
                    </span>
                }
                    .into_any()
            }}
        </div>
    }
}

#[component]
pub fn DabingList() -> impl IntoView {
    let store = expect_context::<crate::store::DashboardStore>();
    let dabing = store.dabing;

    view! {
        <div class="dabing-list" data-testid="dabing-list">
            {move || {
                let rows = dabing.get();
                if rows.is_empty() {
                    view! {
                        <p class="dabing-empty">
                            "Zatiaľ žiadne dabingové videá — vlož URL vyššie."
                        </p>
                    }
                        .into_any()
                } else {
                    view! {
                        <For
                            each=move || dabing.get()
                            key=|r| {
                                (r.video_id, r.chain_state.clone(), r.dub_error.clone())
                            }
                            children=move |row| {
                                let playlist_id = row.playlist_id;
                                let video_id = row.video_id;
                                let title = row.title.clone();
                                view! {
                                    <div class="dabing-row" data-video-id=video_id.to_string()>
                                        <div class="dabing-row-head">
                                            <span class="dabing-title">{title.clone()}</span>
                                            <button
                                                class="dabing-play-btn"
                                                data-testid="dabing-play"
                                                title="Prehrať na výstupe SP-dabing"
                                                on:click=move |_| {
                                                    spawn_local(async move {
                                                        let _ = api::post_live_play_video(
                                                                playlist_id,
                                                                video_id,
                                                                None,
                                                            )
                                                            .await;
                                                    });
                                                }
                                            >
                                                "Prehrať"
                                            </button>
                                        </div>
                                        {chain_row(row.chain_state.clone(), row.dub_error.clone())}
                                        // #181: the same modern mixer, bound to this
                                        // dub video's blend ratio (inert + labelled
                                        // until the dub is generated).
                                        <DubMixer
                                            video_id=video_id
                                            title=title
                                            dub_status=row.dub_status.clone()
                                            dub_mix_ratio=row.dub_mix_ratio
                                            stem_status=row.stem_status.clone()
                                        />
                                    </div>
                                }
                            }
                        />
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
