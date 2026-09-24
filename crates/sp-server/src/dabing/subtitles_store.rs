//! Glue (#182 D3): build a dub's EN/SK subtitles from the saved Live-session
//! transcripts and persist them as the video's lyrics track, reusing the SAME
//! writer the lyrics worker uses (`lyrics::track_store`) — never a parallel
//! writer. The caller treats a failure as non-fatal (a dub without subtitles is
//! still a finished dub).
//!
//! The stored track carries the builder version that made it
//! (`dub_subtitles_builder_version`, #184 H5), so the startup backfill rebuilds a
//! track made by an older builder from its saved transcripts — a pairing or
//! grouping change reaches every stored dub with the next deploy, no re-dub.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::dabing::subtitles::{self, DubTranscripts};
use crate::db::models_dabing::DubSubtitleBackfill;

/// The builder version of a stored track written before the field existed.
const LEGACY_BUILDER_VERSION: u32 = 1;

/// The stored dub subtitle JSON: the track's own fields plus the builder version
/// (#184 H5). It still reads as a plain `LyricsTrack` (unknown fields are
/// ignored), so the wall, the dashboard and the lyrics loader are unaffected.
#[derive(Serialize)]
struct StoredDubTrack<'a> {
    #[serde(flatten)]
    track: &'a sp_core::lyrics::LyricsTrack,
    dub_subtitles_builder_version: u32,
}

/// Only the builder version of a stored track.
#[derive(Deserialize)]
struct StoredBuilderVersion {
    #[serde(default)]
    dub_subtitles_builder_version: Option<u32>,
}

/// The builder version a stored track JSON was made by: its
/// `dub_subtitles_builder_version`, or [`LEGACY_BUILDER_VERSION`] when the field
/// is absent or the JSON is unreadable (both mean "rebuild it").
fn stored_builder_version(json: &[u8]) -> u32 {
    serde_json::from_slice::<StoredBuilderVersion>(json)
        .ok()
        .and_then(|s| s.dub_subtitles_builder_version)
        .unwrap_or(LEGACY_BUILDER_VERSION)
}

/// Whether a track made by builder `stored` must be rebuilt: only when it is
/// OLDER than the current builder.
fn is_stale(stored: u32) -> bool {
    stored < subtitles::DUB_SUBTITLES_BUILDER_VERSION
}

/// Read `<base>_dub_transcripts.json`, build the subtitle track and store it as
/// the video's lyrics track (`{youtube_id}_lyrics.json` + the `videos` row),
/// stamped with the current builder version.
/// Returns the number of subtitle lines stored — `0` means the transcript had no
/// usable SK timing, so nothing was written.
#[cfg_attr(test, mutants::skip)] // I/O glue (read file → parse → persist); the pure
// transcripts_to_track it wraps is exhaustively unit-tested in subtitles_tests.rs.
pub(crate) async fn build_and_store_subtitles(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: &str,
    video_id: i64,
    transcripts_path: &Path,
) -> Result<usize> {
    let bytes = tokio::fs::read(transcripts_path)
        .await
        .with_context(|| format!("reading {}", transcripts_path.display()))?;
    let transcripts: DubTranscripts =
        serde_json::from_slice(&bytes).context("parsing dub transcripts JSON")?;

    let mut track = subtitles::transcripts_to_track(&transcripts);
    if track.lines.is_empty() {
        return Ok(0);
    }
    track.version = crate::lyrics::LYRICS_PIPELINE_VERSION;

    let lines = track.lines.len();
    let stored = StoredDubTrack {
        track: &track,
        dub_subtitles_builder_version: subtitles::DUB_SUBTITLES_BUILDER_VERSION,
    };
    crate::lyrics::track_store::persist_lyrics_json(
        pool,
        cache_dir,
        youtube_id,
        video_id,
        &stored,
        &track.source,
        crate::lyrics::LYRICS_PIPELINE_VERSION,
    )
    .await?;
    Ok(lines)
}

/// One-shot startup backfill (#182, extended by #184 H5). Every finished dub
/// that still lacks the Live-Translate subtitle track gets it built from its
/// saved transcripts JSON (dubs that finished before D3 shipped never ran the
/// builder), and every stored subtitle track made by an OLDER builder
/// ([`is_stale`]) is rebuilt the same way. Runs ONCE per process (never per
/// tick), so a permanently unusable JSON cannot loop; every failure is a WARN and
/// never touches the dub itself.
#[cfg_attr(test, mutants::skip)] // I/O glue over the unit-tested selectors + the
// pure is_stale/stored_builder_version; covered end-to-end in subtitles_store_tests.rs.
pub(crate) async fn backfill_missing_subtitles(pool: &SqlitePool) {
    let missing = match crate::db::models_dabing::list_ready_dubs_without_subtitles(pool).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%e, "dub subtitles backfill: listing failed");
            return;
        }
    };
    for job in &missing {
        rebuild_subtitles(pool, job, "stored EN/SK subtitles").await;
    }

    let with_track = match crate::db::models_dabing::list_ready_dubs_with_subtitles(pool).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%e, "dub subtitles rebuild: listing failed");
            return;
        }
    };
    for job in &with_track {
        let audio = Path::new(&job.audio_file_path);
        let cache_dir = audio.parent().unwrap_or_else(|| Path::new("."));
        let track_path = cache_dir.join(format!("{}_lyrics.json", job.youtube_id));
        // A track file that cannot be read counts as a legacy (stale) track.
        let stored = tokio::fs::read(&track_path)
            .await
            .map(|b| stored_builder_version(&b))
            .unwrap_or(LEGACY_BUILDER_VERSION);
        if !is_stale(stored) {
            continue;
        }
        tracing::info!(
            video_id = job.video_id,
            stored_version = stored,
            builder_version = subtitles::DUB_SUBTITLES_BUILDER_VERSION,
            "dub subtitles rebuild: stored track is from an older builder — rebuilding"
        );
        rebuild_subtitles(pool, job, "rebuilt EN/SK subtitles").await;
    }
}

/// Build + store one dub's subtitles from its saved transcripts JSON; log the
/// outcome (`done` names what a success did). A missing JSON is an INFO skip, a
/// failure a WARN — never an error for the dub.
#[cfg_attr(test, mutants::skip)] // I/O glue; covered by subtitles_store_tests.rs.
async fn rebuild_subtitles(pool: &SqlitePool, job: &DubSubtitleBackfill, done: &str) {
    let audio = Path::new(&job.audio_file_path);
    let transcripts = crate::stems::dub_transcripts_path(audio);
    if !transcripts.exists() {
        tracing::info!(
            video_id = job.video_id,
            "dub subtitles backfill: no transcripts JSON — skipped"
        );
        return;
    }
    let cache_dir = audio.parent().unwrap_or_else(|| Path::new("."));
    match build_and_store_subtitles(pool, cache_dir, &job.youtube_id, job.video_id, &transcripts)
        .await
    {
        Ok(lines) => tracing::info!(
            video_id = job.video_id,
            lines,
            "dub subtitles backfill: {done}"
        ),
        Err(e) => tracing::warn!(
            %e,
            video_id = job.video_id,
            "dub subtitles backfill: build failed (dub unaffected)"
        ),
    }
}

#[cfg(test)]
#[path = "subtitles_store_tests.rs"]
mod tests;
