//! Glue (#182 D3): build a dub's EN/SK subtitles from the saved Live-session
//! transcripts and persist them as the video's lyrics track, reusing the SAME
//! writer the lyrics worker uses (`lyrics::track_store`) — never a parallel
//! writer. The caller treats a failure as non-fatal (a dub without subtitles is
//! still a finished dub).

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::dabing::subtitles::{self, DubTranscripts};

/// Read `<base>_dub_transcripts.json`, build the subtitle track and store it as
/// the video's lyrics track (`{youtube_id}_lyrics.json` + the `videos` row).
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
    crate::lyrics::track_store::persist_lyrics_track(
        pool,
        cache_dir,
        youtube_id,
        video_id,
        &track,
        crate::lyrics::LYRICS_PIPELINE_VERSION,
    )
    .await?;
    Ok(lines)
}

/// One-shot startup backfill (#182): every finished dub that still lacks the
/// Live-Translate subtitle track gets it built from its saved transcripts JSON —
/// dubs that finished before D3 shipped never ran the builder. Runs ONCE per
/// process (never per tick), so a permanently unusable JSON cannot loop; every
/// failure is a WARN and never touches the dub itself.
#[cfg_attr(test, mutants::skip)] // I/O glue over the unit-tested selector
// (models_dabing::list_ready_dubs_without_subtitles) + build_and_store_subtitles.
pub(crate) async fn backfill_missing_subtitles(pool: &SqlitePool) {
    let pending = match crate::db::models_dabing::list_ready_dubs_without_subtitles(pool).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%e, "dub subtitles backfill: listing failed");
            return;
        }
    };
    for job in pending {
        let audio = std::path::PathBuf::from(&job.audio_file_path);
        let transcripts = crate::stems::dub_transcripts_path(&audio);
        if !transcripts.exists() {
            tracing::info!(
                video_id = job.video_id,
                "dub subtitles backfill: no transcripts JSON — skipped"
            );
            continue;
        }
        let cache_dir = audio.parent().unwrap_or_else(|| Path::new("."));
        match build_and_store_subtitles(
            pool,
            cache_dir,
            &job.youtube_id,
            job.video_id,
            &transcripts,
        )
        .await
        {
            Ok(lines) => tracing::info!(
                video_id = job.video_id,
                lines,
                "dub subtitles backfill: stored EN/SK subtitles"
            ),
            Err(e) => tracing::warn!(
                %e,
                video_id = job.video_id,
                "dub subtitles backfill: build failed (dub unaffected)"
            ),
        }
    }
}

#[cfg(test)]
#[path = "subtitles_store_tests.rs"]
mod tests;
