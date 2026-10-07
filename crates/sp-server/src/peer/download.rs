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
use tracing::{info, warn};

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::config::PeerConfig;
use super::hash::sha256_hex;
use super::kind::{ArtifactKind, Job, METADATA_PROVIDER};
use super::wire::{PeerMetadata, now_ms};
use crate::db::models_peer;
use crate::downloader::VideoRow;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::metadata::ProviderChain;
use crate::metadata::manual::{
    DownloadTitle, MANUAL_SOURCE, download_title, manual_title, record_download,
};

/// A peer's title for a video, with where it came from: recorded in
/// `peer_fetches` (kind `metadata`, [`record_title`]) once a download or the
/// metadata repair wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTitle {
    pub title: DownloadTitle,
    /// The peer's configured name.
    pub peer: String,
    /// The title's metadata version (`kind::metadata_version`).
    pub version: u32,
    /// The sha256 of the title's canonical bytes (the catalog's).
    pub sha256: String,
}

impl PeerTitle {
    /// `m` from `peer`, when this node takes it ([`adopted_title`]).
    pub fn of(peer: &str, m: &PeerMetadata) -> Option<Self> {
        let title = adopted_title(m)?;
        Some(Self {
            title,
            peer: peer.to_string(),
            version: m.version(),
            sha256: sha256_hex(&m.to_bytes()),
        })
    }
}

/// `youtube_id`'s title came from `t.peer`: recorded once it was written.
pub async fn record_title(ex: &Exchange, youtube_id: &str, t: &PeerTitle) {
    let recorded = models_peer::record_fetch(
        &ex.pool,
        youtube_id,
        ArtifactKind::Metadata.as_str(),
        &t.peer,
        t.version,
        &t.sha256,
        now_ms(),
    )
    .await;
    if let Err(e) = recorded {
        warn!(youtube_id, %e, "exchange: recording a fetch failed");
    }
    info!(
        youtube_id,
        source = %format!("peer:{}", t.peer),
        "exchange: a peer's title was taken"
    );
}

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
            Ok(taken) => {
                ex.fetched(job, &row.youtube_id, &plan.peer.name, &plan.artifacts)
                    .await;
                if let Some(t) = &taken {
                    record_title(ex, &row.youtube_id, t).await;
                }
                PeerStep::Done
            }
            Err(e) => {
                ex.after_failed_fetch(job, row.id, &row.youtube_id, &plan.peer.name, &e)
                    .await
            }
        },
    }
}

/// The peer's pair into this node's cache under this node's title, recorded;
/// the peer's title when it was the one taken. The title is chosen once the
/// pair is here: a failed fetch (retried on every recheck) never calls a
/// provider.
pub(crate) async fn adopt(
    ex: &Exchange,
    chain: &ProviderChain,
    row: &VideoRow,
    plan: &FetchPlan,
) -> Result<Option<PeerTitle>, PeerError> {
    let video_artifact = plan.artifact(ArtifactKind::Video)?;
    let audio_artifact = plan.artifact(ArtifactKind::Audio)?;
    let video_part = ex.fetch(&plan.peer, video_artifact).await?;
    let audio_part = ex.fetch(&plan.peer, audio_artifact).await?;
    let (title, taken) = title_for(ex, chain, row, &plan.peer).await;
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
    if let Err(e) = tokio::fs::rename(&video_part, &video).await {
        // The local download's rule: no unrecorded audio under its final
        // name. The verified video part goes too; the next ask fetches again.
        let _ = tokio::fs::remove_file(&audio).await;
        let _ = tokio::fs::remove_file(&video_part).await;
        return Err(e.into());
    }
    let recorded = record_download(
        &ex.pool,
        &ex.cache_dir,
        row.id,
        &row.youtube_id,
        &title,
        &video,
        &audio,
    )
    .await?;
    // A correction made meanwhile is what was recorded (#136): then no title
    // was taken from the peer.
    Ok(taken.filter(|t| t.title == recorded))
}

/// The title the fetched pair is named after and recorded with, and the
/// peer's title when that is the one.
async fn title_for(
    ex: &Exchange,
    chain: &ProviderChain,
    row: &VideoRow,
    peer: &PeerConfig,
) -> (DownloadTitle, Option<PeerTitle>) {
    if let Ok(Some((song, artist))) = manual_title(&ex.pool, &row.youtube_id).await {
        let correction = DownloadTitle {
            song,
            artist,
            source: MANUAL_SOURCE,
            gemini_failed: false,
        };
        return (correction, None);
    }
    match ex.client.video(peer, &row.youtube_id).await {
        Ok(video) => {
            if let Some(taken) = PeerTitle::of(&peer.name, &video.metadata) {
                return (taken.title.clone(), Some(taken));
            }
        }
        Err(e) => warn!(
            youtube_id = %row.youtube_id,
            %e,
            "exchange: reading the peer's title failed - asking this node's providers"
        ),
    }
    let own = download_title(&ex.pool, chain, &row.youtube_id, &row.title).await;
    (own, None)
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
