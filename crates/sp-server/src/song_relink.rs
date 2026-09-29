//! #136: re-link the files a song names after its audio sidecar (the karaoke
//! stems, the dub track + transcripts, [`cache::derived_files`]) that were left
//! under an OLD name, so they sit where the song's CURRENT audio derives them.
//!
//! Every consumer finds these files only under that name: the stem mixer, the
//! dub mixer, and the lyrics isolation's vocals input. The metadata repair of
//! 29.9.2026 renamed ~99 songs' video + audio and left their stems behind, so
//! the lyrics queue waited for stems forever and the mixer found none. Two
//! entry points repair such drift from any cause:
//!
//! - [`relink_derived_files`]: every song, from
//!   [`crate::startup::self_heal_cache`], after the complete-pair re-link (the
//!   audio paths are current) and the duplicate removal.
//! - [`relink_song`]: one song, run by the stem worker and the dub worker once
//!   they recorded a finished job. A job writes under the name its song had
//!   when the job STARTED, so a rename while it ran would leave those files
//!   behind.
//!
//! Per row whose audio exists:
//!
//! - **stems** (`stem_status = 'done'`). When the pair is missing under the
//!   audio's name, the old name with the newest pair holding BOTH stems is
//!   moved over as one unit; stems from two names are never mixed. When no name
//!   holds them, the row goes back to pending (the `enqueue_stems` reset,
//!   recorded paths cleared) so the stem worker separates the song again.
//! - **dub** (`dub_status = 'ready'`). A missing dub track is moved over, with
//!   its transcripts, from the old name with the newest dub. A dub no name holds
//!   is WARNed and counted but never reset, because a dub is an
//!   operator-requested synthesis.
//!
//! The recorded path columns are then set to the audio's names (only when they
//! differ), so the dub worker and the dashboard read the same files the mixers
//! open. A move that fails leaves its row for the next pass. A pass holds
//! [`cache::SONG_FILES`] from reading the rows to the last record, so it never
//! interleaves with a rename.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::downloader::cache;

/// What one pass did (logged at INFO).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelinkCounts {
    /// Songs whose stems moved from an old name to their audio's name.
    pub stems_relinked: usize,
    /// `'done'` songs with no stems under any name, reset to pending.
    pub stems_reset: usize,
    /// Songs whose dub track moved from an old name to their audio's name.
    pub dubs_relinked: usize,
    /// `'ready'` dubs with no track under any name (left as they are).
    pub dubs_missing: usize,
}

/// The rows a pass considers: stems `'done'` or a dub `'ready'`.
const ROWS: &str = "SELECT id, youtube_id, audio_file_path, stem_status, dub_status \
     FROM videos \
     WHERE audio_file_path IS NOT NULL \
       AND (stem_status = 'done' OR dub_status = 'ready')";

/// Re-link every song's stems / dub (the startup self-heal pass).
pub(crate) async fn relink_derived_files(
    pool: &SqlitePool,
    cache_dir: &Path,
) -> Result<RelinkCounts, sqlx::Error> {
    // The rows are read under the lock too, so the pass acts on them as a
    // rename left them, never on a read taken before it.
    let _files = cache::SONG_FILES.lock().await;
    let rows = sqlx::query(ROWS).fetch_all(pool).await?;
    let counts = relink_rows(pool, cache_dir, &rows).await?;
    tracing::info!(
        stems_relinked = counts.stems_relinked,
        stems_reset = counts.stems_reset,
        dubs_relinked = counts.dubs_relinked,
        dubs_missing = counts.dubs_missing,
        "self-heal: re-linked the stems / dub left under an old name"
    );
    Ok(counts)
}

/// Re-link ONE song's stems / dub, after a worker recorded a finished job for
/// it (`video_id`). `written_for` is the audio the job's output is named after:
/// the song's audio when the job STARTED. If the song was renamed meanwhile, the
/// rename already carried the song's older files to the new name, so the job's
/// own output is the fresh copy. It moves over them first, as one unit, and then
/// the normal pass runs (review round 4: a re-dub stranded its new dub while the
/// old one stayed in place). It moves every file named after `written_for`: a
/// completed rename leaves nothing under the old name, and a stem job and a dub
/// job never run at once (one heavy slot), so those files are this job's. Not
/// when the current audio is missing (the pass skips such a row too). If the
/// move fails, it rolls back and the output stays under the start name. When an
/// older copy is under the current name, that copy stays in effect and nothing
/// retries the move. When there is none, the pass right after, and the next
/// start, re-link the output like any drifted file.
pub(crate) async fn relink_song(
    pool: &SqlitePool,
    cache_dir: &Path,
    video_id: i64,
    written_for: &Path,
) -> Result<RelinkCounts, sqlx::Error> {
    let _files = cache::SONG_FILES.lock().await;
    let song: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT youtube_id, audio_file_path FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_optional(pool)
            .await?;
    if let Some((youtube_id, Some(current))) = song
        && Path::new(&current) != written_for
        && Path::new(&current).exists()
    {
        let moves: Vec<(PathBuf, PathBuf)> = cache::derived_files(written_for)
            .into_iter()
            .zip(cache::derived_files(Path::new(&current)))
            .collect();
        if cache::move_as_unit(&youtube_id, &moves).is_err() {
            tracing::warn!(
                youtube_id = %youtube_id,
                written_for = %written_for.display(),
                current = %current,
                "re-link: a job's output could not follow the song's rename and stays under \
                 the start name; an older copy under the current name stays in effect and is \
                 not replaced later, with none the pass below and the next start re-link it"
            );
        }
    }
    let rows = sqlx::query(&format!("{ROWS} AND id = ?"))
        .bind(video_id)
        .fetch_all(pool)
        .await?;
    relink_rows(pool, cache_dir, &rows).await
}

