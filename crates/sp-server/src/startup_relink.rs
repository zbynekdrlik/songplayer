//! #136 startup self-heal pass: re-link the files a song names after its audio
//! sidecar (the karaoke stems, the dub track + transcripts,
//! [`cache::derived_files`]) that a rename left under an OLD name.
//!
//! Every consumer finds these files only under the name the song's CURRENT
//! audio derives: the stem mixer, the dub mixer, and the lyrics isolation's
//! vocals input. The metadata repair of 29.9.2026 renamed ~99 songs' video +
//! audio and left their stems behind, so the lyrics queue waited for stems
//! forever and the mixer found none. This pass repairs such drift from any
//! cause. It runs inside [`super::self_heal_cache`], after the complete-pair
//! re-link (the audio paths are current) and the duplicate removal (no
//! superseded download's files are left to adopt).
//!
//! Per row whose audio exists:
//!
//! - **stems** (`stem_status = 'done'`). When the pair is missing under the
//!   audio's name, the newest old name holding BOTH stems is moved over as one
//!   unit; stems from two names are never mixed. When no name holds them, the
//!   row goes back to pending (the `enqueue_stems` reset, recorded paths
//!   cleared) so the stem worker separates the song again.
//! - **dub** (`dub_status = 'ready'`). A missing dub track is moved over, with
//!   its transcripts, from the newest old name holding one. A dub no name holds
//!   is WARNed and counted but never reset, because a dub is an
//!   operator-requested synthesis.
//!
//! The recorded path columns are then rewritten to the audio's names, so the
//! dub worker and the dashboard read the same files the mixers open. A move
//! that fails leaves its row for the next start.

use std::path::{Path, PathBuf};

use sqlx::{Row, SqlitePool};

use crate::downloader::cache;

/// What one [`relink_derived_files`] pass did (logged at INFO).
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

/// Run the pass over every row with stems `'done'` or a dub `'ready'`.
pub(crate) async fn relink_derived_files(
    pool: &SqlitePool,
    cache_dir: &Path,
) -> Result<RelinkCounts, sqlx::Error> {
    let owners = cache::derived_file_owners(cache_dir);
    let rows = sqlx::query(
        "SELECT id, youtube_id, audio_file_path, stem_status, dub_status \
         FROM videos \
         WHERE audio_file_path IS NOT NULL \
           AND (stem_status = 'done' OR dub_status = 'ready')",
    )
    .fetch_all(pool)
    .await?;
    let mut counts = RelinkCounts::default();
    for r in &rows {
        let audio = PathBuf::from(r.get::<String, _>("audio_file_path"));
        if !audio.exists() {
            continue;
        }
        let song = Song {
            id: r.get("id"),
            youtube_id: r.get("youtube_id"),
            audio,
        };
        let names = owners
            .get(&song.youtube_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if r.get::<Option<String>, _>("stem_status").as_deref() == Some("done") {
            relink_stems(pool, &song, names, &mut counts).await?;
        }
        if r.get::<String, _>("dub_status") == "ready" {
            relink_dub(pool, &song, names, &mut counts).await?;
        }
    }
    tracing::info!(
        stems_relinked = counts.stems_relinked,
        stems_reset = counts.stems_reset,
        dubs_relinked = counts.dubs_relinked,
        dubs_missing = counts.dubs_missing,
        "self-heal: re-linked the stems / dub left under an old name"
    );
    Ok(counts)
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
    names: &[PathBuf],
    counts: &mut RelinkCounts,
) -> Result<(), sqlx::Error> {
    let (vocals, instrumental) = crate::stems::stem_paths(&song.audio);
    if !both_stems_exist(&song.audio) {
        let Some(old) = names.iter().find(|name| both_stems_exist(name)) else {
            tracing::warn!(
                youtube_id = %song.youtube_id,
                expected_vocals = %vocals.display(),
                "self-heal: stems are recorded done but no name holds them, \
                 back to pending so the stem worker separates the song again"
            );
            sqlx::query(
                "UPDATE videos \
                 SET stem_status = NULL, stem_attempts = 0, stem_next_attempt_at = NULL, \
                     vocals_file_path = NULL, instrumental_file_path = NULL \
                 WHERE id = ?",
            )
            .bind(song.id)
            .execute(pool)
            .await?;
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
    sqlx::query("UPDATE videos SET vocals_file_path = ?, instrumental_file_path = ? WHERE id = ?")
        .bind(vocals.to_string_lossy().as_ref())
        .bind(instrumental.to_string_lossy().as_ref())
        .bind(song.id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bring the dub track (and its transcripts) under `song.audio`'s name.
async fn relink_dub(
    pool: &SqlitePool,
    song: &Song,
    names: &[PathBuf],
    counts: &mut RelinkCounts,
) -> Result<(), sqlx::Error> {
    let dub = crate::stems::dub_path(&song.audio);
    if !dub.exists() {
        let Some(old) = names
            .iter()
            .find(|name| crate::stems::dub_path(name).exists())
        else {
            tracing::warn!(
                youtube_id = %song.youtube_id,
                expected_dub = %dub.display(),
                "self-heal: the dub is recorded ready but no name holds its track \
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
    sqlx::query("UPDATE videos SET dub_file_path = ? WHERE id = ?")
        .bind(dub.to_string_lossy().as_ref())
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

#[cfg(test)]
#[path = "startup_relink_tests.rs"]
mod tests;
