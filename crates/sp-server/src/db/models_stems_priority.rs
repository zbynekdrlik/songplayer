//! #195: the stems queue serves what is IN USE first. The plain
//! `models_stems::get_next_video_for_stems` picks by `stem_manual_priority DESC,
//! id ASC` only — so the playlist on OBS program can wait days behind low-id
//! videos of unused playlists. This module wraps that query in a tiered selector
//! (and mirrors the tier rank onto the panel's queue position) without touching
//! the eligibility predicate or the stem separation itself.
//!
//! Tiers (first hit wins):
//!   0. manual/dub priority (`stem_manual_priority > 0`) on ANY playlist — an
//!      explicit operator ask always wins.
//!   1. the on-program playlist(s).
//!   2. playlists played in the last `stems_recent_days` days.
//!   3. today's unrestricted oldest-first query.
//!
//! Inner order inside every tier is unchanged (`stem_manual_priority DESC,
//! id ASC`); an empty id list SKIPS its tier (never `IN ()`).
//!
//! In its own sibling module because `db/models.rs` is at the 1000-line cap
//! (`db/models_ndi.rs` precedent). The playlist-id tier inputs are built by the
//! worker-agnostic free fns in `crate::stems::queue_tiers`.

use sqlx::{Row, SqlitePool};

use crate::db::models_stems::StemJob;

/// The queue-eligibility predicate, byte-identical to
/// `get_next_video_for_stems` / `queue_position`: normalized with an audio
/// sidecar, not already `done`/`unsupported`, and (if previously `failed`) past
/// its backoff. Kept as one const so every tier query gates on the same rows.
pub(crate) const STEM_ELIGIBLE_PRED: &str = "normalized = 1 AND audio_file_path IS NOT NULL \
     AND (stem_status IS NULL OR stem_status = 'failed') \
     AND (stem_next_attempt_at IS NULL \
          OR stem_next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))";

/// Tier index for the manual-priority tier (any playlist).
const MANUAL_TIER: i64 = 0;
/// Tier index for the unrestricted fallback (the `ELSE` of the tier CASE).
const FALLBACK_TIER: i64 = 3;

/// Render a slice of playlist ids as a SQL `IN`-list body (`"1, 2, 3"`). The ids
/// are i64 read from the DB, so inlining them is injection-safe. Pure +
/// unit-tested (mutation-scored); callers guarantee a non-empty slice.
fn int_list(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the tier-rank `CASE` expression for `queue_position` from the restricted
/// tier list (`(tier_index, playlist_ids)` pairs, from
/// `queue_tiers::restricted_tiers`). Tier 0 is manual priority; each non-empty
/// restricted list contributes its `WHEN playlist_id IN (…) THEN <tier>`; an
/// empty list is skipped (never `IN ()`); everything else falls to
/// `FALLBACK_TIER`. Pure + unit-tested (mutation-scored).
fn tier_case_sql(restricted: &[(i64, &[i64])]) -> String {
    let mut sql = format!("CASE WHEN stem_manual_priority > 0 THEN {MANUAL_TIER}");
    for (tier, ids) in restricted {
        if ids.is_empty() {
            continue; // empty tier is skipped — never `IN ()`
        }
        sql.push_str(&format!(
            " WHEN playlist_id IN ({}) THEN {tier}",
            int_list(ids)
        ));
    }
    sql.push_str(&format!(" ELSE {FALLBACK_TIER} END"));
    sql
}

/// Map a selected `videos` row to a [`StemJob`] (same columns as
/// `get_next_video_for_stems`).
fn row_to_stem_job(r: &sqlx::sqlite::SqliteRow) -> StemJob {
    StemJob {
        video_id: r.get("id"),
        youtube_id: r.get("youtube_id"),
        audio_file_path: r.get("audio_file_path"),
        duration_ms: r.get("duration_ms"),
        song: r.get("song"),
        artist: r.get("artist"),
    }
}

/// Tier 0: the next eligible song with `stem_manual_priority > 0`, on ANY
/// playlist. `None` when no manual-priority row is due.
///
/// mutants::skip — pure SQL (WHERE/ORDER) + thin bind/map/Ok glue; the tier
/// ordering is exercised end-to-end by the `get_next_stem_job` integration tests.
/// MAINTAINERS: if you add ANY non-SQL branch here, REMOVE this skip (the
/// justification only holds while the body stays pure SQL + glue — see
/// `lyrics::reprocess::fetch_bucket_null`).
#[cfg_attr(test, mutants::skip)]
pub async fn next_stem_manual(pool: &SqlitePool) -> Result<Option<StemJob>, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT id, youtube_id, audio_file_path, duration_ms, song, artist \
         FROM videos \
         WHERE {STEM_ELIGIBLE_PRED} AND stem_manual_priority > 0 \
         ORDER BY stem_manual_priority DESC, id ASC \
         LIMIT 1"
    ))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| row_to_stem_job(&r)))
}

/// Tiers 1/2: the next eligible song whose playlist is in `ids`, oldest-first
/// within the same priority. An EMPTY `ids` skips the tier (returns `None`,
/// never emits `IN ()`). NOT `mutants::skip`'d — the empty guard is a real branch
/// (covered by the direct empty + non-empty tests).
pub async fn next_stem_for_playlists(
    pool: &SqlitePool,
    ids: &[i64],
) -> Result<Option<StemJob>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(None); // empty tier is skipped — never `IN ()`
    }
    let row = sqlx::query(&format!(
        "SELECT id, youtube_id, audio_file_path, duration_ms, song, artist \
         FROM videos \
         WHERE {STEM_ELIGIBLE_PRED} AND playlist_id IN ({}) \
         ORDER BY stem_manual_priority DESC, id ASC \
         LIMIT 1",
        int_list(ids)
    ))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| row_to_stem_job(&r)))
}

