//! The ONE live mixer console endpoints (#184 round G/G2, supersedes #14 karaoke.rs).
//!
//! `GET /api/v1/mix`   → BOTH kind-scoped memories (`song:{vokaly,podklad}`,
//!                       `dub:{vokaly,podklad,dabing}`) + stem progress + per-song
//!                       now-playing stems state (the #177 block, unchanged). There
//!                       is NO global "active kind" (round G2).
//! `PATCH /api/v1/mix` → `{kind:"song"|"dub", vokaly?, podklad?, dabing?}` — set any
//!                       subset of ONE memory's faders. `kind` is REQUIRED (400
//!                       without it; `dabing` on a `song` edit → 400). The live
//!                       `EngineCommand::SetMix` push is awaited FIRST (so a playing
//!                       mix re-blends in ~1.6 s), THEN that kind's settings persist —
//!                       the round-A live-first ordering.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sp_core::mixer_model::{MixFaders, MixKind};

use crate::{AppState, EngineCommand};

/// The wire string for a [`MixKind`] — the `"kind"` field of `PATCH /api/v1/mix`.
fn kind_str(kind: MixKind) -> &'static str {
    match kind {
        MixKind::Song => "song",
        MixKind::Dub => "dub",
    }
}

/// Parse the required `"kind"` field of a `PATCH /api/v1/mix` body.
fn parse_kind(s: &str) -> Option<MixKind> {
    match s {
        "song" => Some(MixKind::Song),
        "dub" => Some(MixKind::Dub),
        _ => None,
    }
}

/// GET the live mixer state (BOTH kind memories) + stem progress for the dashboard.
///
/// #177: also returns `now_playing[]` — one entry per currently-playing pipeline:
/// `{playlist_id, video_id, title, stems_state, stems_error, queue_position}` — so
/// the mixer can bind to the SELECTED playlist's song and show whether ITS stems
/// are ready. The set comes from the process-global now-playing registry; per-song
/// stems state is read from the same DB the stem worker writes (never re-derived).
pub async fn get_mix(State(state): State<AppState>) -> impl IntoResponse {
    let control = crate::stems::control::global();
    // Round G2: each reader family owns its memory; the strip reads the object for
    // ITS item's kind. No global active kind.
    let song = control.faders(MixKind::Song);
    let dub = control.faders(MixKind::Dub);
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
        // The SONG memory (its dabing is unused, so it is not sent).
        "song": { "vokaly": song.vokaly, "podklad": song.podklad },
        // The DUB memory (all three faders live).
        "dub": { "vokaly": dub.vokaly, "podklad": dub.podklad, "dabing": dub.dabing },
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

/// Body for `PATCH /api/v1/mix` — the REQUIRED `kind` (`"song"|"dub"`) plus any
/// subset of the three faders (each `0.0..=1.0`; an omitted fader keeps its current
/// value in that kind's memory).
#[derive(Debug, Deserialize)]
pub struct SetMixRequest {
    /// Which memory to edit — REQUIRED (400 without it, round G2).
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub vokaly: Option<f32>,
    #[serde(default)]
    pub podklad: Option<f32>,
    #[serde(default)]
    pub dabing: Option<f32>,
}

/// PATCH any subset of ONE memory's faders. `kind` is REQUIRED (400 without it, and
/// `dabing` on a `song` edit → 400 — a song has no dub stream). The live
/// `EngineCommand::SetMix` push is awaited FIRST (so a playing mix re-blends in
/// ~1.6 s), THEN that kind's settings persist — the reverse of a naive order, where
/// the persist's pool `acquire()` could park for up to sqlx's 30 s default before
/// the live gains were touched. A persist failure is logged + returned as 500, but
/// the live change already happened. 200 + that memory + `kind` on success.
pub async fn patch_mix(
    State(state): State<AppState>,
    Json(body): Json<SetMixRequest>,
) -> impl IntoResponse {
    // `kind` is required and must be "song" or "dub".
    let kind = match body.kind.as_deref().and_then(parse_kind) {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "kind is required and must be \"song\" or \"dub\"",
            )
                .into_response();
        }
    };
    // `dabing` is meaningless for a song memory (a song has no dub stream).
    if matches!(kind, MixKind::Song) && body.dabing.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            "dabing is not a song fader — use kind \"dub\"",
        )
            .into_response();
    }

    let control = crate::stems::control::global();
    // Merge the partial body onto THIS kind's current memory (an omitted fader keeps
    // its value); read once so the live push and the persist agree.
    let cur = control.faders(kind);
    let target = MixFaders::new(
        body.vokaly.unwrap_or(cur.vokaly),
        body.podklad.unwrap_or(cur.podklad),
        body.dabing.unwrap_or(cur.dabing),
    );

    let engine_tx = state.engine_tx.clone();
    let push = async move {
        // Apply the live console update DIRECTLY here (idempotent with the engine's
        // own `set_faders`) so a following PARTIAL PATCH reads a fresh `cur` from
        // the global control — the partial-merge must not race the async engine loop
        // (and must work even where no engine drains the channel). The engine
        // `SetMix` push additionally broadcasts `MixChanged` + logs.
        crate::stems::control::global().set_faders(kind, target);
        let _ = engine_tx
            .send(EngineCommand::SetMix {
                kind,
                faders: target,
            })
            .await;
    };
    let pool = state.pool.clone();
    let persist = async move {
        // Persist ONLY this kind's keys: a SONG edit writes the song pair (its
        // `dabing` is unused), a DUB edit writes the dub triple. The other memory's
        // settings are untouched, so it survives a restart (#184 round G1/G2).
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
        Ok(f) => {
            // Echo back the updated memory + the kind it edited (parity with GET's
            // per-kind objects). A song memory omits the unused `dabing`.
            let mut memory = serde_json::json!({
                "kind": kind_str(kind),
                "vokaly": f.vokaly,
                "podklad": f.podklad,
            });
            if matches!(kind, MixKind::Dub) {
                memory["dabing"] = serde_json::json!(f.dabing);
            }
            (StatusCode::OK, Json(memory)).into_response()
        }
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
