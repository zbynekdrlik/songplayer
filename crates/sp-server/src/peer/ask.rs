//! #229: ask first. Before a heavy job a node reads its peers' catalogs and
//! fetches what a peer has, waits (≤ 2 h) for what a peer is making, or runs
//! the job itself — announced in its own catalog for as long as the returned
//! guard lives (`peer::decide` holds the rules). With no peers (SNV in phase
//! 1) `ask` answers "here" with no network and no DB write (a hook's own
//! Local path, `run_here`, may delete a wait that cannot exist there).
//!
//! The waits are durable (V30 `peer_waits`, the FIRST start kept until the
//! wait ends), the origin of a fetched artifact goes to `peer_fetches`
//! (`source = peer:<node>` in the log); the row's own `metadata_source` /
//! `lyrics_source` keep the peer's real values (they drive the repair queue,
//! `alignment_model_for_source` and the ★ wall marker).

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::Exchange;
use super::board::JobGuard;
use super::client::PeerError;
use super::config::{NodeConfig, PeerConfig};
use super::decide::{Decision, LocalWhy, PeerRead, WaitWhy, after_failure, decide, recheck_after};
use super::kind::{ArtifactKind, Job};
use super::wire::{Artifact, Catalog, now_ms};
use crate::db::models_peer;

/// What to fetch, from whom: every artifact the job needs, from one peer.
#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub peer: PeerConfig,
    pub artifacts: Vec<Artifact>,
}

impl FetchPlan {
    /// The plan's artifact of `kind`.
    pub fn artifact(&self, kind: ArtifactKind) -> Result<&Artifact, PeerError> {
        self.artifacts
            .iter()
            .find(|a| a.kind == kind)
            .ok_or_else(|| PeerError::BadResponse(format!("the plan has no {}", kind.as_str())))
    }
}

/// The answer to "ask first".
#[must_use = "a Local answer's guard announces the job; a Wait is to be recorded on the row"]
pub enum Ask {
    /// Fetch the plan's artifacts instead of running the job.
    Fetch(FetchPlan),
    /// A peer will have it, or could not be asked: ask again after `recheck`.
    Wait { peer: String, recheck: Duration },
    /// Run the job here, announced while the guard lives.
    Local(JobGuard),
}

/// What a worker's hook (`peer::{download, stems, lyrics}::first`) tells its
/// worker to do next.
#[must_use = "Local carries the job's announcement"]
pub enum PeerStep {
    /// The artifacts are in place and recorded: the job is done.
    Done,
    /// The row is picked again later, no attempt counted.
    Deferred,
    /// Run the job here, announced while the guard lives (`None`: no
    /// exchange wired, e.g. a unit-test worker).
    Local(Option<JobGuard>),
}

