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

/// The title shows this long after a song's `Started` (the show timer)…
pub const TITLE_SHOW_DELAY_MS: u64 = 1500;

/// …and hides this long before the song's end (the hide timer).
pub const TITLE_HIDE_BEFORE_END_MS: u64 = 3500;

/// Whether `position_ms` of a song `duration_ms` long is inside its title
/// window: from the show point to the hide point, the same constants as the
/// show and hide timers. A song too short for a hide timer (≤ 5 s, or an
/// unknown 0 duration) keeps its title to the end, as the timers do
/// (#217 addendum 3).
pub fn title_window_open(position_ms: u64, duration_ms: u64) -> bool {
    position_ms >= TITLE_SHOW_DELAY_MS
        && (duration_ms <= TITLE_SHOW_DELAY_MS + TITLE_HIDE_BEFORE_END_MS
            || position_ms < duration_ms - TITLE_HIDE_BEFORE_END_MS)
}

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

/// Tell the Resolume driver which title SHOULD be on the wall now: the song
/// title of `due` (a video inside its title window), or none (#217
/// addendum 3). The driver owns what the wall shows and acts only on a
/// difference, so a resync is idempotent. The Resolume text comes from the
/// driver's own formatter, the one its ShowTitle state is compared in. The
/// OBS text source gets the title too, as in `push_title`. A failed DB read
/// sends nothing: hiding a title mid-song on a transient error would be the
/// glitch this avoids. Returns the title sent.
pub async fn resync_title(
    pool: &SqlitePool,
    obs_cmd_tx: Option<&mpsc::Sender<ObsCommand>>,
    resolume_tx: &mpsc::Sender<ResolumeCommand>,
    due: Option<i64>,
) -> Option<String> {
    let title = match due {
        None => None,
        Some(video_id) => match get_video_title_info(pool, video_id).await {
            Ok(info) => info
                .map(|(song, artist)| crate::resolume::handlers::format_title_text(&song, &artist))
                .filter(|text| !text.is_empty()),
            Err(e) => {
                tracing::warn!(video_id, %e, "title resync: DB lookup failed — nothing sent");
                return None;
            }
        },
    };
    if let (Some(text), Some(cmd_tx)) = (&title, obs_cmd_tx) {
        let _ = cmd_tx
            .send(ObsCommand::SetTextSource {
                source_name: OBS_TITLE_SOURCE.to_string(),
                text: text.clone(),
            })
            .await;
    }
    let _ = resolume_tx
        .send(ResolumeCommand::Resync {
            title: title.clone(),
        })
        .await;
    title
}

#[cfg(test)]
mod tests {
    use super::{format_title_text, title_window_open};

    /// #217 addendum 3: the window is the show and hide timers' span, from
    /// 1.5 s to 3.5 s before the end.
    #[test]
    fn the_title_window_spans_the_show_and_hide_points() {
        assert!(!title_window_open(1_499, 180_000), "before the show point");
        assert!(title_window_open(1_500, 180_000), "from the show point");
        assert!(title_window_open(176_499, 180_000), "until the hide point");
        assert!(
            !title_window_open(176_500, 180_000),
            "from the hide point on"
        );
    }

    /// A song of 5 s or less has no hide timer (`dur > 5000`), nor does an
    /// unknown 0 duration: its title stays to the end. 5001 ms is the
    /// shortest song with one, at 1501 ms.
    #[test]
    fn a_song_too_short_for_a_hide_timer_keeps_its_title_to_the_end() {
        assert!(title_window_open(4_999, 5_000));
        assert!(title_window_open(60_000, 0), "unknown duration");
        assert!(!title_window_open(1_499, 0));
        assert!(title_window_open(1_500, 5_001));
        assert!(!title_window_open(1_501, 5_001));
    }

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
