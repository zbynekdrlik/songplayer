//! #229: the lyrics job asks first. Never for an operator's own ask here (a
//! reprocess or "Nesedí": `lyrics_manual_priority`; a `lyrics_override_text`),
//! and never for a video whose `{yt}_lyrics.json` here is a dub's subtitles
//! (any row of it dub-requested or Live-Translate: the catalog's dub rule,
//! Review Focus 5), and only when this node's audio IS the peer's
//! (`peer::audio`): every track's line timings were measured on the peer's
//! audio, so a song whose audio here is another encode is processed here.
//! The peer's row (`/videos`) must match its catalog and must
//! not be the Live-Translate track; a copy of what the row already serves (the
//! same source at the same version, e.g. the daily full-mix upgrade) is
//! nothing newer: the job runs here. The track is parsed as a typed
//! `LyricsTrack` whose source must be the row's, renamed into
//! `{yt}_lyrics.json`, and the row takes the peer's lyrics columns
//! (`models_peer::adopt_lyrics`).

use std::sync::Arc;

use sqlx::SqlitePool;
use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::kind::{ArtifactKind, Job};
use super::wire::PeerLyrics;
use crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;
use crate::db::models::VideoLyricsRow;
use crate::db::models_peer;
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
        return PeerStep::Local(Some(ex.run_here(job, &row.youtube_id).await));
    }
    match ex.ask(job, &row.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => ex.defer(job, row.id, recheck).await,
        Ask::Fetch(plan) => {
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
                Ok(Adopted::NothingNewer) => {
                    PeerStep::Local(Some(ex.run_here(job, &row.youtube_id).await))
                }
                Err(e) => {
                    ex.after_failed_fetch(job, row.id, &row.youtube_id, &plan.peer.name, &e)
                        .await
                }
            }
        }
    }
}

/// `?1` = the row, `?2` = its YouTube id, `?3` = the Live-Translate source.
const LOCAL_ONLY: &str = "SELECT EXISTS (SELECT 1 FROM videos WHERE id = ?1 \
         AND lyrics_manual_priority != 0) \
     OR EXISTS (SELECT 1 FROM videos WHERE youtube_id = ?2 \
         AND (dub_requested = 1 OR lyrics_source = ?3))";

/// An operator asked THIS node (a reprocess, a "Nesedí") or gave it the
/// text, or this node's `{yt}_lyrics.json` is a dub's subtitles. A failed
/// read keeps the job here (the worker's own path, as before the exchange).
async fn wants_local(pool: &SqlitePool, row: &VideoLyricsRow) -> bool {
    if row
        .lyrics_override_text
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return true;
    }
    sqlx::query_scalar::<_, bool>(LOCAL_ONLY)
        .bind(row.id)
        .bind(&row.youtube_id)
        .bind(SOURCE_LIVE_TRANSLATE)
        .fetch_one(pool)
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
    let artifact = plan.artifact(ArtifactKind::Lyrics)?;
    if !lyrics_size_ok(artifact.size) {
        return Err(PeerError::BadResponse(format!(
            "a lyrics track over {MAX_LYRICS_BYTES} bytes"
        )));
    }
    let video = ex.client.video(&plan.peer, &row.youtube_id).await?;
    let lyrics = video
        .lyrics
        .ok_or_else(|| PeerError::BadResponse("the peer's row has no lyrics".into()))?;
    if lyrics.pipeline_version != artifact.version || lyrics.source == SOURCE_LIVE_TRANSLATE {
        return Err(PeerError::BadResponse(
            "the peer's lyrics row does not match its catalog".into(),
        ));
    }
    if serves_the_same(&ex.pool, row.id, &lyrics).await? {
        return Ok(Adopted::NothingNewer);
    }
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
    let json = ex.cache_dir.join(format!("{}_lyrics.json", row.youtube_id));
    tokio::fs::rename(&part, &json).await?;
    models_peer::adopt_lyrics(&ex.pool, row.id, &lyrics).await?;
    Ok(Adopted::Track)
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
