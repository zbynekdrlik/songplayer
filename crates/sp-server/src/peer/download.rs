//! #229: the download job (download + normalize + metadata) asks first. A
//! peer's video + audio pair is fetched instead of yt-dlp + loudnorm and named
//! after THIS node's title: an operator's correction here, else the peer's
//! provider or operator title, else this node's own providers
//! (`download_title`). It is recorded through
//! `metadata::manual::record_download`, the local download's own record path,
//! which re-reads a correction made meanwhile under `cache::SONG_FILES` (#136,
//! `.claude/rules/song-files.md`). `downloader/` is out of the mutation gate,
//! so the logic lives here and only the hook sits in `DownloadWorker`.

use std::sync::Arc;

use sp_core::metadata::MetadataSource;
use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::config::PeerConfig;
use super::kind::{ArtifactKind, Job, METADATA_PROVIDER};
use super::wire::PeerMetadata;
use crate::downloader::VideoRow;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::metadata::ProviderChain;
use crate::metadata::manual::{
    DownloadTitle, MANUAL_SOURCE, download_title, manual_title, record_download,
};

/// The download worker's hook: fetch, wait or run here.
pub(crate) async fn first(
    ex: Option<&Arc<Exchange>>,
    chain: &ProviderChain,
    row: &VideoRow,
) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    let job = Job::Download;
    match ex.ask(job, &row.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => ex.defer(job, row.id, recheck).await,
        Ask::Fetch(plan) => match adopt(ex, chain, row, &plan).await {
            Ok(()) => {
                ex.fetched(job, &row.youtube_id, &plan.peer.name, &plan.artifacts)
                    .await;
                PeerStep::Done
            }
            Err(e) => {
                ex.after_failed_fetch(job, row.id, &row.youtube_id, &plan.peer.name, &e)
                    .await
            }
        },
    }
}

/// The peer's pair into this node's cache under this node's title, recorded.
/// The title is chosen once the pair is here: a failed fetch (retried on
/// every recheck) never calls a provider.
pub(crate) async fn adopt(
    ex: &Exchange,
    chain: &ProviderChain,
    row: &VideoRow,
    plan: &FetchPlan,
) -> Result<(), PeerError> {
    let video_artifact = plan.artifact(ArtifactKind::Video)?;
    let audio_artifact = plan.artifact(ArtifactKind::Audio)?;
    let video_part = ex.fetch(&plan.peer, video_artifact).await?;
    let audio_part = ex.fetch(&plan.peer, audio_artifact).await?;
    let title = title_for(ex, chain, row, &plan.peer).await;
    let gf = title.gemini_failed;
    let video = ex.cache_dir.join(video_filename(
        &title.song,
        &title.artist,
        &row.youtube_id,
        gf,
    ));
    let audio = ex.cache_dir.join(audio_filename(
        &title.song,
        &title.artist,
        &row.youtube_id,
        gf,
    ));
    tokio::fs::rename(&audio_part, &audio).await?;
    tokio::fs::rename(&video_part, &video).await?;
    record_download(
        &ex.pool,
        &ex.cache_dir,
        row.id,
        &row.youtube_id,
        &title,
        &video,
        &audio,
    )
    .await?;
    Ok(())
}

/// The title the fetched pair is named after and recorded with.
async fn title_for(
    ex: &Exchange,
    chain: &ProviderChain,
    row: &VideoRow,
    peer: &PeerConfig,
) -> DownloadTitle {
    if let Ok(Some((song, artist))) = manual_title(&ex.pool, &row.youtube_id).await {
        return DownloadTitle {
            song,
            artist,
            source: MANUAL_SOURCE,
            gemini_failed: false,
        };
    }
    match ex.client.video(peer, &row.youtube_id).await {
        Ok(video) => {
            if let Some(title) = adopted_title(&video.metadata) {
                return title;
            }
        }
        Err(e) => warn!(
            youtube_id = %row.youtube_id,
            %e,
            "exchange: reading the peer's title failed - asking this node's providers"
        ),
    }
    download_title(&ex.pool, chain, &row.youtube_id, &row.title).await
}

/// A peer's title this node takes as its own: a provider's answer or an
/// operator's correction (metadata version ≥ 1, `kind::metadata_version`)
/// with a song, under the `metadata_source` this node writes for it. `None` =
/// ask this node's providers (a parser's guess there is no better than one
/// made here).
pub fn adopted_title(m: &PeerMetadata) -> Option<DownloadTitle> {
    if m.version() < METADATA_PROVIDER || m.song.trim().is_empty() {
        return None;
    }
    let label = m.metadata_source.as_deref()?;
    let source = [MANUAL_SOURCE, MetadataSource::Gemini.as_str()]
        .into_iter()
        .find(|s| *s == label)?;
    Some(DownloadTitle {
        song: m.song.clone(),
        artist: m.artist.clone(),
        source,
        gemini_failed: false,
    })
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
