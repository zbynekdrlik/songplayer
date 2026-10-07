//! #136 (ROZHODNUTÉ 5908227964): an operator's title correction belongs to
//! the VIDEO, not to the playlist row, and it is final.
//!
//! One YouTube video is one song with one title (owner rule: one app, one
//! behaviour), yet the same video in two playlists is two `videos` rows (14
//! ids had 2+ rows on the box, 30.9.2026). So a title PATCH of one row
//! (`PATCH /api/v1/videos/{id}`, `api/routes.rs::patch_video`) is spread by
//! [`apply_to_video`] to every row of its `youtube_id` — `song`, `artist`,
//! `metadata_source = 'manual'`, `gemini_failed = 0` — and each row's files
//! are renamed after the correction through `cache::rename_song_files`, the
//! one rename path (`.claude/rules/song-files.md`). A `'manual'` row is out
//! of the metadata repair's queue (`health::REPAIR_QUEUE_WHERE`), so nothing
//! renames it back.
//!
//! A download keeps it too ([`download_title`], item 2): the download worker
//! names a corrected video's files after its corrected title and never asks
//! the provider chain over it — a re-download (a row goes back to
//! `normalized = 0` on the startup 48 kHz reset, `startup.rs`), and the
//! first download of the same video added to another playlist later.

use std::path::Path;

use sqlx::SqlitePool;
use tracing::{info, warn};

use super::ProviderChain;
use crate::downloader::cache::{SongFiles, rename_song_files};

/// The `metadata_source` of an operator's correction.
pub const MANUAL_SOURCE: &str = "manual";

/// The title a download of a row names its files after and records
/// (`DownloadWorker::process_next` → `mark_video_processed_pair`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadTitle {
    pub song: String,
    pub artist: String,
    /// `metadata_source`: [`MANUAL_SOURCE`], or the chain's `MetadataSource`.
    pub source: &'static str,
    pub gemini_failed: bool,
}

/// #136 (ROZHODNUTÉ 5908227964 item 2): the title a download of YouTube video
/// `youtube_id` (YouTube title `title`) names it after. An operator's
/// correction of the video (a row with `metadata_source = 'manual'` and a
/// song; `apply_to_video` gives every row of it the same one) is final: it
/// is kept, and the provider chain is never asked over it. Any other video
/// asks the chain (`get_metadata`: the first provider that answers, else the
/// title parser). A title that cannot be read asks the chain too (WARN).
pub async fn download_title(
    pool: &SqlitePool,
    chain: &ProviderChain,
    youtube_id: &str,
    title: &str,
) -> DownloadTitle {
    match manual_title(pool, youtube_id).await {
        Ok(Some((song, artist))) => {
            info!(
                youtube_id,
                song = %song,
                artist = %artist,
                "metadata: a re-download keeps the operator's title correction"
            );
            return DownloadTitle {
                song,
                artist,
                source: MANUAL_SOURCE,
                gemini_failed: false,
            };
        }
        Ok(None) => {}
        Err(e) => warn!(
            youtube_id,
            %e,
            "metadata: reading the video's title failed — asking the providers"
        ),
    }
    let meta = super::get_metadata(chain.providers(), youtube_id, title).await;
    DownloadTitle {
        song: meta.song,
        artist: meta.artist,
        source: meta.source.as_str(),
        gemini_failed: meta.gemini_failed,
    }
}

/// #136 (review round 1): record a finished download of row `video_db_id` of
/// video `youtube_id` — the `asked` title it was named after and its fresh
/// pair `video` / `audio` in `cache_dir` — through `mark_video_processed_pair`,
/// under `cache::SONG_FILES` (no rename or re-link interleaves). Answers the
/// title it recorded (#229: a peer's title taken is recorded as such only
/// when it is that one).
///
/// `asked` was read (`download_title`) before the download and the loudnorm,
/// about a minute earlier. A correction of the video made in between (a PATCH
/// of this row or of another row of it) is final, so the video's correction
/// is read again under the lock: when there is one, the fresh pair is
/// renamed after it (`rename_song_files`; a no-op when it already carries
/// those names) and the correction is recorded — never the chain's title
/// over `'manual'`.
pub async fn record_download(
    pool: &SqlitePool,
    cache_dir: &Path,
    video_db_id: i64,
    youtube_id: &str,
    asked: &DownloadTitle,
    video: &Path,
    audio: &Path,
) -> Result<DownloadTitle, sqlx::Error> {
    let _files = crate::downloader::cache::SONG_FILES.lock().await;
    let fresh = SongFiles {
        video: Some(video.to_path_buf()),
        audio: Some(audio.to_path_buf()),
    };
    let (title, files) = match manual_title(pool, youtube_id).await {
        Ok(Some((song, artist))) => {
            let named = fresh.named(cache_dir, &song, &artist, youtube_id, false);
            log_corrected_download(youtube_id, &fresh, &named);
            let files = rename_song_files(youtube_id, &fresh, &named);
            let corrected = DownloadTitle {
                song,
                artist,
                source: MANUAL_SOURCE,
                gemini_failed: false,
            };
            (corrected, files)
        }
        Ok(None) => (asked.clone(), fresh),
        Err(e) => {
            warn!(
                video_db_id,
                youtube_id,
                %e,
                "metadata: reading the video's correction failed — the download keeps its title"
            );
            (asked.clone(), fresh)
        }
    };
    let columns = files.columns();
    crate::db::models::mark_video_processed_pair(
        pool,
        video_db_id,
        &title.song,
        &title.artist,
        title.source,
        title.gemini_failed,
        &columns.video,
        columns.audio.as_deref().unwrap_or_default(),
    )
    .await?;
    Ok(asked.clone())
}

