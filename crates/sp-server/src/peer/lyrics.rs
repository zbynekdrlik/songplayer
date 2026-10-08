//! #229: the lyrics job asks first. Never while an operator's mark here is
//! one the lyrics queue acts on, on any row of the video (`kept_local`: a
//! `lyrics_override_text` on a row of an active playlist, a reprocess or
//! "Nesedí" flag on such a row that is not parked), and never for a video
//! whose `{yt}_lyrics.json` here is a dub's subtitles (any row of it
//! dub-requested or Live-Translate: the catalog's dub rule, Review
//! Focus 5), and only when this node's audio IS the peer's
//! (`peer::audio`): every track's line timings were measured on the peer's
//! audio, so a song whose audio here is another encode is processed here.
//! The peer's row (`/videos`) must match its catalog and must
//! not be the Live-Translate track; a copy of what the row already serves (the
//! same source at the same version, e.g. the daily full-mix upgrade) is
//! nothing newer: the job runs here. A video whose track here stands in for
//! a peer's copy (`peer::standin`) takes no copy through this hook: the
//! copy goes into EVERY row of it (they serve one file), whatever its
//! source, through the stand-in's own look, made due now; the row is put
//! back. The track is parsed as a typed
//! `LyricsTrack` whose source must be the row's, renamed into
//! `{yt}_lyrics.json`, and the row takes the peer's lyrics columns
//! (`models_peer::adopt_lyrics`).
//!
//! #229 item C: while this node's paid AI is off (`paid_ai`), every "run
//! here" of this hook is a hold instead (`Exchange::local` / `hold`): only a
//! peer's copy is taken, and the row is picked again later, no attempt.

use std::sync::Arc;

use sqlx::SqlitePool;
use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::kind::{ArtifactKind, Job};
use super::wire::{Artifact, PeerLyrics};
use crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;
use crate::db::models::VideoLyricsRow;
use crate::db::models_peer;
use crate::lyrics::queue_sql::LYRICS_NOT_PARKED;
use crate::metadata::health::bounded_error;

/// What [`adopt`] did with the peer's lyrics.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Adopted {
    /// The peer's track is in place and the row records it.
    Track,
    /// The row already serves that very track: nothing was fetched.
    NothingNewer,
}

/// The lyrics worker's hook: fetch, wait or process here.
pub async fn first(ex: Option<&Arc<Exchange>>, row: &VideoLyricsRow) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    let job = Job::Lyrics;
    if wants_local(&ex.pool, row).await {
        return ex.local(job, row.id, &row.youtube_id).await;
    }
    match ex.ask(job, &row.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Held => ex.hold(job, row.id, &row.youtube_id).await,
        Ask::Wait { recheck, .. } => ex.defer(job, row.id, recheck).await,
        Ask::Fetch(plan) => {
            // A stand-in takes the copy into every row of the video through
            // its own look (`peer::standin`; review rounds 3 and 4).
            if let Some(peer) = ex.standin_peer(job, &row.youtube_id).await {
                return ex
                    .hand_to_standin(job, row.id, &row.youtube_id, &peer)
                    .await;
            }
            if let Some(step) = ex
                .unless_peers_audio(job, &plan, row.id, &row.youtube_id)
                .await
            {
                return step;
            }
            match adopt(ex, row, &plan).await {
                Ok(Adopted::Track) => {
                    ex.fetched(job, &row.youtube_id, &plan.peer.name, &plan.artifacts)
                        .await;
                    PeerStep::Done
                }
                Ok(Adopted::NothingNewer) => ex.local(job, row.id, &row.youtube_id).await,
                Err(e) => {
                    ex.after_failed_fetch(job, row.id, &row.youtube_id, &plan.peer.name, &e)
                        .await
                }
            }
        }
    }
}

/// Binds, in text order: the YouTube id, the Live-Translate source, the
/// current pipeline version (`LYRICS_NOT_PARKED`'s). Bare `?` only: sqlx
/// binds by position, and a bare `?` after `?1` / `?2` would take the
/// FIRST value (review round 9, `rust-workspace.md`). The dub and the
/// Live-Translate track count on any row; an operator's mark only where the
/// lyrics queue acts on it (review rounds 7-8): a text on a row of an
/// active playlist (the worker makes no other row's lyrics), a reprocess
/// flag on such a row that is not parked (bucket 1's own condition: the
/// queue never takes another, so its flag is never cleared).
fn local_only_sql() -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM videos v WHERE v.youtube_id = ? \
         AND (v.dub_requested = 1 OR v.lyrics_source = ? \
              OR (EXISTS (SELECT 1 FROM playlists p \
                          WHERE p.id = v.playlist_id AND p.is_active = 1) \
                  AND (TRIM(COALESCE(v.lyrics_override_text, '')) != '' \
                       OR (v.lyrics_manual_priority != 0 AND {LYRICS_NOT_PARKED})))))"
    )
}