/// The pass over `rows`; the caller holds [`cache::SONG_FILES`] since it read them.
async fn relink_rows(
    pool: &SqlitePool,
    cache_dir: &Path,
    rows: &[SqliteRow],
) -> Result<RelinkCounts, sqlx::Error> {
    let mut owners = Owners {
        cache_dir,
        scanned: None,
    };
    let mut counts = RelinkCounts::default();
    for r in rows {
        let audio = PathBuf::from(r.get::<String, _>("audio_file_path"));
        if !audio.exists() {
            continue;
        }
        let song = Song {
            id: r.get("id"),
            youtube_id: r.get("youtube_id"),
            audio,
        };
        if r.get::<Option<String>, _>("stem_status").as_deref() == Some("done") {
            relink_stems(pool, &song, &mut owners, &mut counts).await?;
        }
        if r.get::<String, _>("dub_status") == "ready" {
            relink_dub(pool, &song, &mut owners, &mut counts).await?;
        }
    }
    Ok(counts)
}

/// The names in the cache holding derived files ([`cache::derived_file_owners`]).
/// The cache is scanned once per pass, and only when a unit is missing: after a
/// job whose output is already in place, nothing is scanned.
struct Owners<'a> {
    cache_dir: &'a Path,
    scanned: Option<HashMap<String, Vec<PathBuf>>>,
}

impl Owners<'_> {
    fn of(&mut self, youtube_id: &str) -> &[PathBuf] {
        self.scanned
            .get_or_insert_with(|| cache::derived_file_owners(self.cache_dir))
            .get(youtube_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// One row the pass works on: its audio exists.
struct Song {
    id: i64,
    youtube_id: String,
    audio: PathBuf,
}

/// Bring the stems pair under `song.audio`'s name (see the module doc).
async fn relink_stems(
    pool: &SqlitePool,
    song: &Song,
    owners: &mut Owners<'_>,
    counts: &mut RelinkCounts,
) -> Result<(), sqlx::Error> {
    let (vocals, instrumental) = crate::stems::stem_paths(&song.audio);
    if !both_stems_exist(&song.audio) {
        let names = owners.of(&song.youtube_id);
        let holders = names.iter().filter(|name| both_stems_exist(name));
        let Some(old) = newest(holders, |name| crate::stems::stem_paths(name).0) else {
            tracing::warn!(
                youtube_id = %song.youtube_id,
                expected_vocals = %vocals.display(),
                "re-link: stems are recorded done but no name holds them, \
                 back to pending so the stem worker separates the song again"
            );
            crate::db::models_stems::requeue_lost_stems(pool, song.id).await?;
            counts.stems_reset += 1;
            return Ok(());
        };
        let (old_vocals, old_instrumental) = crate::stems::stem_paths(old);
        let moves = [
            (old_vocals, vocals.clone()),
            (old_instrumental, instrumental.clone()),
        ];
        if cache::move_as_unit(&song.youtube_id, &moves).is_err() {
            return Ok(());
        }
        counts.stems_relinked += 1;
    }
    let (vocals, instrumental) = (vocals.to_string_lossy(), instrumental.to_string_lossy());
    sqlx::query(
        "UPDATE videos SET vocals_file_path = ?1, instrumental_file_path = ?2 \
         WHERE id = ?3 AND (vocals_file_path IS NOT ?1 OR instrumental_file_path IS NOT ?2)",
    )
    .bind(vocals.as_ref())
    .bind(instrumental.as_ref())
    .bind(song.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Bring the dub track (and its transcripts) under `song.audio`'s name.
async fn relink_dub(
    pool: &SqlitePool,
    song: &Song,
    owners: &mut Owners<'_>,
    counts: &mut RelinkCounts,
) -> Result<(), sqlx::Error> {
    let dub = crate::stems::dub_path(&song.audio);
    if !dub.exists() {
        let names = owners.of(&song.youtube_id);
        let holders = names
            .iter()
            .filter(|name| crate::stems::dub_path(name).exists());
        let Some(old) = newest(holders, crate::stems::dub_path) else {
            tracing::warn!(
                youtube_id = %song.youtube_id,
                expected_dub = %dub.display(),
                "re-link: the dub is recorded ready but no name holds its track \
                 (left as it is: a dub is only synthesized on request)"
            );
            counts.dubs_missing += 1;
            return Ok(());
        };
        let moves = [
            (crate::stems::dub_path(old), dub.clone()),
            (
                crate::stems::dub_transcripts_path(old),
                crate::stems::dub_transcripts_path(&song.audio),
            ),
        ];
        if cache::move_as_unit(&song.youtube_id, &moves).is_err() {
            return Ok(());
        }
        counts.dubs_relinked += 1;
    }
    let dub = dub.to_string_lossy();
    sqlx::query("UPDATE videos SET dub_file_path = ?1 WHERE id = ?2 AND dub_file_path IS NOT ?1")
        .bind(dub.as_ref())
        .bind(song.id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether both stems named after `audio` exist.
fn both_stems_exist(audio: &Path) -> bool {
    let (vocals, instrumental) = crate::stems::stem_paths(audio);
    vocals.exists() && instrumental.exists()
}

/// The name among `names` whose `file` (the unit's own file, derived from the
/// name) was written last.
fn newest<'a>(
    names: impl Iterator<Item = &'a PathBuf>,
    file: impl Fn(&Path) -> PathBuf,
) -> Option<&'a PathBuf> {
    names.max_by_key(|name| written_at(&file(name.as_path())))
}

/// A file's modification time (the epoch when unreadable, so it loses).
fn written_at(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
#[path = "song_relink_tests.rs"]
mod tests;
