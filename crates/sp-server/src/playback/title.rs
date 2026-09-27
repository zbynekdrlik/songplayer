//! The engine's song-title helpers: the one title formatter, the song's
//! `TitleClock` (#217 addendum 3), and the two ways the title reaches OBS's
//! `#sp-title` text source and Resolume's `#sp-title` clips:
//!
//! * `push_title`: the show timer's ShowTitle (`title_timers.rs`);
//! * `title_text` + `send_resync`: a recovery's or a scene-on's `Resync`
//!   (`recovery.rs::resync_wall_title`).

use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::obs::ObsCommand;
use crate::resolume::ResolumeCommand;

/// The one title formatter: the Resolume driver compares its title state in
/// this text, and the OBS text source shows the same (#217 addendum 3).
pub use crate::resolume::handlers::format_title_text;

/// OBS text source name used for the fallback title display (in the
/// CG OVERLAY scene). Must match the source name in OBS exactly.
pub const OBS_TITLE_SOURCE: &str = "#sp-title";

/// The title shows this long after a song's `Started`…
pub const TITLE_SHOW_DELAY_MS: u64 = 1500;

/// …and hides this long before the song's end.
pub const TITLE_HIDE_BEFORE_END_MS: u64 = 3500;

/// The one clock of a song's title window (#217 addendum 3): the instants
/// the title timers sleep until, fixed at the song's `Started`. A recovery
/// or a scene-on reads the SAME instants, so its `Resync` never contradicts a
/// timer. (It read the decoder position before, which the pipeline reports
/// every 500 ms: near each boundary the two disagreed, and a queued Resync
/// could keep a title into the next song or hide a title just shown.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleClock {
    /// The song this clock belongs to.
    pub video_id: i64,
    /// When the title shows: `Started` + 1.5 s.
    pub show_at: Instant,
    /// When it hides: 3.5 s before the end. `None` for a song of 5 s or less
    /// (or an unknown 0 duration): it keeps its title to the end.
    pub hide_at: Option<Instant>,
}

impl TitleClock {
    /// The clock of `video_id`, started at `started_at`, `duration_ms` long.
    pub fn new(video_id: i64, started_at: Instant, duration_ms: u64) -> Self {
        let hide_at = if duration_ms > TITLE_SHOW_DELAY_MS + TITLE_HIDE_BEFORE_END_MS {
            Some(started_at + Duration::from_millis(duration_ms - TITLE_HIDE_BEFORE_END_MS))
        } else {
            None
        };
        Self {
            video_id,
            show_at: started_at + Duration::from_millis(TITLE_SHOW_DELAY_MS),
            hide_at,
        }
    }

    /// Whether the title is due at `now`: from `show_at`, before `hide_at`.
    pub fn open_at(&self, now: Instant) -> bool {
        now >= self.show_at && self.hide_at.is_none_or(|hide_at| now < hide_at)
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

/// The wall title of `video_id` (`format_title_text`): `None` when the
/// video has no row, or neither a song nor an artist.
pub async fn title_text(pool: &SqlitePool, video_id: i64) -> Result<Option<String>, sqlx::Error> {
    Ok(get_video_title_info(pool, video_id)
        .await?
        .map(|(song, artist)| format_title_text(&song, &artist))
        .filter(|text| !text.is_empty()))
}

/// Tell the wall which title SHOULD be up (`None` = no title, #217
/// addendum 3). The Resolume `Resync` goes first: the driver owns what the
/// wall shows and acts only on a difference. The OBS text source then gets
/// the same title, or is cleared as the hide timer clears it. The OBS send
/// can stall while cg OBS is away, so the Resync must not wait behind it.
pub async fn send_resync(
    obs_cmd_tx: Option<&mpsc::Sender<ObsCommand>>,
    resolume_tx: &mpsc::Sender<ResolumeCommand>,
    title: Option<String>,
) {
    let _ = resolume_tx
        .send(ResolumeCommand::Resync {
            title: title.clone(),
        })
        .await;
    if let Some(cmd_tx) = obs_cmd_tx {
        let _ = cmd_tx
            .send(ObsCommand::SetTextSource {
                source_name: OBS_TITLE_SOURCE.to_string(),
                text: title.unwrap_or_default(),
            })
            .await;
    }
}

#[cfg(test)]
#[path = "title_tests.rs"]
mod tests;