/// The lyrics of `youtube_id` stay this node's own: an operator asked THIS
/// node (a reprocess, a "Nesedí") or gave it the text, where the lyrics
/// queue acts on it, or its `{yt}_lyrics.json` is a dub's subtitles — on
/// ANY row of the video, whose rows serve that one file (review round 5:
/// the hook read the ask and the text on the asked row only). The hook and
/// the stand-in's look ask it (V31's back-fill is a literal copy, without
/// the version: a stand-in it made for a video with a pending reprocess is
/// dropped by the look).
pub(crate) async fn kept_local(pool: &SqlitePool, youtube_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&local_only_sql())
        .bind(youtube_id)
        .bind(SOURCE_LIVE_TRANSLATE)
        .bind(i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION))
        .fetch_one(pool)
        .await
}

/// [`kept_local`] for the hook's row; a failed read keeps the job here (the
/// worker's own path, as before the exchange).
async fn wants_local(pool: &SqlitePool, row: &VideoLyricsRow) -> bool {
    kept_local(pool, &row.youtube_id)
        .await
        .inspect_err(|e| warn!(video_id = row.id, %e, "exchange: reading the lyrics row failed"))
        .unwrap_or(true)
}

/// The largest lyrics track this node takes from a peer (a real one is a few
/// hundred KiB; the part is read whole to check it): 16 MiB.
const MAX_LYRICS_BYTES: u64 = 16_777_216;

/// A peer's lyrics artifact of `size` bytes is one this node fetches.
fn lyrics_size_ok(size: u64) -> bool {
    size <= MAX_LYRICS_BYTES
}

/// The peer's track into `{yt}_lyrics.json` with its row, or nothing newer.
pub(crate) async fn adopt(
    ex: &Exchange,
    row: &VideoLyricsRow,
    plan: &FetchPlan,
) -> Result<Adopted, PeerError> {
    let (artifact, lyrics) = peer_row(ex, &row.youtube_id, plan).await?;
    if serves_the_same(&ex.pool, row.id, &lyrics).await? {
        return Ok(Adopted::NothingNewer);
    }
    place(ex, &row.youtube_id, plan, artifact, &lyrics).await?;
    models_peer::adopt_lyrics(&ex.pool, row.id, &lyrics).await?;
    Ok(Adopted::Track)
}

/// The plan's lyrics artifact and the peer's lyrics row of `youtube_id`,
/// checked before any transfer: a track within [`MAX_LYRICS_BYTES`], a row
/// at the artifact's pipeline version that is not the Live-Translate track.
pub(crate) async fn peer_row<'p>(
    ex: &Exchange,
    youtube_id: &str,
    plan: &'p FetchPlan,
) -> Result<(&'p Artifact, PeerLyrics), PeerError> {
    let artifact = plan.artifact(ArtifactKind::Lyrics)?;
    if !lyrics_size_ok(artifact.size) {
        return Err(PeerError::BadResponse(format!(
            "a lyrics track over {MAX_LYRICS_BYTES} bytes"
        )));
    }
    let video = ex.client.video(&plan.peer, youtube_id).await?;
    let lyrics = video
        .lyrics
        .ok_or_else(|| PeerError::BadResponse("the peer's row has no lyrics".into()))?;
    if lyrics.pipeline_version != artifact.version || lyrics.source == SOURCE_LIVE_TRANSLATE {
        return Err(PeerError::BadResponse(
            "the peer's lyrics row does not match its catalog".into(),
        ));
    }
    Ok((artifact, lyrics))
}

/// The peer's track fetched, parsed as a `LyricsTrack` whose source is the
/// peer's row's (`lyrics`), and renamed into `{yt}_lyrics.json`. A refused
/// part is deleted.
pub(crate) async fn place(
    ex: &Exchange,
    youtube_id: &str,
    plan: &FetchPlan,
    artifact: &Artifact,
    lyrics: &PeerLyrics,
) -> Result<(), PeerError> {
    let part = ex.fetch(&plan.peer, artifact).await?;
    let bytes = tokio::fs::read(&part).await?;
    let source = match serde_json::from_slice::<sp_core::lyrics::LyricsTrack>(&bytes) {
        Ok(track) => track.source,
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(PeerError::BadResponse(format!(
                "not a lyrics track (line {}, column {})",
                e.line(),
                e.column()
            )));
        }
    };
    if source != lyrics.source {
        let _ = tokio::fs::remove_file(&part).await;
        // Both texts are the peer's: cut to the bounded error size.
        let text = format!(
            "the track's source {source:?} is not the row's {:?}",
            lyrics.source
        );
        return Err(PeerError::BadResponse(bounded_error(&text)));
    }
    let json = ex.cache_dir.join(format!("{youtube_id}_lyrics.json"));
    tokio::fs::rename(&part, &json).await?;
    Ok(())
}

/// The row already serves this very track: the same source, the same version.
async fn serves_the_same(
    pool: &SqlitePool,
    video_id: i64,
    lyrics: &PeerLyrics,
) -> Result<bool, sqlx::Error> {
    let row: Option<(i64, Option<String>, i64)> = sqlx::query_as(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    Ok(matches!(
        row,
        Some((1, Some(source), version))
            if source == lyrics.source && version == i64::from(lyrics.pipeline_version)
    ))
}

#[cfg(test)]
#[path = "lyrics_tests.rs"]
mod tests;
