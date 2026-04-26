//! Title-text push helpers shared by the engine.
//!
//! Two call sites in `playback/mod.rs` push the same song title to the
//! same downstreams (OBS text source + Resolume `#sp-title` clips):
//!
//! * The 1.5 s post-`Started` timer task (in `handle_pipeline_event`)
//! * The scene-go-on refresh path (in `handle_scene_change`)
//!
//! Extracting the body keeps both sites consistent and stops `mod.rs`
//! from creeping past the 1000-line cap.

use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::obs::ObsCommand;
use crate::resolume::ResolumeCommand;

/// OBS text source name used for the fallback title display (in the
/// CG OVERLAY scene). Must match the source name in OBS exactly.
pub const OBS_TITLE_SOURCE: &str = "#sp-title";

/// Format a title for display: `"<song> - <artist>"`, falling back to
/// whichever side is non-empty when the other is missing.
pub fn format_title_text(song: &str, artist: &str) -> String {
    if artist.is_empty() {
        song.to_string()
    } else if song.is_empty() {
        artist.to_string()
    } else {
        format!("{song} - {artist}")
    }
}

/// Look up a video's `(song, artist)` for title display.
pub async fn get_video_title_info(
    pool: &SqlitePool,
    video_id: i64,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let row = sqlx::query("SELECT song, artist FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|r| {
        use sqlx::Row;
        let song: String = r.get::<Option<String>, _>("song").unwrap_or_default();
        let artist: String = r.get::<Option<String>, _>("artist").unwrap_or_default();
        (song, artist)
    }))
}

/// Push the song title to OBS (if configured) and Resolume.
///
/// Returns `true` if a title was pushed, `false` if the video had no
/// title info on disk (silent, mirrors prior 1.5 s timer behaviour).
/// Idempotent — Resolume's A/B crossfade no-ops on same-text writes.
pub async fn push_title(
    pool: &SqlitePool,
    obs_cmd_tx: Option<&mpsc::Sender<ObsCommand>>,
    resolume_tx: &mpsc::Sender<ResolumeCommand>,
    video_id: i64,
) -> bool {
    let Ok(Some((song, artist))) = get_video_title_info(pool, video_id).await else {
        return false;
    };
    let text = format_title_text(&song, &artist);
    if let Some(cmd_tx) = obs_cmd_tx {
        let _ = cmd_tx
            .send(ObsCommand::SetTextSource {
                source_name: OBS_TITLE_SOURCE.to_string(),
                text,
            })
            .await;
    }
    let _ = resolume_tx
        .send(ResolumeCommand::ShowTitle { song, artist })
        .await;
    true
}

#[cfg(test)]
mod tests {
    use super::format_title_text;

    #[test]
    fn formats_song_and_artist() {
        assert_eq!(format_title_text("Song", "Artist"), "Song - Artist");
    }

    #[test]
    fn empty_artist_yields_song_only() {
        assert_eq!(format_title_text("Song", ""), "Song");
    }

    #[test]
    fn empty_song_yields_artist_only() {
        assert_eq!(format_title_text("", "Artist"), "Artist");
    }

    #[test]
    fn both_empty_yields_empty() {
        assert_eq!(format_title_text("", ""), "");
    }
}
