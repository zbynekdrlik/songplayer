//! The engine's song-title helpers: the one title formatter, the song's
//! `TitleClock` (#217 addendum 3), and the three ways the title reaches
//! Resolume's `#sp-title` clips and OBS's `#sp-title` text source:
//!
//! * `push_title` / `push_hide`: the show / hide timer's ShowTitle /
//!   HideTitle (`title_timers.rs`);
//! * `title_text` + `send_resync`: the `Resync` of a recovery, a scene-on or
//!   a Play (`recovery.rs::decide_wall_title`).
//!
//! All three send Resolume first and never wait for cg OBS
//! (`send_obs_title`, review round 4).

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
/// the title timers sleep until, fixed at the song's `Started` and moved by
/// a seek ([`seeked`](Self::seeked)). A recovery or a scene-on reads the SAME
/// instants, so its `Resync` never contradicts a timer. (It read the decoder
/// position before, which the pipeline reports every 500 ms: near each
/// boundary the two disagreed, and a queued Resync could keep a title into
/// the next song or hide a title just shown.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleClock {
    /// The song this clock belongs to.
    pub video_id: i64,
    /// When the title shows: `Started` + 1.5 s.
    pub show_at: Instant,
    /// When it hides: 3.5 s before the song's end. `None` for a song of 5 s
    /// or less (or an unknown 0 duration): it keeps its title to the end.
    pub hide_at: Option<Instant>,
}

impl TitleClock {
    /// The clock of `video_id`, a `duration_ms` song whose `Started` came at
    /// `started_at`, `start_ms` into it: where `Started` says the song really
    /// starts (#217: 0, or a resume's position; 0 again when the resume's
    /// seek failed and the song plays from its start). The title shows 1.5 s
    /// after the start and hides 3.5 s before the song's REAL end, so a
    /// resume 5 s or less before the end has no window (`shows`, review
    /// round 4).
    pub fn new(video_id: i64, started_at: Instant, duration_ms: u64, start_ms: u64) -> Self {
        Self {
            video_id,
            show_at: started_at + Duration::from_millis(TITLE_SHOW_DELAY_MS),
            hide_at: hide_point(started_at, duration_ms, start_ms),
        }
    }

    /// #217: the clock after a seek to `position_ms` at `now` (the song plays
    /// on from there). The hide point follows the song's new position: 3.5 s
    /// before its end, counted from `now`, so a seek into the last 3.5 s puts
    /// it at `now` and the title is no longer due. The show point stays where
    /// the song's start put it: a seek in the song's first 1.5 s still shows
    /// the title then, and a later seek never runs the show again over a title
    /// that is already up.
    pub fn seeked(self, now: Instant, duration_ms: u64, position_ms: u64) -> Self {
        Self {
            hide_at: hide_point(now, duration_ms, position_ms),
            ..self
        }
    }

    /// Whether the song has a title window at all: its hide point, if any,
    /// comes after its show point. A resume 5 s or less before the end has
    /// none: no title, and no timer is armed (`arm_title_timers`).
    pub fn shows(&self) -> bool {
        self.hide_at.is_none_or(|hide_at| hide_at > self.show_at)
    }

    /// Whether the title is due at `now`: from `show_at`, before `hide_at`.
    pub fn open_at(&self, now: Instant) -> bool {
        now >= self.show_at && self.hide_at.is_none_or(|hide_at| now < hide_at)
    }
}

/// When a `duration_ms` song playing from `position_ms` at `at` hides its
/// title: 3.5 s before its end, at `at` when that is already past. `None`
/// for a song of 5 s or less (or an unknown 0 duration): it keeps its title
/// to the end.
fn hide_point(at: Instant, duration_ms: u64, position_ms: u64) -> Option<Instant> {
    if duration_ms > TITLE_SHOW_DELAY_MS + TITLE_HIDE_BEFORE_END_MS {
        let until_hide = (duration_ms - TITLE_HIDE_BEFORE_END_MS).saturating_sub(position_ms);
        Some(at + Duration::from_millis(until_hide))
    } else {
        None
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

/// Set OBS's `#sp-title` text source (`""` clears it), when cg OBS is
/// configured. cg OBS's command queue drains only while it is connected, so
/// the text (the fallback display) is dropped when the queue is full, never
/// awaited: an await parked the engine loop (a resync) or a title timer
/// (review rounds 3 and 4). The caller has already sent Resolume its command.
fn send_obs_title(obs_cmd_tx: Option<&mpsc::Sender<ObsCommand>>, text: String) {
    let Some(cmd_tx) = obs_cmd_tx else {
        return;
    };
    let cmd = ObsCommand::SetTextSource {
        source_name: OBS_TITLE_SOURCE.to_string(),
        text,
    };
    if let Err(e) = cmd_tx.try_send(cmd) {
        tracing::debug!(%e, "OBS title text dropped (cg OBS queue full or closed)");
    }
}

/// The show timer's title: Resolume's ShowTitle, then the OBS text.
///
/// Returns `true` if a title was pushed, `false` if the video had no
/// title info on disk (silent, mirrors prior 1.5 s timer behaviour).
/// The driver acts only on a difference: a title already up is not faded
/// again (#217 addendum 3).
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
    let _ = resolume_tx
        .send(ResolumeCommand::ShowTitle { song, artist })
        .await;
    send_obs_title(obs_cmd_tx, text);
    true
}

/// The hide timer's hide (3.5 s before a song's end): Resolume's
/// HideTitle, then the OBS text cleared.
pub async fn push_hide(
    obs_cmd_tx: Option<&mpsc::Sender<ObsCommand>>,
    resolume_tx: &mpsc::Sender<ResolumeCommand>,
) {
    let _ = resolume_tx.send(ResolumeCommand::HideTitle).await;
    send_obs_title(obs_cmd_tx, String::new());
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
/// the same title, or is cleared as the hide timer clears it (never
/// awaited, `send_obs_title`: this runs on the engine loop).
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
    send_obs_title(obs_cmd_tx, title.unwrap_or_default());
}

#[cfg(test)]
#[path = "title_tests.rs"]
mod tests;
