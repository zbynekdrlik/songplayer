//! #195: worker-agnostic tier inputs for the in-use-first stems queue.
//!
//! Pure free fns (the `lyrics::idle_gate::wall_activity_from` precedent) that
//! reduce the in-process NDI health registry + one `play_history` query to the
//! two playlist-id lists the tiered selector
//! (`db::models_stems_priority::get_next_stem_job`) and the panel's tiered
//! `queue_position` both consume:
//!   - tier 1: the playlist(s) ON OBS program right now, and
//!   - tier 2: playlists played in the last `stems_recent_days` days.
//!
//! Kept free fns so the lyrics reprocess worker can adopt the same tiering later
//! without either worker touching the other's module.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::playback::ndi_health::{NdiHealthRegistry, PipelineHealthSnapshot, PlaybackStateLabel};

/// Number of playlist-restricted tiers consulted between the manual-priority
/// tier (0) and the unrestricted fallback (3): on-program (1) then recently
/// played (2). Drives BOTH the selector loop and the `queue_position` tier CASE
/// via [`restricted_tiers`].
const RESTRICTED_TIERS: usize = 2;

/// Default recency window (`stems_recent_days` setting) for tier 2.
pub(crate) const STEMS_RECENT_DAYS_DEFAULT: i64 = 7;

/// The playlist(s) currently ON OBS program. A snapshot's `state` is already
/// reconciled by `handle_health_snapshot` to mean "playing AND on program" (a
/// playing-but-off-program pipeline is stored as `Paused`), so `Playing` here is
/// precisely "the wall is showing this output". Pure + unit-tested.
pub(crate) fn on_program_playlists(snapshots: &[PipelineHealthSnapshot]) -> Vec<i64> {
    snapshots
        .iter()
        .filter(|s| matches!(s.state, PlaybackStateLabel::Playing))
        .map(|s| s.playlist_id)
        .collect()
}

/// Tier-1 input: the on-program playlist ids, but EMPTY while the wall reading is
/// not trustworthy yet (#167 startup grace: `!activity_known`) or within the
/// heavy-step startup floor (`startup_floor`). During that window a missing
/// `Playing` reads as idle, so promoting "nothing on program" to tier 1 would
/// mis-tier the queue for the first ~60 s — so tier 1 stays empty and the queue
/// falls through to recency / id order. Pure + unit-tested.
pub(crate) fn tier_inputs(
    activity_known: bool,
    startup_floor: bool,
    snapshots: &[PipelineHealthSnapshot],
) -> Vec<i64> {
    if !activity_known || startup_floor {
        return Vec::new();
    }
    on_program_playlists(snapshots)
}

/// Parse the `stems_recent_days` setting, defaulting to
/// [`STEMS_RECENT_DAYS_DEFAULT`] on absent / non-integer values and clamping a
/// negative to 0 (a 0-day window is `datetime('now','-0 days')` = now, i.e. no
/// playlist counts as recent). Pure + unit-tested.
pub(crate) fn recent_days_from(setting: Option<&str>) -> i64 {
    setting
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(STEMS_RECENT_DAYS_DEFAULT)
        .max(0)
}

/// The `(tier_index, playlist_ids)` pairs the tiered selector + `queue_position`
/// iterate: `[(1, on_program), (2, recent)]` for the first [`RESTRICTED_TIERS`]
/// tiers. One pure fn so the tier numbering flows into BOTH the selector loop and
/// the position CASE from a single place. Pure + unit-tested.
pub(crate) fn restricted_tiers<'a>(
    on_program: &'a [i64],
    recent: &'a [i64],
) -> Vec<(i64, &'a [i64])> {
    [on_program, recent]
        .into_iter()
        .enumerate()
        .take(RESTRICTED_TIERS)
        .map(|(i, ids)| (i as i64 + 1, ids))
        .collect()
}

/// Read tier 2's playlist ids: the DISTINCT playlists with a `play_history` row
/// in the last `days` days. Tolerates an empty / just-cleared table (returns an
/// empty Vec). `played_at` and `datetime('now', ?)` share the `datetime('now')`
/// format the table stores, so the comparison is lexicographic-safe.
///
/// mutants::skip — a `play_history` read; the window (`recent_days_from`) is pure
/// + unit-tested and the boundary is covered by a direct backdated-rows test.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn recent_playlists(
    pool: &SqlitePool,
    days: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT DISTINCT playlist_id FROM play_history \
         WHERE played_at >= datetime('now', ?)",
    )
    .bind(format!("-{days} days"))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Build both tier inputs `(on_program, recent)` for a worker/API caller from the