/// The tiered stems selector (#195). Consults the tiers in order — manual (tier
/// 0, any playlist) → on-program (tier 1) → recently played (tier 2) → today's
/// unrestricted query (tier 3) — returning the first hit. `on_program` and
/// `recent` are the playlist-id lists the worker builds from the in-process NDI
/// health registry + one `play_history` query (`queue_tiers::compute_tier_inputs`);
/// an empty list skips its tier.
pub async fn get_next_stem_job(
    pool: &SqlitePool,
    on_program: &[i64],
    recent: &[i64],
) -> Result<Option<StemJob>, sqlx::Error> {
    // Tier 0: an explicit manual/dub priority ask wins on ANY playlist.
    if let Some(job) = next_stem_manual(pool).await? {
        return Ok(Some(job));
    }
    // Tiers 1..: on-program, then recently played. First hit wins; an empty id
    // list skips its tier.
    for (_tier, ids) in crate::stems::queue_tiers::restricted_tiers(on_program, recent) {
        if let Some(job) = next_stem_for_playlists(pool, ids).await? {
            return Ok(Some(job));
        }
    }
    // Tier 3: today's unrestricted oldest-first query.
    crate::db::models_stems::get_next_video_for_stems(pool).await
}

/// 1-based position of `video_id` in the TIERED stem queue, or `None` when the
/// row is not queue-eligible. Ranks by `(tier ASC, stem_manual_priority DESC,
/// id ASC)` — the exact order [`get_next_stem_job`] picks in — so the panel's
/// "vo fronte (N.)" stays truthful once the queue is served in-use-first.
/// `on_program`/`recent` are the same tier inputs the selector uses (empty lists
/// → the unrestricted, oldest-first position, matching the legacy 2-arg form).
///
/// mutants::skip — the ranking lives in the inlined `tier_case_sql` CASE (a SQL
/// string cargo-mutants cannot mutate) + the SQL COUNT; the tier structure is
/// unit-tested in `tier_case_sql`/`restricted_tiers`, and the `+1` / eligibility
/// branch by the exact-position + ineligible-row integration tests below.
/// MAINTAINERS: if you add a non-SQL branch beyond the eligibility early-return,
/// REMOVE this skip.
#[cfg_attr(test, mutants::skip)]
pub async fn queue_position(
    pool: &SqlitePool,
    video_id: i64,
    on_program: &[i64],
    recent: &[i64],
) -> Result<Option<i64>, sqlx::Error> {
    let restricted = crate::stems::queue_tiers::restricted_tiers(on_program, recent);
    let case = tier_case_sql(&restricted);

    // This row's (priority, tier) — `None` when it is not queue-eligible.
    let this: Option<(i64, i64)> = sqlx::query_as(&format!(
        "SELECT stem_manual_priority AS prio, ({case}) AS tier \
         FROM videos WHERE id = ? AND {STEM_ELIGIBLE_PRED}"
    ))
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let Some((prio, tier)) = this else {
        return Ok(None);
    };

    // Count eligible rows that sort BEFORE it under (tier ASC, prio DESC, id ASC).
    let before: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM videos \
         WHERE {STEM_ELIGIBLE_PRED} \
           AND ( ({case}) < ? \
                 OR ( ({case}) = ? \
                      AND ( stem_manual_priority > ? \
                            OR (stem_manual_priority = ? AND id < ?) ) ) )"
    ))
    .bind(tier)
    .bind(tier)
    .bind(prio)
    .bind(prio)
    .bind(video_id)
    .fetch_one(pool)
    .await?;
    Ok(Some(before + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_list_joins_with_comma() {
        assert_eq!(int_list(&[1, 2, 3]), "1, 2, 3");
        assert_eq!(int_list(&[7]), "7");
        assert_eq!(int_list(&[]), "");
        assert_eq!(int_list(&[10, -1]), "10, -1");
    }

    #[test]
    fn tier_case_sql_includes_nonempty_tiers() {
        let on = [1_i64, 2];
        let rec = [3_i64];
        let restricted = [(1_i64, &on[..]), (2_i64, &rec[..])];
        assert_eq!(
            tier_case_sql(&restricted),
            "CASE WHEN stem_manual_priority > 0 THEN 0 WHEN playlist_id IN (1, 2) THEN 1 WHEN playlist_id IN (3) THEN 2 ELSE 3 END"
        );
    }

    #[test]
    fn tier_case_sql_skips_an_empty_tier_but_keeps_its_number() {
        // on-program empty (skipped), recent kept as tier 2.
        let rec = [5_i64];
        let restricted = [(1_i64, &[][..]), (2_i64, &rec[..])];
        assert_eq!(
            tier_case_sql(&restricted),
            "CASE WHEN stem_manual_priority > 0 THEN 0 WHEN playlist_id IN (5) THEN 2 ELSE 3 END"
        );
    }

    #[test]
    fn tier_case_sql_with_no_restricted_tiers_is_manual_vs_rest() {
        assert_eq!(
            tier_case_sql(&[]),
            "CASE WHEN stem_manual_priority > 0 THEN 0 ELSE 3 END"
        );
    }
}
