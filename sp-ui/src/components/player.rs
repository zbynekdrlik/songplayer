//! #194: the ONE playback surface of the app.
//!
//! `Player(playlist_id)` is rendered identically wherever something can play —
//! every Dashboard card, the Live page, and the Dabing page. It composes the
//! existing pieces (now-playing + on/off-program badge, a real seek bar with
//! ±10 s, transport, playback-mode select, the click-to-start live A/V preview,
//! and the shared `Mixer` through the karaoke or dub adapter chosen from the
//! PLAYING item) so the three pages can never drift apart again.
//!
//! Every test id is set INSIDE this component (`player-*`), never injected by a
//! caller — one convention on every page.

use leptos::prelude::*;
use serde::Serialize;
use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::player_view::{self, NowPlayingView, ProgramBadge};
use sp_core::preview_lag::preview_lag_display;
use sp_core::seek_model::{PendingSeek, format_position, seek_display_ms, seek_target_ms};

use crate::api;
use crate::components::live_mixer::LiveMixer;
use crate::components::lyrics_view::LyricsView;
use crate::components::preview_video::PreviewVideo;
use crate::store::DashboardStore;

#[derive(Serialize)]
struct SetModeBody {
    mode: String,
}

/// Current wall-clock time in ms from the browser's monotonic `performance`
/// clock (immune to system-clock jumps), used to age out a pending seek's
/// display hold (#184). sp-ui has no `js-sys` dep, so this reads the already-
/// present `web-sys` `Performance` clock; `0` if the clock is unavailable (never
/// in the running CSR app — the display rule just falls back to the live
/// position, which is the safe default).
pub(crate) fn now_ms() -> u64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0) as u64
}

