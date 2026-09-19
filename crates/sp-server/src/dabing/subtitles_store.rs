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
