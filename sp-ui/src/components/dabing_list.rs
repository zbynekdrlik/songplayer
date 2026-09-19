//! Dabing section list (#180, chain rework #182). Renders every dub-requested
//! video newest-first (from `store.dabing`) with the real engine-chain glyph row
//! `stiahnuté → dabing → titulky → pripravené` (a `stemy` step is inserted when
//! the video has or is getting stems), or `chyba: <krok>`, plus a **Prehrať**
//! button that plays the video on its own playlist output.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

/// Whether the chain shows a `stemy` step — the video has stems or is getting
/// them. Only an over-cap video (`stem_status = "unsupported"`) is dubbed without
/// stems (the 2-stream over-original path) and shows no stems step.
fn shows_stems_step(stem_status: Option<&str>) -> bool {
    stem_status != Some("unsupported")
}

/// The ordered chain steps. The real engine chain is
/// `stiahnuté → dabing → titulky → pripravené`; a `stemy` step is inserted after
/// `stiahnuté` when the video has or is getting stems (#182 — the old
/// `prepis`/`preklad` steps are gone, subtitles come from the dub session).
fn chain_labels(with_stems: bool) -> Vec<&'static str> {
    if with_stems {
        vec!["stiahnuté", "stemy", "dabing", "titulky", "pripravené"]
    } else {
        vec!["stiahnuté", "dabing", "titulky", "pripravené"]
    }
}

/// Map a `chain_state` wire string to the reached step index into
/// `chain_labels(with_stems)`, or `None` for `failed` (rendered as an error line
/// instead of the glyph row). `titulky` completes together with `ready`, so a
/// live `synth` row highlights up to `dabing` and a `ready` row to the end.
fn reached_step(chain_state: &str, with_stems: bool) -> Option<usize> {
    let dabing_idx = if with_stems { 2 } else { 1 };
    let ready_idx = if with_stems { 4 } else { 3 };
    match chain_state {
        "queued" => Some(0), // stiahnuté
        "stems" => Some(if with_stems { 1 } else { 0 }),
        // transcript/translation are legacy pre-transcript refinements; the dub is
        // being synthesized (or about to be), so they map to the dabing step.
        "synth" | "transcript" | "translation" => Some(dabing_idx),
        "ready" => Some(ready_idx),
        _ => None, // failed / unknown
    }
}

/// Owned inputs (no borrow of the row) so the returned view is `'static` and can
/// be embedded in the `<For>` children view (edition-2024 `impl Trait` would
/// otherwise capture a `&DubRow` lifetime → E0515).
fn chain_row(chain_state: String, dub_error: Option<String>, with_stems: bool) -> impl IntoView {
    let is_failed = chain_state == "failed";
    let reached = reached_step(&chain_state, with_stems);
    let labels = chain_labels(with_stems);
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
                        {labels
                            .into_iter()
                            .enumerate()
                            .map(|(i, label)| {
                                let done = reached.map(|r| i <= r).unwrap_or(false);
                                let sep = if i > 0 { " → " } else { "" };
                                view! {
                                    <span>{sep}</span>
                                    <span class=("dabing-step-done", done)>{label}</span>
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
                                // #181: include stem_status so a late stems arrival
                                // (null → "done" while dub_status stays "ready")
                                // recreates the row and its DubMixer, refreshing the
                                // read-only originál-bed display (floored → 1−r).
                                (
                                    r.video_id,
                                    r.chain_state.clone(),
                                    r.dub_error.clone(),
                                    r.stem_status.clone(),
                                )
                            }
                            children=move |row| {
                                let playlist_id = row.playlist_id;
                                let video_id = row.video_id;
                                let title = row.title.clone();
                                let with_stems = shows_stems_step(row.stem_status.as_deref());
                                // #194: the dub mixer moved to the shared Player at
                                // the top of the page (one mixer location). A row
                                // that is NOT the playing item shows its stored blend
                                // ratio as read-only text instead.
                                let ready = row.dub_status == "ready";
                                let ratio_pct = (row.dub_mix_ratio.clamp(0.0, 1.0) * 100.0)
                                    .round() as i64;
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
                                        {chain_row(
                                            row.chain_state.clone(),
                                            row.dub_error.clone(),
                                            with_stems,
                                        )}
                                        {if ready {
                                            view! {
                                                <div
                                                    class="dabing-row-ratio"
                                                    data-testid="dabing-row-ratio"
                                                >
                                                    {format!(
                                                        "Pomer dabingu: {ratio_pct} % (mix v prehrávači hore)",
                                                    )}
                                                </div>
                                            }
                                                .into_any()
                                        } else {
                                            view! { <span></span> }.into_any()
                                        }}
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
