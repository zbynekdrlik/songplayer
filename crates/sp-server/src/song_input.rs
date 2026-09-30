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
//!   [`INPUT_MISSING_RECHECK`] later, so a song whose audio is gone for good
//!   is not re-picked every tick ahead of the rest of the queue (both
//!   selectors order by id / request time).

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

impl HeavyJob {
    /// The column its worker's selector waits on.
    fn next_attempt_column(self) -> &'static str {
        match self {
            Self::Stems => "stem_next_attempt_at",
            Self::Dub => "dub_next_attempt_at",
        }
    }
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
/// slot). `picked_audio` is the audio the job was picked with, logged next to
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
        Ok(Some(input)) => {
            tracing::info!(
                video_id,
                ?job,
                picked = picked_audio,
                current = %input.audio_file_path,
                "heavy job: input re-read after the slot (a rename while it waited moves it)"
            );
            Some(input)
        }
        Ok(None) => {
            tracing::warn!(
                video_id,
                ?job,
                picked = picked_audio,
                recheck_secs = INPUT_MISSING_RECHECK.as_secs(),
                "heavy job: the song has no audio on disk after the re-read — \
                 re-picked later, no attempt counted"
            );
            if let Err(e) = recheck_later(pool, video_id, job).await {
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

/// The row's input, or `None` when the row is gone, records no audio, or its
/// audio file does not exist.
async fn current_input(pool: &SqlitePool, video_id: i64) -> Result<Option<SongInput>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT audio_file_path, vocals_file_path, stem_status FROM videos WHERE id = ?",
    )
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let Some((Some(audio_file_path), vocals_file_path, stem_status)) = row else {
        return Ok(None);
    };
    if !Path::new(&audio_file_path).exists() {
        return Ok(None);
    }
    Ok(Some(SongInput {
        audio_file_path,
        vocals_file_path,
        stem_status,
    }))
}

/// Schedule the job's next pick [`INPUT_MISSING_RECHECK`] ahead, touching no
/// attempt count and no status. Same `strftime` format the selectors compare.
async fn recheck_later(pool: &SqlitePool, video_id: i64, job: HeavyJob) -> Result<(), sqlx::Error> {
    let sql = format!(
        "UPDATE videos SET {} = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)) \
         WHERE id = ?",
        job.next_attempt_column()
    );
    sqlx::query(&sql)
        .bind(INPUT_MISSING_RECHECK.as_secs() as i64)
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "song_input_tests.rs"]
mod tests;