#[component]
pub fn Player(playlist_id: i64) -> impl IntoView {
    let store = use_context::<DashboardStore>().expect("DashboardStore in context");
    let pid = playlist_id;

    // --- now-playing derivations (each subscribes to store.now_playing) ---
    let np = move || store.now_playing.get().get(&pid).cloned();
    // #194 hotfix: a `Memo`, not a plain closure. A plain closure re-subscribes
    // to `store.now_playing` and re-runs on EVERY position tick, so any slot
    // closure that read `has_content()` (the mixer slot) re-created its child
    // twice a second — resetting a mid-drag fader. A `Memo` only propagates when
    // the boolean actually flips, so a position tick no longer touches the slot.
    let has_content = Memo::new(move |_| np().map(|i| i.has_now_playing_content()).unwrap_or(false));
    // #225: the server told this playlist's state since the page loaded (its
    // on-connect replay tells every playlist's). Until then the Player claims
    // nothing — no "Nič nehrá", no "Nehrá", no program badge.
    let state_known = Memo::new(move |_| np().is_some_and(|i| i.state_known));
    let song = move || np().map(|i| i.song).unwrap_or_default();
    let artist = move || np().map(|i| i.artist).unwrap_or_default();
    let position = move || np().map(|i| i.position_ms).unwrap_or(0);
    // #184: the currently-playing video id — a song change abandons a pending
    // seek (its target belongs to the previous song).
    let video_id = move || np().map(|i| i.video_id).unwrap_or(0);
    let duration = move || np().map(|i| i.duration_ms).unwrap_or(0);
    let state = move || np().map(|i| i.state).unwrap_or_default();
    let transport = move || np().map(|i| i.transport).unwrap_or_default();
    let mode = move || np().map(|i| i.mode).unwrap_or_default();

    // #225: what the now-playing area may show — a song, "Nič nehrá" only once
    // told nothing plays, else "Načítavam…" (not told yet, or it plays and its
    // song has not arrived). A Memo, so a position tick never re-runs the
    // mixer slot (Rule 1).
    let np_view = Memo::new(move |_| {
        player_view::now_playing_view(state_known.get(), has_content.get(), transport())
    });

    // #201: the play/pause label follows the pipeline's own TRANSPORT state, not
    // the scene-aware `state` — a dub decoding OFF program (state
    // WaitingForScene, transport Playing) reads `⏸ Pauza` and a click posts
    // /pause. On/off-program shows only in the badge (the WS state, #225).
    let is_playing = Memo::new(move |_| matches!(transport(), TransportState::Playing));

    // The pipeline is DECODING when it is Playing OR waiting off-program for its
    // scene with a real current video. Preparing a dub on the Dabing page before
    // cutting it in is exactly this off-program decoding state, so the preview +
    // mixer follow "is decoding", not "is on the program".
    let is_decoding = Memo::new(move |_| {
        matches!(
            state(),
            PlaybackState::Playing | PlaybackState::WaitingForScene
        ) && has_content.get()
    });

    // Playback-command errors (play / pause / skip / prev / seek / mode) surface
    // here as Slovak text and clear on the next successful command.
    let player_error = RwSignal::new(None::<String>);

    // #229: this playlist's videos that failed to open in a row (its health
    // row's `open_failures`, the 1 Hz poll of `store.ndi_health`): the reason
    // a black program has. The line is mounted by a Memo (Rule 1); only its
    // text follows the poll, whose `retry_in_ms` (read on the server's clock)
    // moves the countdown.
    let open_failures = move || {
        store
            .ndi_health
            .get()
            .into_iter()
            .find(|o| o.playlist_id == pid)
            .and_then(|o| o.open_failures)
    };
    let failing = Memo::new(move |_| open_failures().is_some());
    let retry_pending =
        Memo::new(move |_| open_failures().is_some_and(|f| f.retry_in_ms.is_some()));

    // #221 L4b: "Hrá mimo programu" for a playlist playing off program
    // (`sp_core::player_view::state_label`); "—" until the state is known.
    // #229: "Čaká na ďalší pokus" while the retry of failed opens waits.
    let state_label = move || {
        player_view::player_state_label(
            state_known.get(),
            state(),
            transport(),
            retry_pending.get(),
        )
    };

    // #225: the badge reads the SAME live WS state as the state label (`Playing`
    // = on program, #170), so a cut flips both in one render. It used to read
    // `store.ndi_health` — the 1 Hz poll of the server's 5 s health sample —
    // and lagged the cut by up to ~5 s. A `Memo`, so a position tick never
    // re-renders it.
    let badge = Memo::new(move |_| player_view::program_badge(state_known.get(), state()));

    // --- transport (each command reports failure into `player_error`) ---
    let report = move |ctx: &'static str, r: Result<(), String>| match r {
        Ok(()) => player_error.set(None),
        Err(e) => player_error.set(Some(format!("{ctx}: {e}"))),
    };
    let do_play_pause = move |_| {
        let playing = is_playing.get_untracked();
        leptos::task::spawn_local(async move {
            let action = if playing { "pause" } else { "play" };
            let r = api::post_empty(&format!("/api/v1/playback/{pid}/{action}")).await;
            report(
                if playing {
                    "Pauza zlyhala"
                } else {
                    "Prehrávanie zlyhalo"
                },
                r,
            );
        });
    };
    let do_prev = move |_| {
        leptos::task::spawn_local(async move {
            let r = api::post_empty(&format!("/api/v1/playback/{pid}/previous")).await;
            report("Predošlá zlyhala", r);
        });
    };
    let do_skip = move |_| {
        leptos::task::spawn_local(async move {
            let r = api::post_empty(&format!("/api/v1/playback/{pid}/skip")).await;
            report("Ďalšia zlyhala", r);
        });
    };
    // #225: a refused mode change is no state change, so nothing re-applies
    // the select's `prop:value` and the DOM keeps the operator's unsaved pick.
    // Bumping this on a refusal re-runs that closure, which writes the told
    // mode back into the select.
    let mode_refused = RwSignal::new(0_u32);
    let on_mode = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        let mode = PlaybackMode::from_str_lossy(&val);
        let body = SetModeBody {
            mode: mode.as_str().to_string(),
        };
        leptos::task::spawn_local(async move {
            let r = api::put_json_empty(&format!("/api/v1/playback/{pid}/mode"), &body).await;
            if r.is_err() {
                mode_refused.update(|n| *n = n.wrapping_add(1));
            }
            report("Zmena režimu zlyhala", r);
        });
    };

    // --- seek (with a drag gate) ---
    // While the pointer is down the DRAGGED value is authoritative, so the
    // twice-a-second position tick can't snap the thumb back mid-drag; exactly
    // ONE seek POST fires on release (`on:change`). #194 hotfix.
    let seek_dragging = RwSignal::new(false);
    let seek_drag_ms = RwSignal::new(0_u64);
    // #200: the commit happens on RELEASE (`pointerup`/`touchend`) from the pending
    // drag value — a real browser fires `pointerup` before `change`, and once the
    // gate re-applies the live value Chrome suppresses `change` entirely, so
    // `change` is only the keyboard path. Value-dedup keeps it to ONE POST.
    let seek_committed = RwSignal::new(None::<u64>);
    // #198 item 1: a `dirty` latch set by `on:input`, cleared on commit. A bare
    // `change` with no preceding `input` in this session (a programmatic / stale
    // dispatch, or a keyboard change that never moved the slider) must NOT commit
    // the initial 0 ms — `seek_drag_ms` starts at 0. `on:change` reads this latch
    // and is a no-op when it is false.
    let seek_dirty = RwSignal::new(false);
    // #184: the committed-but-not-yet-honoured seek. Set on the commit below;
    // the seek bar DISPLAYS its target (never the stale live position) until the
    // pipeline's post-seek fast-forward catches up, so the bar no longer jumps
    // back to the pre-seek position and then forward (the owner's "skocil spat a
    // potom na miesto"). Cleared by the Effect below.
    let seek_pending = RwSignal::new(None::<PendingSeek>);
    let do_seek = move |ms: u64| {
        leptos::task::spawn_local(async move {
            let r = api::seek_playlist(pid, ms).await;
            report("Pretáčanie zlyhalo", r);
        });
    };
    let commit_seek = move |ms: u64| {
        seek_dirty.set(false);
        if seek_committed.get_untracked() != Some(ms) {
            seek_committed.set(Some(ms));
            // #184: remember the target + when we committed so the display holds
            // it while the live position is still stale.
            seek_pending.set(Some(PendingSeek {
                target_ms: ms,
                committed_at_ms: now_ms(),
            }));
            do_seek(ms);
        }
    };
    // #184: release the pending display hold the moment the SAME pure rule the
    // display uses stops returning the target — the live position has caught up
    // to within SEEK_CATCH_UP_MS of it, or the 5 s hold expired (a seek the
    // pipeline could not honour, e.g. at EOS). A song change abandons the pending
    // seek outright (its target belongs to the previous song). Reads `pending`
    // untracked so neither the commit nor this Effect's own clear re-triggers it;
    // it tracks only the live position + video id (both fire on the WS tick).
    Effect::new(move |prev_vid: Option<i64>| -> i64 {
        let vid = video_id();
        let live = position();
        if prev_vid.is_some_and(|p| p != vid) {
            seek_pending.set(None);
        } else if let Some(p) = seek_pending.get_untracked() {
            if seek_display_ms(false, 0, live, Some(p), now_ms()) != p.target_ms {
                seek_pending.set(None);
            }
        }
        vid
    });
    let seek_back = move |_| do_seek(seek_target_ms(position(), -10_000, duration()));
    let seek_fwd = move |_| do_seek(seek_target_ms(position(), 10_000, duration()));

    // --- preview (click-to-start; torn down when the pipeline stops decoding) ---
    let preview_on = RwSignal::new(false);
    // #184 round F/G: the latest picture lag (seconds behind the wall) the
    // preview shim reported (beacon lag or ping/pong transport lag, whichever is
    // worse); the readout shows only at ≥ 3 s.
    let preview_lag = RwSignal::new(0.0_f64);
    // Fold the raw shim reports (which arrive ~4×/s from the pump tick) into the
    // DISPLAYED readout (Option<i64>). A Memo only propagates when that value
    // changes, so the span re-renders on a real change, not on every tick — the
    // shared sp-ui rule that a slot closure reads a Memo, not a chatty signal.
    let preview_lag_readout = Memo::new(move |_| {
        if preview_on.get() {
            preview_lag_display(preview_lag.get())
        } else {
            None
        }
    });
    // #225 review round 3: only once the server TOLD this playlist's state. A
    // reconnect forgets every playlist until its replay lands (and the replay
    // tells a playing one's song a message before its state), so a running
    // preview is kept through it and comes back by itself, instead of the
    // operator clicking "▶ Živý náhľad" again after every deploy.
    Effect::new(move |_| {
        if state_known.get() && !is_decoding.get() {
            preview_on.set(false);
        }
    });

    // --- mixer slot: the ONE LiveMixer (#184 round G). It reads the playing item
    // (its stems + dub readiness) from `store.now_playing` + the app-wide
    // `store.dabing` itself, so it renders identically on every page and stays
    // mounted across position ticks (its channel shape is Memo-gated internally).

    view! {
        <div class="player" data-testid="player">
            <div class="player-head">
                <div class="player-titles">
                    <span class="player-title" data-testid="player-title">
                        {move || player_view::player_title(np_view.get(), &song(), &artist())}
                    </span>
                    <span class="player-state" data-testid="player-state">
                        {state_label}
                    </span>
                </div>
                <span
                    class="player-program-badge"
                    class:on=move || badge.get() == ProgramBadge::OnProgram
                    data-testid="player-program-badge"
                >
                    {move || badge.get().label()}
                </span>
            </div>

            // --- error line: a failed command surfaces here, clears on success ---
            {move || {
                player_error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="player-error" data-testid="player-error">
                                {e}
                            </div>
                        }
                    })
            }}

            // --- #229: why the program is black: the videos cannot be opened ---
            {move || {
                failing
                    .get()
                    .then(|| {
                        view! {
                            <div class="player-open-failures" data-testid="player-open-failures">
                                {move || {
                                    open_failures()
                                        .map(|f| player_view::open_failures_line(&f))
                                        .unwrap_or_default()
                                }}
                            </div>
                        }
                    })
            }}

            // --- seek row (the range IS the progress; no second bar) ---
            <div class="player-seek-row">
                <input
                    type="range"
                    class="player-seek"
                    data-testid="player-seek"
                    min="0"
                    max=move || duration().to_string()
                    step="1000"
                    prop:value=move || {
                        seek_display_ms(
                            seek_dragging.get(),
                            seek_drag_ms.get(),
                            position(),
                            seek_pending.get(),
                            now_ms(),
                        )
                            .to_string()
                    }
                    prop:disabled=move || !has_content.get()
                    on:pointerdown=move |_| {
                        seek_committed.set(None); // a new drag may land on the old value
                        seek_drag_ms.set(position());
                        seek_dragging.set(true);
                    }
                    on:touchstart=move |_| {
                        seek_committed.set(None);
                        seek_drag_ms.set(position());
                        seek_dragging.set(true);
                    }
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<u64>() {
                            seek_drag_ms.set(v);
                            seek_dirty.set(true);
                        }
                    }
                    on:change=move |_| {
                        // Keyboard / programmatic path (a pointer release already
                        // committed and the dedup makes this a no-op then). #198:
                        // a bare `change` with no preceding `input` this session is
                        // a no-op — the `dirty` latch gates it, so a stale/synthetic
                        // change never commits the initial 0 ms.
                        if seek_dirty.get_untracked() {
                            commit_seek(seek_drag_ms.get_untracked());
                        }
                        seek_dragging.set(false);
                    }
                    on:pointerup=move |_| {
                        if seek_dragging.get_untracked() {
                            commit_seek(seek_drag_ms.get_untracked());
                        }
                        seek_dragging.set(false);
                    }
                    on:touchend=move |_| {
                        if seek_dragging.get_untracked() {
                            commit_seek(seek_drag_ms.get_untracked());
                        }
                        seek_dragging.set(false);
                    }
                    on:pointercancel=move |_| seek_dragging.set(false)
                />
                <div class="player-seek-controls">
                    <button
                        type="button"
                        class="player-btn"
                        data-testid="player-back10"
                        title="Pretočiť o 10 s späť"
                        prop:disabled=move || !has_content.get()
                        on:click=seek_back
                    >
                        "−10 s"
                    </button>
                    <span class="player-pos" data-testid="player-pos">
                        {move || {
                            let p = seek_display_ms(
                                seek_dragging.get(),
                                seek_drag_ms.get(),
                                position(),
                                seek_pending.get(),
                                now_ms(),
                            );
                            format!("{} / {}", format_position(p), format_position(duration()))
                        }}
                    </span>
                    <button
                        type="button"
                        class="player-btn"
                        data-testid="player-fwd10"
                        title="Pretočiť o 10 s vpred"
                        prop:disabled=move || !has_content.get()
                        on:click=seek_fwd
                    >
                        "+10 s"
                    </button>
                </div>
            </div>

            // --- transport ---
            <div class="player-transport">
                <button
                    type="button"
                    class="player-btn"
                    data-testid="player-prev"
                    title="Predošlá"
                    on:click=do_prev
                >
                    "⏮ Predošlá"
                </button>
                <button
                    type="button"
                    class="player-btn player-btn-primary"
                    data-testid="player-playpause"
                    title=move || player_view::play_pause(state_known.get(), is_playing.get()).1
                    prop:disabled=move || !state_known.get()
                    on:click=do_play_pause
                >
                    // #225 review round 4: "⏯", disabled, until the state is
                    // known — no "▶ Prehrať" for a playlist that plays.
                    {move || player_view::play_pause(state_known.get(), is_playing.get()).0}
                </button>
                <button
                    type="button"
                    class="player-btn"
                    data-testid="player-skip"
                    title="Ďalšia"
                    on:click=do_skip
                >
                    "⏭ Ďalšia"
                </button>
                <select
                    class="player-mode"
                    data-testid="player-mode"
                    title="Režim prehrávania"
                    // #225 review round 4: "—", disabled, until the mode is
                    // told — never the default "Plynulo" as if it were known.
                    prop:value=move || {
                        mode_refused.track();
                        player_view::mode_value(state_known.get(), mode()).to_string()
                    }
                    prop:disabled=move || !state_known.get()
                    on:change=on_mode
                >
                    <option value="" disabled hidden>"—"</option>
                    <option value="continuous">"Plynulo"</option>
                    <option value="single">"Jedna skladba"</option>
                    <option value="loop">"Opakovať"</option>
                </select>
            </div>

            // --- live A/V preview slot (click-to-start; available whenever the
            // pipeline is decoding, incl. an off-program dub on the Dabing page) ---
            <div class="player-preview">
                {move || {
                    if !is_decoding.get() {
                        view! {
                            <div class="preview-placeholder" data-testid="preview-placeholder">
                                "Bez náhľadu"
                            </div>
                        }
                            .into_any()
                    } else if preview_on.get() {
                        view! {
                            <PreviewVideo
                                playlist_id=pid
                                on_stop=Callback::new(move |_| preview_on.set(false))
                                on_lag=Callback::new(move |s: f64| preview_lag.set(s))
                            />
                        }
                            .into_any()
                    } else {
                        view! {
                            <div class="preview-placeholder" data-testid="preview-placeholder">
                                <button
                                    type="button"
                                    class="preview-btn preview-start-btn"
                                    data-testid="preview-start"
                                    on:click=move |_| {
                                        preview_lag.set(0.0);
                                        preview_on.set(true);
                                    }
                                >
                                    "▶ Živý náhľad"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                }}
                // #184 round F: the lag readout — shown only while the preview is
                // mounted AND the picture is ≥ 3 s behind the wall (the pure
                // sp_core threshold), so the owner sees at a glance that the
                // PICTURE is late, not the control he just moved.
                {move || {
                    preview_lag_readout
                        .get()
                        .map(|n| {
                            view! {
                                <span class="preview-lag" data-testid="preview-lag">
                                    {format!("náhľad mešká {n} s")}
                                </span>
                            }
                        })
                }}
            </div>

            // --- mixer slot: collapses to one line when nothing plays (or,
            // #225, while the playlist's state / song is not known yet); the
            // faders/presets appear only with a playing item, and the adapter
            // follows that item (dub row → dub mixer, else stems mixer) ---
            <div class="player-mixer">
                {move || match np_view.get() {
                    NowPlayingView::Song => {
                        // #184 round G: the ONE LiveMixer follows the playing item
                        // (song stems / dub) itself — one strip, every page.
                        view! { <LiveMixer playlist_id=pid /> }.into_any()
                    }
                    NowPlayingView::Idle => {
                        view! {
                            <div class="player-mixer-idle" data-testid="player-mixer-idle">
                                "Mixér — nič nehrá"
                            </div>
                        }
                            .into_any()
                    }
                    NowPlayingView::Pending => {
                        view! {
                            <div class="player-mixer-pending" data-testid="player-mixer-pending">
                                "Mixér — načítavam…"
                            </div>
                        }
                            .into_any()
                    }
                }}
            </div>

            // --- lyrics slot: the ONE shared LyricsView (compact karaoke
            // preview) — identical on Dashboard, Live and Dabing; the dub's
            // subtitles arrive over the same now-playing WS lines (#194 r3c).
            <div class="player-lyrics">
                <LyricsView playlist_id=pid />
            </div>
        </div>
    }
}