/// The INFO of a download whose fresh pair is renamed after a correction
/// made while it ran (`record_download`). Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_corrected_download(youtube_id: &str, fresh: &SongFiles, named: &SongFiles) {
    if named != fresh {
        info!(
            youtube_id,
            from = ?fresh.audio,
            to = ?named.audio,
            "metadata: the video was corrected during its download — its files are named after the correction"
        );
    }
}

/// Why a title PATCH of row `video_db_id` is refused (its 400), or `None`
/// (`api/routes.rs::patch_video`): `song` is the sanitized song it sets
/// (`""` = whitespace only), `sets_artist` whether it sets the artist.
///
/// - A blank song has nothing to show on the wall.
/// - Review round 2: an artist ALONE for a video no row of which has a song
///   (none downloaded yet). A correction is final, but a download keeps one
///   only with its song ([`download_title`], [`record_download`]) and names
///   such a video after the provider chain, over the artist. The dashboard
///   sends the song with the artist; only the API sends an artist alone.
///
/// A row that does not exist is no refusal: the PATCH answers 404.
pub async fn refused_title(
    pool: &SqlitePool,
    video_db_id: i64,
    song: Option<&str>,
    sets_artist: bool,
) -> Result<Option<&'static str>, sqlx::Error> {
    match song {
        Some("") => return Ok(Some("song must not be whitespace-only")),
        Some(_) => return Ok(None),
        None if !sets_artist => return Ok(None),
        None => {}
    }
    let songless: Option<bool> = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM videos w WHERE w.youtube_id = v.youtube_id \
                AND TRIM(COALESCE(w.song, '')) != '') \
         FROM videos v WHERE v.id = ?",
    )
    .bind(video_db_id)
    .fetch_optional(pool)
    .await?;
    Ok(songless
        .unwrap_or(false)
        .then_some("the video has no song yet: send the song with the artist"))
}

/// Video `youtube_id`'s `(song, artist)` when a row of it is an operator's
/// correction with a song (`mark_video_processed_pair` refuses an empty one;
/// the lowest row id when several are); `artist` `""` when it has none. Also
/// read by the node exchange (#229, `peer::download`): a peer's pair is named
/// after it too.
pub(crate) async fn manual_title(
    pool: &SqlitePool,
    youtube_id: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT song, COALESCE(artist, '') FROM videos \
         WHERE youtube_id = ? AND metadata_source = ? AND TRIM(COALESCE(song, '')) != '' \
         ORDER BY id LIMIT 1",
    )
    .bind(youtube_id)
    .bind(MANUAL_SOURCE)
    .fetch_optional(pool)
    .await
}

/// Spread row `video_db_id`'s title (the correction its PATCH just wrote) to
/// every row of its YouTube video, and rename each row's files after it in
/// `cache_dir` (no `_gf`: a correction is no parser title). Each row's set is
/// read right before its move, never from a snapshot (an earlier row of the
/// loop may have moved the same files), and recorded on every row that
/// recorded it (`SongColumns::record`). A row with no files yet (not
/// downloaded) takes the title and records none. The caller holds
/// `cache::SONG_FILES` from its own UPDATE to here, so a repair or a
/// re-link never interleaves. A row that no longer exists is nothing to do.
///
/// Review round 1: a patched row with NO song (not downloaded yet, an
/// artist-only PATCH) spreads no empty song — every row keeps its own song,
/// and each row's files are named after the row's title as it now is.
pub async fn apply_to_video(
    pool: &SqlitePool,
    cache_dir: &Path,
    video_db_id: i64,
) -> Result<(), sqlx::Error> {
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT youtube_id, COALESCE(song, ''), artist FROM videos WHERE id = ?")
            .bind(video_db_id)
            .fetch_optional(pool)
            .await?;
    let Some((youtube_id, song, artist)) = row else {
        return Ok(());
    };
    let spread = sqlx::query(
        "UPDATE videos SET song = COALESCE(NULLIF(?, ''), song), artist = ?, \
         metadata_source = ?, gemini_failed = 0 \
         WHERE youtube_id = ?",
    )
    .bind(&song)
    .bind(&artist)
    .bind(MANUAL_SOURCE)
    .bind(&youtube_id)
    .execute(pool)
    .await?;
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ? ORDER BY id")
            .bind(&youtube_id)
            .fetch_all(pool)
            .await?;
    for id in ids {
        let (file_path, audio_file_path, row_song, row_artist): RowFiles = sqlx::query_as(
            "SELECT COALESCE(file_path, ''), audio_file_path, COALESCE(song, ''), \
                    COALESCE(artist, '') FROM videos WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        let old = SongFiles::recorded(&file_path, audio_file_path.as_deref());
        if old.is_empty() {
            continue; // not downloaded yet: no files to name
        }
        let new = old.named(cache_dir, &row_song, &row_artist, &youtube_id, false);
        let files = rename_song_files(&youtube_id, &old, &new).columns();
        files
            .record(pool, &youtube_id, &file_path, audio_file_path.as_deref())
            .await?;
    }
    info!(
        video_db_id,
        youtube_id = %youtube_id,
        song = %song,
        artist = ?artist,
        rows = spread.rows_affected(),
        "metadata: the operator's title correction applies to every row of the video"
    );
    Ok(())
}

/// A row's recorded files and title as `apply_to_video` reads them:
/// `file_path`, `audio_file_path`, `song`, `artist` (`""` for a NULL text).
type RowFiles = (String, Option<String>, String, String);

#[cfg(test)]
#[path = "manual_tests.rs"]
mod tests;
