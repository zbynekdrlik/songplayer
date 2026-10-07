//! #229: the stems job asks first. A peer's two stems are fetched as parts,
//! then placed under THIS node's audio name as the row records it AFTER the
//! transfer — read under `cache::SONG_FILES`, the lock a rename holds (#136,
//! `.claude/rules/song-files.md`) — and marked done. The stem worker asks
//! before its venv check (the plan's decisions): a node with no lyrics venv
//! still takes a peer's stems.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::kind::{ArtifactKind, Job};
use crate::db::models_peer;
use crate::db::models_stems::{StemJob, mark_stems_done};
use crate::downloader::cache::SONG_FILES;

/// The stem worker's hook: fetch, wait or separate here.
pub async fn first(ex: Option<&Arc<Exchange>>, job: &StemJob) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    match ex.ask(Job::Stems, &job.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => {
            defer(ex, job.video_id, recheck).await;
            PeerStep::Deferred
        }
        Ask::Fetch(plan) => match adopt(ex, job, &plan).await {
            Ok(()) => {
                ex.fetched(
                    Job::Stems,
                    &job.youtube_id,
                    &plan.peer.name,
                    &plan.artifacts,
                )
                .await;
                PeerStep::Done
            }
            Err(e) => {
                let recheck = ex
                    .fetch_failed(Job::Stems, &job.youtube_id, &plan.peer.name, &e)
                    .await
                    .unwrap_or_default();
                defer(ex, job.video_id, recheck).await;
                PeerStep::Deferred
            }
        },
    }
}

async fn defer(ex: &Exchange, video_id: i64, wait: Duration) {
    if let Err(e) = models_peer::defer_stems(&ex.pool, video_id, wait).await {
        warn!(video_id, %e, "exchange: deferring the stems failed");
    }
}

/// The peer's stems under the row's CURRENT audio name, done.
pub(crate) async fn adopt(ex: &Exchange, job: &StemJob, plan: &FetchPlan) -> Result<(), PeerError> {
    let vocals_part = ex
        .fetch(&plan.peer, plan.artifact(ArtifactKind::StemVocals)?)
        .await?;
    let instrumental_part = ex
        .fetch(&plan.peer, plan.artifact(ArtifactKind::StemInstrumental)?)
        .await?;
    let _files = SONG_FILES.lock().await;
    let audio: Option<Option<String>> =
        sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
            .bind(job.video_id)
            .fetch_optional(&ex.pool)
            .await?;
    let audio = audio
        .flatten()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .ok_or_else(|| PeerError::Io("the song has no audio on disk here".into()))?;
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    tokio::fs::rename(&vocals_part, &vocals).await?;
    tokio::fs::rename(&instrumental_part, &instrumental).await?;
    mark_stems_done(
        &ex.pool,
        job.video_id,
        &vocals.to_string_lossy(),
        &instrumental.to_string_lossy(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "stems_tests.rs"]
mod tests;