impl Exchange {
    /// Ask the peers about `job` for `youtube_id`.
    pub async fn ask(&self, job: Job, youtube_id: &str) -> Ask {
        let cfg = match NodeConfig::load(&self.pool).await {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(
                    youtube_id,
                    job = job.as_str(),
                    error = %e,
                    "exchange: the settings do not hold - processing here"
                );
                return Ask::Local(self.announce(youtube_id, job));
            }
        };
        if !cfg.asking() {
            return Ask::Local(self.announce(youtube_id, job));
        }
        let catalogs = self.read_peers(&cfg.peers).await;
        let reads: Vec<PeerRead<'_>> = cfg
            .peers
            .iter()
            .zip(&catalogs)
            .map(|(peer, c)| PeerRead {
                peer: &peer.name,
                catalog: c.as_deref(),
            })
            .collect();
        let now = now_ms();
        let waited = models_peer::waited(&self.pool, youtube_id, job.as_str(), now)
            .await
            .inspect_err(|e| warn!(youtube_id, %e, "exchange: reading the wait failed"))
            .ok()
            .flatten();
        match decide(job, youtube_id, &reads, waited) {
            Decision::Fetch { peer, artifacts } => match cfg.peer(&peer) {
                Some(p) => Ask::Fetch(FetchPlan {
                    peer: p.clone(),
                    artifacts,
                }),
                None => Ask::Local(self.run_here(job, youtube_id).await),
            },
            Decision::Wait { peer, why } => {
                if let Err(e) =
                    models_peer::start_wait(&self.pool, youtube_id, job.as_str(), now).await
                {
                    warn!(youtube_id, %e, "exchange: recording the wait failed");
                }
                let recheck = recheck_after(waited.unwrap_or_default());
                log_wait(youtube_id, job, &peer, why, waited, recheck);
                Ask::Wait { peer, recheck }
            }
            Decision::Local(why) => {
                log_local(youtube_id, job, why);
                Ask::Local(self.run_here(job, youtube_id).await)
            }
        }
    }

    /// This node asks its peers before a job (a node name and at least one
    /// peer, `NodeConfig::asking`); settings that do not hold read as no.
    pub(crate) async fn asks_peers(&self) -> bool {
        NodeConfig::load(&self.pool)
            .await
            .is_ok_and(|cfg| cfg.asking())
    }

    /// `job` of `youtube_id` runs here: a wait of it ends (a later ask starts
    /// a fresh one, never inheriting this one's spent bound), and the job is
    /// announced while the returned guard lives.
    pub(crate) async fn run_here(&self, job: Job, youtube_id: &str) -> JobGuard {
        if let Err(e) = models_peer::end_wait(&self.pool, youtube_id, job.as_str()).await {
            warn!(youtube_id, %e, "exchange: ending the wait failed");
        }
        self.announce(youtube_id, job)
    }

    /// Row `video_id` of `job` is picked again after `wait`, no attempt
    /// counted, through that job's own recheck column.
    pub(crate) async fn defer(&self, job: Job, video_id: i64, wait: Duration) -> PeerStep {
        let deferred = match job {
            Job::Download => models_peer::defer_download(&self.pool, video_id, wait).await,
            Job::Stems => models_peer::defer_stems(&self.pool, video_id, wait).await,
            Job::Lyrics => crate::db::models::record_lyrics_wait(&self.pool, video_id, wait).await,
        };
        if let Err(e) = deferred {
            warn!(video_id, job = job.as_str(), %e, "exchange: deferring the job failed");
        }
        PeerStep::Deferred
    }

    /// A fetch of `job` for row `video_id` from `peer` failed: the row is
    /// deferred while the job has waited less than the bound, else the job
    /// runs here (`fetch_failed`).
    pub(crate) async fn after_failed_fetch(
        &self,
        job: Job,
        video_id: i64,
        youtube_id: &str,
        peer: &str,
        error: &PeerError,
    ) -> PeerStep {
        match self.fetch_failed(job, youtube_id, peer, error).await {
            Some(recheck) => self.defer(job, video_id, recheck).await,
            None => PeerStep::Local(Some(self.run_here(job, youtube_id).await)),
        }
    }

    /// Each peer's catalog (cached up to `CATALOG_TTL`), `None` where it could
    /// not be read; in `peers` order.
    async fn read_peers(&self, peers: &[PeerConfig]) -> Vec<Option<Arc<Catalog>>> {
        let mut catalogs = Vec::with_capacity(peers.len());
        for peer in peers {
            let read = self.client.catalog(peer).await;
            if let Err(e) = &read {
                debug!(peer = %peer.name, error = %e, "exchange: a peer's catalog could not be read");
            }
            catalogs.push(read.ok());
        }
        catalogs
    }

    /// A fetch from `peer` did not work: the job waits (counted against the
    /// 2 h bound) and asks again after the returned recheck; `None` once it
    /// has waited the bound (`decide::gives_up`): it runs here then, so a
    /// peer's copy that keeps failing (no audio here, a refused track, this
    /// node paused) is never retried forever.
    pub async fn fetch_failed(
        &self,
        job: Job,
        youtube_id: &str,
        peer: &str,
        error: &PeerError,
    ) -> Option<Duration> {
        let now = now_ms();
        if let Err(e) = models_peer::start_wait(&self.pool, youtube_id, job.as_str(), now).await {
            warn!(youtube_id, %e, "exchange: recording the wait failed");
        }
        let waited = models_peer::waited(&self.pool, youtube_id, job.as_str(), now)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let paused_here = self.transfers_paused().await;
        let Some(recheck) = after_failure(waited, paused_here) else {
            warn!(
                youtube_id,
                job = job.as_str(),
                peer,
                %error,
                "exchange: fetching from a peer kept failing for 2 h - processing here"
            );
            return None;
        };
        warn!(
            youtube_id,
            job = job.as_str(),
            peer,
            %error,
            paused_here,
            recheck_s = recheck.as_secs(),
            "exchange: fetching from a peer failed - asking again later"
        );
        Some(recheck)
    }

    /// `job` is done with what `peer` had: the wait ends and each artifact's
    /// origin is recorded (`source = peer:<node>`).
    pub async fn fetched(&self, job: Job, youtube_id: &str, peer: &str, artifacts: &[Artifact]) {
        if let Err(e) = models_peer::end_wait(&self.pool, youtube_id, job.as_str()).await {
            warn!(youtube_id, %e, "exchange: ending the wait failed");
        }
        let at = now_ms();
        for a in artifacts {
            let recorded = models_peer::record_fetch(
                &self.pool,
                youtube_id,
                a.kind.as_str(),
                peer,
                a.version,
                &a.sha256,
                at,
            )
            .await;
            if let Err(e) = recorded {
                warn!(youtube_id, %e, "exchange: recording a fetch failed");
            }
        }
        info!(
            youtube_id,
            job = job.as_str(),
            source = %format!("peer:{peer}"),
            artifacts = artifacts.len(),
            "exchange: done with a peer's copy"
        );
    }
}

/// The INFO of a deferred job. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_wait(
    youtube_id: &str,
    job: Job,
    peer: &str,
    why: WaitWhy,
    waited: Option<Duration>,
    recheck: Duration,
) {
    info!(
        youtube_id,
        job = job.as_str(),
        peer,
        ?why,
        waited_s = waited.map_or(0, |w| w.as_secs()),
        recheck_s = recheck.as_secs(),
        "exchange: a peer will have it, or could not be asked - waiting"
    );
}

/// The log line of a job that runs here: INFO when it waited the full bound,
/// else DEBUG. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_local(youtube_id: &str, job: Job, why: LocalWhy) {
    if why == LocalWhy::WaitedLongEnough {
        info!(
            youtube_id,
            job = job.as_str(),
            "exchange: waited 2 h for the peers - processing here"
        );
    } else {
        debug!(
            youtube_id,
            job = job.as_str(),
            ?why,
            "exchange: no peer has it - processing here"
        );
    }
}

#[cfg(test)]
#[path = "ask_tests.rs"]
mod tests;
