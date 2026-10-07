//! #136: a stem / dub job's input, re-read AFTER the job holds the heavy slot.
//!
//! A job is picked, then queues for the one heavy slot behind a running
//! separation, dub or lyrics step, for minutes. The metadata repair can rename
//! the song meanwhile (`downloader::cache::rename_song_files`: the audio and
//! the stems named after it move to the new name). A job that kept the paths
//! it read at pick time then opened a file that no longer existed, and took a
//! penalised failure (`record_stem_deferral` / `record_dub_deferral`).
//!
//! So each worker, holding the slot, calls [`job_input`] and runs the job on
//! what it returns ([`SongInput::stem_job`] / [`SongInput::dub_job`]):
//!
//! - the row is read under [`cache::SONG_FILES`], the lock a rename holds
//!   from its read to its record, so a rename in flight is never seen
//!   half-done;
//! - no audio on disk after that read is a re-pick with NO penalty: no
//!   attempt is counted and the status stays. The row is only rechecked
//!   [`INPUT_MISSING_RECHECK`] later: both selectors would otherwise pick the
//!   same row again first on every tick (the stem queue is in-use-first, then
//!   by id; the dub queue newest request first), ahead of the rest of the
//!   queue. Read under the lock, a missing file is a real loss, not the
//!   rename race, so the WARN names the recorded path and a dub also records
//!   it in `dub_error` (the Dabing tooltip shows it). The next re-read that
//!   finds the audio clears it again.

use std::path::Path;

use sqlx::SqlitePool;

use crate::db::models_dabing::DubJob;
use crate::db::models_stems::StemJob;
use crate::downloader::cache;

/// How long a job whose song has no audio on disk waits before its worker
/// picks it again (no attempt counted, the status untouched).
pub(crate) const INPUT_MISSING_RECHECK: std::time::Duration = std::time::Duration::from_secs(600);

/// The worker a job belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeavyJob {
    Stems,
    Dub,
}

/// A job's input as the song's row records it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SongInput {
    pub audio_file_path: String,
    pub vocals_file_path: Option<String>,
    pub stem_status: Option<String>,
}

impl SongInput {
    /// The picked stem job, on this input.
    pub(crate) fn stem_job(self, job: StemJob) -> StemJob {
        StemJob {
            audio_file_path: self.audio_file_path,
            ..job
        }
    }

    /// The picked dub job, on this input: the audio, and the vocals stem (with
    /// the stems status) that `dub_input_audio` may feed the session instead.
    pub(crate) fn dub_job(self, job: DubJob) -> DubJob {
        DubJob {
            audio_file_path: self.audio_file_path,
            vocals_file_path: self.vocals_file_path,
            stem_status: self.stem_status,
            ..job
        }
    }
}

/// The input of `video_id`'s job, re-read now (the caller holds the heavy
/// slot, or is the node exchange about to fetch the song's stems, #229
/// `peer::stems`). `picked_audio` is the audio the job was picked with, logged next to
/// the current one. `None` = do not run the job now: the song has no audio on
/// disk (the row is rechecked [`INPUT_MISSING_RECHECK`] later, no penalty), or
/// the read failed (logged; the next tick picks it again).
pub(crate) async fn job_input(
    pool: &SqlitePool,
    video_id: i64,
    picked_audio: &str,
    job: HeavyJob,
) -> Option<SongInput> {
    let read = {
        let _files = cache::SONG_FILES.lock().await;
        current_input(pool, video_id).await
    };
    match read {
        Ok(Found::Input(input)) => {
            tracing::info!(
                video_id,
                ?job,
                picked = picked_audio,
                current = %input.audio_file_path,
                "heavy job: input re-read before the job runs (a rename while it waited moves it)"
            );
            if let Err(e) = clear_missing_note(pool, video_id, job).await {
                tracing::warn!(video_id, ?job, %e, "heavy job: clearing the missing-audio note failed");
            }
            Some(input)
        }
        Ok(Found::NoAudio(recorded)) => {
            let recorded = recorded.unwrap_or_default();
            tracing::warn!(
                video_id,
                ?job,
                picked = picked_audio,
                recorded = %recorded,
                recheck_secs = INPUT_MISSING_RECHECK.as_secs(),
                "heavy job: the song has no audio on disk after the re-read — \
                 re-picked later, no attempt counted"
            );
            if let Err(e) = recheck_later(pool, video_id, job, &recorded).await {
                tracing::warn!(video_id, ?job, %e, "heavy job: scheduling the recheck failed");
            }
            None
        }
        Err(e) => {
            tracing::warn!(video_id, ?job, %e, "heavy job: re-reading the input failed");
            None
        }
    }
}

/// What the re-read found.
enum Found {
    Input(SongInput),
    /// No audio on disk; the path the row records, if any.
    NoAudio(Option<String>),
}

/// The row's input, or [`Found::NoAudio`] when the row is gone, records no
/// audio, or its audio file does not exist.
async fn current_input(pool: &SqlitePool, video_id: i64) -> Result<Found, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT audio_file_path, vocals_file_path, stem_status FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let Some((Some(audio_file_path), vocals_file_path, stem_status)) = row else {
        return Ok(Found::NoAudio(None));
    };
    if !Path::new(&audio_file_path).exists() {
        return Ok(Found::NoAudio(Some(audio_file_path)));
    }
    Ok(Found::Input(SongInput {
        audio_file_path,
        vocals_file_path,
        stem_status,
    }))
}

/// The input is there, so a dub's "audio file is missing" note
/// ([`recheck_later`]) no longer holds. It is the only `dub_error` a `synth`
/// row carries (`mark_dub_synth` cleared any other on the way there), and
/// the Dabing tooltip shows it on a chain that is not failed.
async fn clear_missing_note(
    pool: &SqlitePool,
    video_id: i64,
    job: HeavyJob,
) -> Result<(), sqlx::Error> {
    match job {
        HeavyJob::Stems => Ok(()),
        HeavyJob::Dub => {
            sqlx::query("UPDATE videos SET dub_error = NULL WHERE id = ?")
                .bind(video_id)
                .execute(pool)
                .await?;
            Ok(())
        }
    }
}

/// Schedule the job's next pick [`INPUT_MISSING_RECHECK`] ahead, touching no
/// attempt count and no status (same `strftime` format the selectors
/// compare). A dub also records why in `dub_error`, naming `recorded`.
async fn recheck_later(
    pool: &SqlitePool,
    video_id: i64,
    job: HeavyJob,
    recorded: &str,
) -> Result<(), sqlx::Error> {
    let HeavyJob::Dub = job else {
        // The one stem recheck write (also the node exchange's, #229).
        return crate::db::models_stems::defer_stems(pool, video_id, INPUT_MISSING_RECHECK).await;
    };
    let secs = INPUT_MISSING_RECHECK.as_secs() as i64;
    sqlx::query(
        "UPDATE videos SET dub_next_attempt_at = \
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)), \
             dub_error = ? \
         WHERE id = ?",
    )
    .bind(secs)
    .bind(format!(
        "the song's audio file is missing: {recorded} (the dub waits, re-checked later)"
    ))
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "song_input_tests.rs"]
mod tests;
