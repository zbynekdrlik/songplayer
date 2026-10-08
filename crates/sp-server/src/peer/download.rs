//! #229: the download job (download + normalize + metadata) asks first. A
//! peer's video + audio pair is fetched instead of yt-dlp + loudnorm and named
//! after THIS node's title: an operator's correction here, else the peer's
//! provider or operator title, else the title parser's, marked for the
//! repair (item A: the peer holds this very song and names it too, so no
//! provider — paid AI — is asked here; the repair takes the peer's title
//! once it has one, `peer::repair::waits_for_peer`). It is recorded through
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
use crate::metadata::manual::{DownloadTitle, MANUAL_SOURCE, manual_title, record_download};

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
    /// The sha256 of `PeerMetadata::to_bytes`, as the catalog computes it.
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

/// The download worker's hook: fetch, wait or run here. A download that runs
/// here forgets the pair's peer origin before it starts (`forget_origins`):
/// it writes this node's own audio under the names a fetched pair had. That
/// holds on the no-peers and bad-settings paths too, which never go through
/// `run_here`.
pub(crate) async fn first(ex: Option<&Arc<Exchange>>, row: &VideoRow) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    let step = ask_or_fetch(ex, row).await;
    if let PeerStep::Local(_) = &step {
        ex.forget_origins(Job::Download, &row.youtube_id).await;
    }
    step
}

/// Ask the peers about the row's download and act on the answer.
async fn ask_or_fetch(ex: &Exchange, row: &VideoRow) -> PeerStep {
    let job = Job::Download;
    match ex.ask(job, &row.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Held => ex.hold(job, row.id, &row.youtube_id).await,
        Ask::Wait { recheck, .. } => ex.defer(job, row.id, recheck).await,
        Ask::Fetch(plan) => match adopt(ex, row, &plan).await {
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
    row: &VideoRow,
    plan: &FetchPlan,
) -> Result<Option<PeerTitle>, PeerError> {
    let video_artifact = plan.artifact(ArtifactKind::Video)?;
    let audio_artifact = plan.artifact(ArtifactKind::Audio)?;
    let video_part = ex.fetch(&plan.peer, video_artifact).await?;
    let audio_part = ex.fetch(&plan.peer, audio_artifact).await?;
    let (title, taken) = title_for(ex, row, &plan.peer).await;
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
    // Rows of one video share files by name (#136): a final name may hold
    // another row's recorded file. The video goes first: when it cannot take
    // its name (that row's video open in a player) nothing is touched and both
    // verified parts stay for the next ask (re-hashed there, no transfer).
    let video_was_there = tokio::fs::try_exists(&video).await.unwrap_or(true);
    tokio::fs::rename(&video_part, &video).await?;
    if let Err(e) = tokio::fs::rename(&audio_part, &audio).await {
        // No unrecorded video under its final name: one this attempt placed
        // goes back into its part. One that was there (another row's) stays,
        // now the peer's copy of the same video; the next ask fetches again.
        if !video_was_there && tokio::fs::rename(&video, &video_part).await.is_err() {
            let _ = tokio::fs::remove_file(&video).await;
        }
        return Err(e.into());
    }
    // The pair under these names is the peer's from here on. Recorded before
    // `record_download` makes the row playable: a stems or lyrics ask in
    // between would otherwise find no record and process the song here
    // (`peer::audio`). `fetched` records the same rows again once done.
    ex.record_origins(&row.youtube_id, &plan.peer.name, &plan.artifacts)
        .await;
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
    Ok(written_title(taken, &recorded))
}

/// The peer's title when it is the one `record_download` wrote: a correction
/// made meanwhile is what gets written (#136), and then none was taken.
fn written_title(taken: Option<PeerTitle>, recorded: &DownloadTitle) -> Option<PeerTitle> {
    taken.filter(|t| t.title == *recorded)
}

/// The title the fetched pair is named after and recorded with, and the
/// peer's title when that is the one.
async fn title_for(
    ex: &Exchange,
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
            "exchange: reading the peer's title failed - the title parser names it for the repair"
        ),
    }
    // Item A: the peer holds this very song and names it too (its own
    // repair): no provider here, the repair takes the peer's title later.
    let own = DownloadTitle::from(crate::metadata::parser_for_repair(&row.title));
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
