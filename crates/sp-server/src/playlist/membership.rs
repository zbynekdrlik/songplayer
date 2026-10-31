//! #260: what one playlist sync's fetch means for the playlist's rows.
//!
//! The sync used to only upsert what yt-dlp listed, so a song taken out of
//! the YouTube playlist stayed a row and kept being picked (owner,
//! 13.10.2026: ytfast had 114 rows on SNV and PP, its YouTube playlist 108).
//! Now every row the fetch does not list is MARKED (V40 `removed_at`, never
//! deleted: its files, stems, lyrics and dub stay) and a marked row the
//! fetch lists again is brought back. What the mark does elsewhere is in
//! `db::models_membership`; the year after it, `playlist::retention`.

use std::collections::HashSet;

use sqlx::SqlitePool;
use tracing::warn;

use super::PlaylistEntry;
use crate::db::models_membership::{self, Member};

/// What one fetch changes: the rows whose song left the YouTube playlist
/// (marked now) and the marked rows whose song is back in it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diff {
    pub marked: Vec<Member>,
    pub back: Vec<Member>,
}

/// The marks after a fetch that listed `fetched`, for the playlist's
/// `rows`: a row not marked and not listed is marked, a marked row that is
/// listed is back; a marked row still not listed keeps its first mark (it is
/// in neither list). Only ever asked with a fetch that listed something
/// ([`record_fetch`]).
pub fn diff(rows: &[Member], fetched: &[&str]) -> Diff {
    let listed: HashSet<&str> = fetched.iter().copied().collect();
    let mut out = Diff::default();
    for row in rows {
        let in_list = listed.contains(row.youtube_id.as_str());
        if !row.removed && !in_list {
            out.marked.push(row.clone());
        } else if row.removed && in_list {
            out.back.push(row.clone());
        }
    }
    out
}

/// What one sync did, for its INFO line (`lib.rs`, "playlist sync
/// complete").
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Rows inserted for songs new in the playlist.
    pub new: usize,
    /// Rows marked: their song left the YouTube playlist.
    pub marked: Vec<Member>,
    /// Marked rows whose song is back in the YouTube playlist.
    pub back: Vec<Member>,
}

/// `members` as the INFO line names them: `<youtube id> <title>` joined
/// with `; ` (a row with no title yet by its id alone), `-` for none.
pub fn names(members: &[Member]) -> String {
    if members.is_empty() {
        return "-".to_string();
    }
    members
        .iter()
        .map(|m| match &m.title {
            Some(title) => format!("{} {title}", m.youtube_id),
            None => m.youtube_id.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Record one fetch of `playlist_id`'s YouTube playlist at `now_ms`: every
/// fetched entry upserted (`upsert_entry`, which also clears the mark of a
/// song that is back) and every row the fetch no longer lists marked.
///
/// A fetch that listed nothing changes nothing (one WARN): yt-dlp can
/// answer success with no entry on a glitch, and marking the whole playlist
/// would leave it with nothing to play. The writes hold
/// `cache::SONG_FILES`, as the retention does from its last read of a row
/// to its deletion, so a song brought back is never deleted at that moment.
///
/// #261: a fetch of an INACTIVE playlist (a manual sync; the periodic and
/// the startup syncs take active ones only) records nothing (one WARN): its
/// upsert would clear the marks its deactivation set, and those songs would
/// never go (`db::models_membership::deactivate`).
pub async fn record_fetch(
    pool: &SqlitePool,
    playlist_id: i64,
    entries: &[PlaylistEntry],
    now_ms: i64,
) -> Result<SyncReport, sqlx::Error> {
    if entries.is_empty() {
        warn!(
            playlist_id,
            "playlist sync: yt-dlp listed no video - nothing is marked as removed"
        );
        return Ok(SyncReport::default());
    }
    let _files = crate::downloader::cache::SONG_FILES.lock().await;
    if !models_membership::playlist_active(pool, playlist_id).await? {
        warn!(
            playlist_id,
            "playlist sync: the playlist is inactive - nothing is recorded"
        );
        return Ok(SyncReport::default());
    }
    let rows = models_membership::members(pool, playlist_id).await?;
    let fetched: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let changes = diff(&rows, &fetched);
    let mut new = 0;
    for entry in entries {
        if super::upsert_entry(pool, playlist_id, entry).await? {
            new += 1;
        }
    }
    for row in &changes.marked {
        models_membership::mark_removed(pool, playlist_id, &row.youtube_id, now_ms).await?;
    }
    Ok(SyncReport {
        new,
        marked: changes.marked,
        back: changes.back,
    })
}

#[cfg(test)]
#[path = "membership_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "membership_tests_play.rs"]
mod tests_play;

#[cfg(test)]
#[path = "membership_tests_queues.rs"]
mod tests_queues;

#[cfg(test)]
#[path = "membership_tests_inactive.rs"]
mod tests_inactive;
