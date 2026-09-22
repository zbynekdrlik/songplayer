//! The ONE live mixer console endpoints (#184 round G, supersedes #14 karaoke.rs).
//!
//! `GET /api/v1/mix`   → the three fader positions + stem progress + per-song
//!                       now-playing stems state (the #177 block, moved here
//!                       unchanged).
//! `PATCH /api/v1/mix` → set any subset of the three faders (partial). The live
//!                       `EngineCommand::SetMix` push is awaited FIRST (so a
//!                       playing mix re-blends in ~1.6 s), THEN the settings
//!                       persist — the round-A live-first ordering.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sp_core::mixer_model::{MixFaders, MixKind};

use crate::{AppState, EngineCommand};

/// The wire string for a [`MixKind`] — the `"kind"` field of `GET /api/v1/mix`.
fn kind_str(kind: MixKind) -> &'static str {
    match kind {
        MixKind::Song => "song",
        MixKind::Dub => "dub",
    }
}

/// GET the live mixer state + stem progress for the dashboard.
///
/// #177: also returns `now_playing[]` — one entry per currently-playing pipeline:
/// `{playlist_id, video_id, title, stems_state, stems_error, queue_position}` — so
/// the mixer can bind to the SELECTED playlist's song and show whether ITS stems
/// are ready. The set comes from the process-global now-playing registry; per-song
/// stems state is read from the same DB the stem worker writes (never re-derived).
pub async fn get_mix(State(state): State<AppState>) -> impl IntoResponse {
    let control = crate::stems::control::global();
    let f = control.faders();
    let kind = kind_str(control.kind());
    let (pending, done) = crate::db::models_stems::count_stems_progress(&state.pool)
        .await
        .unwrap_or((0, 0));

    let in_flight = crate::stems::progress::in_flight();
    let (on_program, recent) = crate::stems::queue_tiers::compute_tier_inputs(
        Some(&state.ndi_health_registry),
        &state.pool,
    )
    .await;
    let mut now_playing = Vec::new();
    for (playlist_id, video_id) in crate::now_playing::global().snapshot() {
        let info = match crate::db::models_stems::video_stems_info(
            &state.pool,
            video_id,
            in_flight == Some(video_id),
        )
        .await
        {
            Ok(Some(info)) => info,
            _ => continue, // row vanished; skip rather than emit a half entry
        };
        let queue_position = crate::db::models_stems_priority::queue_position(
            &state.pool,
            video_id,
            &on_program,
            &recent,
        )
        .await
        .ok()
        .flatten();
        now_playing.push(serde_json::json!({
            "playlist_id": playlist_id,
            "video_id": video_id,
            "title": info.title,
            "stems_state": info.state.as_str(),
            "stems_error": stems_error(info.state, info.attempts),
            "queue_position": queue_position,
        }));
    }

    Json(serde_json::json!({
        "vokaly": f.vokaly,
        "podklad": f.podklad,
        "dabing": f.dabing,
        "kind": kind,
        "stems_pending": pending,
        "stems_done": done,
        "now_playing": now_playing,
    }))
}

/// A human reason string for the non-ready states the mixer surfaces (#177).
/// There is no per-song stem error text stored in the DB, so this is derived from
/// the state + attempt count; `None` for states that need no explanation.
fn stems_error(state: crate::db::models_stems::StemsState, attempts: i64) -> Option<String> {
    use crate::db::models_stems::StemsState;
    match state {
        StemsState::Failed => Some(format!(
            "posledný pokus o spracovanie stemov zlyhal (pokusov: {attempts})"
        )),
        StemsState::Unavailable => {
            Some("skladba je pridlhá alebo bez vokálov — stemy nie sú dostupné".to_string())
        }
        _ => None,
    }
}

/// Body for `PATCH /api/v1/mix` — any subset of the three faders (each `0.0..=1.0`;
/// an omitted fader keeps its current value).
#[derive(Debug, Deserialize)]
pub struct SetMixRequest {
    #[serde(default)]
    pub vokaly: Option<f32>,
    #[serde(default)]
    pub podklad: Option<f32>,
    #[serde(default)]
    pub dabing: Option<f32>,
}

/// PATCH any subset of the three faders. The live `EngineCommand::SetMix` push is
/// awaited FIRST (so a playing mix re-blends in ~1.6 s), THEN the settings persist
/// — the reverse of a naive order, where the persist's pool `acquire()` could park
/// for up to sqlx's 30 s default before the live gains were touched. A persist
/// failure is logged + returned as 500, but the live change already happened.
/// 200 + the full clamped triple on success.
pub async fn patch_mix(
    State(state): State<AppState>,
    Json(body): Json<SetMixRequest>,
) -> impl IntoResponse {
    let control = crate::stems::control::global();
    let cur = control.faders();
    // The kind whose memory this PATCH edits + persists — the ACTIVE console
    // (#184 round G1). Read once so the live push and the persist agree.
    let kind = control.kind();
    // The SAME clamped/NaN-guarded target the live push and the persist both use.
    let target = MixFaders::new(
        body.vokaly.unwrap_or(cur.vokaly),
        body.podklad.unwrap_or(cur.podklad),
        body.dabing.unwrap_or(cur.dabing),
    );

    let engine_tx = state.engine_tx.clone();
    let push = async move {
        // Apply the live console update DIRECTLY here (idempotent with the engine's
        // own `set_faders`) so a following PARTIAL PATCH reads a fresh `cur` from
        // the global control — the partial-merge must not race the async engine
        // loop (and must work even where no engine drains the channel). The engine
        // `SetMix` push additionally broadcasts `MixChanged` + logs.
        crate::stems::control::global().set_faders(target);
        let _ = engine_tx
            .send(EngineCommand::SetMix { faders: target })
            .await;
    };
    let pool = state.pool.clone();
    let persist = async move {
        // Persist ONLY the active kind's keys: a SONG edit writes the song pair (its
        // `dabing` is unused), a DUB edit writes the dub triple. The other memory's
        // settings are untouched, so it survives a restart (#184 round G1).
        match kind {
            MixKind::Song => {
                crate::db::models::set_setting(
                    &pool,
                    sp_core::config::SETTING_MIX_SONG_VOKALY,
                    &target.vokaly.to_string(),
                )
                .await?;
                crate::db::models::set_setting(
                    &pool,
                    sp_core::config::SETTING_MIX_SONG_PODKLAD,
                    &target.podklad.to_string(),
                )
                .await?;
            }
            MixKind::Dub => {
                crate::db::models::set_setting(
                    &pool,
                    sp_core::config::SETTING_MIX_DUB_VOKALY,
                    &target.vokaly.to_string(),
                )
                .await?;
                crate::db::models::set_setting(
                    &pool,
                    sp_core::config::SETTING_MIX_DUB_PODKLAD,
                    &target.podklad.to_string(),
                )
                .await?;
                crate::db::models::set_setting(
                    &pool,
                    sp_core::config::SETTING_MIX_DUB_DABING,
                    &target.dabing.to_string(),
                )
                .await?;
            }
        }
        Ok::<MixFaders, sqlx::Error>(target)
    };

    match super::mix_apply::apply_mix(push, persist).await {
        Ok(f) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "vokaly": f.vokaly,
                "podklad": f.podklad,
                "dabing": f.dabing,
                // Parity with GET /mix: echo the active kind this PATCH edited.
                "kind": kind_str(kind),
            })),
        )
            .into_response(),
        Err(e) => {
            // The live change already applied; only the persist failed.
            tracing::warn!("patch_mix persist error (live change applied): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[cfg(test)]
#[path = "mix_tests.rs"]
mod tests;
