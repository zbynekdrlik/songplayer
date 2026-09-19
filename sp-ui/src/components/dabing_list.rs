//! Dabing section list (#180, chain rework #182, unified rows #194). Renders
//! every dub-requested video newest-first (from `store.dabing`) as the shared
//! `SongRow`: the dub chip (with the engine-chain `stiahnuté → dabing → titulky
//! → pripravené` folded into its tooltip), the text chip, and the primary play
//! action that plays the video on its own playlist output.

use leptos::prelude::*;
use leptos::task::spawn_local;
use sp_core::status_chip::{dub_chip, text_chip};

use crate::api;
use crate::components::song_row::SongRow;
use crate::components::status_chips::ChipView;

/// Whether the chain shows a `stemy` step — the video has stems or is getting
/// them. Only an over-cap video (`stem_status = "unsupported"`) is dubbed without
/// stems (the 2-stream over-original path) and shows no stems step.
fn shows_stems_step(stem_status: Option<&str>) -> bool {
    stem_status != Some("unsupported")
}

/// The ordered chain steps: `stiahnuté → dabing → titulky → pripravené`; a
/// `stemy` step is inserted after `stiahnuté` when the video has or is getting
/// stems (#182).
fn chain_labels(with_stems: bool) -> Vec<&'static str> {
    if with_stems {
        vec!["stiahnuté", "stemy", "dabing", "titulky", "pripravené"]
    } else {
        vec!["stiahnuté", "dabing", "titulky", "pripravené"]
    }
}

/// Map a `chain_state` wire string to the reached step index into
/// `chain_labels(with_stems)`, or `None` for `failed` / unknown.
fn reached_step(chain_state: &str, with_stems: bool) -> Option<usize> {
    let dabing_idx = if with_stems { 2 } else { 1 };
    let ready_idx = if with_stems { 4 } else { 3 };
    match chain_state {
        "queued" => Some(0),
        "stems" => Some(if with_stems { 1 } else { 0 }),
        "synth" | "transcript" | "translation" => Some(dabing_idx),
        "ready" => Some(ready_idx),
        _ => None,
    }
}

/// The dabing chain as one Slovak tooltip string for the dub chip: the full step
/// path, marking how far it has reached, or the failure text.
fn chain_detail(chain_state: &str, dub_error: Option<&str>, with_stems: bool) -> String {
    if chain_state == "failed" {
        return match dub_error {
            Some(e) if !e.is_empty() => format!("chyba: {e}"),
            _ => "chyba".to_string(),
        };
    }
    let labels = chain_labels(with_stems);
    let path = labels.join(" → ");
    match reached_step(chain_state, with_stems).and_then(|r| labels.get(r)) {
        Some(step) => format!("{path} (teraz: {step})"),
        None => path,
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
                        <crate::components::state_block::StateBlock
                            kind=crate::components::state_block::StateKind::Empty
                            empty_label=Some("Zatiaľ žiadne dabingové videá — vlož URL vyššie.".to_string())
                        />
                    }
                        .into_any()
                } else {
                    view! {
                        <div class="song-list">
                            <For
                                each=move || dabing.get()
                                key=|r| {
                                    (
                                        r.video_id,
                                        r.chain_state.clone(),
                                        r.dub_error.clone(),
                                        r.stem_status.clone(),
                                        r.lyrics_present,
                                        (r.dub_mix_ratio * 1000.0) as i64,
                                    )
                                }
                                children=move |row| {
                                    let playlist_id = row.playlist_id;
                                    let video_id = row.video_id;
                                    let title = row.title.clone();
                                    let with_stems = shows_stems_step(row.stem_status.as_deref());
                                    let ready = row.dub_status == "ready";
                                    let ratio_pct = (row.dub_mix_ratio.clamp(0.0, 1.0) * 100.0)
                                        .round() as i64;
                                    let detail = chain_detail(
                                        &row.chain_state,
                                        row.dub_error.as_deref(),
                                        with_stems,
                                    );
                                    let chips = vec![
                                        ChipView::with_tip(
                                            dub_chip(Some(row.chain_state.as_str())),
                                            detail,
                                        ),
                                        ChipView::new(text_chip(row.lyrics_present, false, false)),
                                    ];
                                    let on_play = Callback::new(move |_| {
                                        spawn_local(async move {
                                            let _ = api::post_live_play_video(
                                                    playlist_id,
                                                    video_id,
                                                    None,
                                                )
                                                .await;
                                        });
                                    });
                                    view! {
                                        <SongRow
                                            video_id=video_id
                                            title=title
                                            chips=chips
                                            on_play=on_play
                                            play_ready=true
                                        >
                                            {ready
                                                .then(|| {
                                                    view! {
                                                        <span
                                                            class="dabing-row-ratio"
                                                            data-testid="dabing-row-ratio"
                                                        >
                                                            {format!(
                                                                "Pomer dabingu: {ratio_pct} % (mix v prehrávači hore)",
                                                            )}
                                                        </span>
                                                    }
                                                })}
                                        </SongRow>
                                    }
                                }
                            />
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