/// in-process registry it already holds + one `play_history` query. No registry
/// (unit tests) → an empty on-program list. I/O orchestration over the pure fns
/// above.
///
/// mutants::skip — pure orchestration of already-tested fns; no logic of its own.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn compute_tier_inputs(
    reg: Option<&Arc<NdiHealthRegistry>>,
    pool: &SqlitePool,
) -> (Vec<i64>, Vec<i64>) {
    let on_program = match reg {
        Some(r) => {
            let snapshots = r.snapshots();
            let known = crate::lyrics::idle_gate::activity_known(
                r.created_pipelines(),
                r.reported_pipelines(),
                r.since_created(),
            );
            let floor = crate::lyrics::idle_gate::startup_floor_defers(r.since_created());
            tier_inputs(known, floor, &snapshots)
        }
        None => Vec::new(),
    };
    let days = recent_days_from(
        crate::db::models::get_setting(pool, "stems_recent_days")
            .await
            .ok()
            .flatten()
            .as_deref(),
    );
    let recent = recent_playlists(pool, days).await.unwrap_or_default();
    (on_program, recent)
}

/// The tiered queue position of `video_id` for the dashboard/API, computed
/// against the SAME on-program + recent tier inputs the worker selects by — so
/// the chip's "vo fronte (N.)" matches the order the worker will pick.
///
/// mutants::skip — pure orchestration; the ranking is in
/// `models_stems_priority::queue_position`.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn queue_position_now(
    reg: Option<&Arc<NdiHealthRegistry>>,
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let (on_program, recent) = compute_tier_inputs(reg, pool).await;
    crate::db::models_stems_priority::queue_position(pool, video_id, &on_program, &recent).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::ndi_health::{AudioStats, PacingStats, PipelineHealthSnapshot};

    /// Minimal health snapshot for the pure tier fns — they read only `state`
    /// and `playlist_id`; every other field is a default (mirrors the engine's
    /// real construction, `worker_tests_idle_gate::playing_snapshot`).
    fn snap(playlist_id: i64, state: PlaybackStateLabel) -> PipelineHealthSnapshot {
        PipelineHealthSnapshot {
            playlist_id,
            ndi_name: format!("SP-{playlist_id}"),
            state,
            connections: 2,
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 0.0,
            source_fps: 0.0,
            last_submit_ts: None,
            last_heartbeat_ts: None,
            consecutive_bad_polls: 0,
            degraded_reason: None,
            clock: crate::playback::clock_health::ClockHealth::default(),
            pacing: PacingStats::default(),
            audio: AudioStats::default(),
            lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
            lock_reason: String::new(),
            burn_on: false,
            recovery_step: None,
            sender_url: None,
            transport: sp_core::playback::TransportState::Idle,
        }
    }

    #[test]
    fn on_program_playlists_keeps_only_playing() {
        let snaps = [
            snap(10, PlaybackStateLabel::Playing),
            snap(11, PlaybackStateLabel::Paused),
            snap(12, PlaybackStateLabel::Playing),
            snap(13, PlaybackStateLabel::Idle),
        ];
        assert_eq!(on_program_playlists(&snaps), vec![10, 12]);
    }

    #[test]
    fn on_program_playlists_empty_when_none_playing() {
        let snaps = [
            snap(10, PlaybackStateLabel::Paused),
            snap(11, PlaybackStateLabel::Idle),
        ];
        assert!(on_program_playlists(&snaps).is_empty());
    }

    #[test]
    fn tier_inputs_returns_on_program_when_known_and_past_floor() {
        let snaps = [
            snap(10, PlaybackStateLabel::Playing),
            snap(11, PlaybackStateLabel::Paused),
        ];
        assert_eq!(tier_inputs(true, false, &snaps), vec![10]);
    }

    #[test]
    fn tier_inputs_is_empty_during_startup_grace_and_floor() {
        let snaps = [snap(10, PlaybackStateLabel::Playing)];
        // wall reading not trustworthy yet → tier 1 empty
        assert!(tier_inputs(false, false, &snaps).is_empty());
        // within the heavy-step startup floor → tier 1 empty
        assert!(tier_inputs(true, true, &snaps).is_empty());
        // both → empty
        assert!(tier_inputs(false, true, &snaps).is_empty());
    }

    #[test]
    fn recent_days_from_defaults_and_clamps() {
        assert_eq!(recent_days_from(None), 7);
        assert_eq!(recent_days_from(Some("abc")), 7);
        assert_eq!(recent_days_from(Some("14")), 14);
        assert_eq!(recent_days_from(Some(" 5 ")), 5);
        assert_eq!(recent_days_from(Some("0")), 0);
        // a negative window clamps to 0, never a future window.
        assert_eq!(recent_days_from(Some("-3")), 0);
    }

    #[test]
    fn restricted_tiers_numbers_on_program_then_recent() {
        let on = [10_i64, 11];
        let rec = [12_i64];
        let t = restricted_tiers(&on, &rec);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], (1_i64, &on[..]));
        assert_eq!(t[1], (2_i64, &rec[..]));
    }

    #[test]
    fn restricted_tiers_keeps_empty_lists_with_their_numbers() {
        // Emptiness is handled downstream (skip a tier), NOT by dropping the pair.
        let t = restricted_tiers(&[], &[9_i64]);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], (1_i64, &[][..]));
        assert_eq!(t[1], (2_i64, &[9_i64][..]));
    }
}
