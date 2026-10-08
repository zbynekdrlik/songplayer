//! #229: ask first. Before a heavy job a node reads its peers' catalogs and
//! fetches what a peer has, waits (≤ 2 h) for what a peer is making, or runs
//! the job itself — announced in its own catalog for as long as the returned
//! guard lives (`peer::decide` holds the rules). With no peers (SNV in
//! phase 1) `ask` answers "here" with no network and no DB write (a hook's
//! own Local path, `run_here`, may delete a wait that cannot exist there).
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
use super::decide::{
    Decision, LocalWhy, MAX_PEER_WAIT, PeerRead, SongFrom, WaitWhy, after_failure, decide,
    listed_audio, recheck_after, song_source,
};
use super::kind::{ArtifactKind, Job};
use super::wire::{Artifact, Catalog, now_ms};
use crate::db::models_peer;

/// What to fetch, from whom: every artifact the job needs, from one peer.
#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub peer: PeerConfig,
    pub artifacts: Vec<Artifact>,
    /// The audio the peer lists for the video (its sha256 and size), read
    /// from the same catalog as the decision (`decide::listed_audio`): what
    /// its stems and lyrics were made from (`Exchange::unless_peers_audio`).
    pub peer_audio: Option<Artifact>,
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
    /// #229 item C: the job may not run here now (`Exchange::may_run_here`:
    /// a lyrics job while this node's paid AI is off) and no peer's copy is
    /// to be taken: hold it (`Exchange::hold`). Nothing is recorded about a
    /// run here: a wait (a spent bound too) and a stand-in stay as they are.
    Held,
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
    /// Ask the peers about `job` for `youtube_id`. A job that may not run
    /// here now (#229 item C) still takes a peer's copy and waits for a
    /// peer's job; where it would run here it is [`Ask::Held`].
    pub async fn ask(&self, job: Job, youtube_id: &str) -> Ask {
        let may_run = self.may_run_here(job).await;
        let cfg = match NodeConfig::load(&self.pool).await {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(
                    youtube_id,
                    job = job.as_str(),
                    error = %e,
                    "exchange: the settings do not hold - processing here"
                );
                return self.unasked(job, youtube_id, may_run);
            }
        };
        if !cfg.asking() {
            return self.unasked(job, youtube_id, may_run);
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
        // The lyrics wait on the peer this node took the song's audio from,
        // unless what they make here already stands in for that peer's copy.
        // Such a job waited its bound once: it waits for no peer again (a
        // run here put back, e.g. for its stems, a later reprocess; review
        // rounds 2 and 4), a peer's copy still comes first.
        let waits_on_song = job.waits_while_a_peer_has_the_song();
        let standing = if waits_on_song {
            self.standin_peer(job, youtube_id).await
        } else {
            None
        };
        let waited = if standing.is_some() {
            Some(MAX_PEER_WAIT)
        } else {
            waited
        };
        let source = if waits_on_song && standing.is_none() {
            self.song_from(youtube_id).await
        } else {
            None
        };
        let song_from = source.as_ref().map(|(peer, sha256)| SongFrom {
            peer: peer.as_str(),
            sha256: sha256.as_str(),
        });
        match decide(job, youtube_id, &reads, waited, song_from) {
            Decision::Fetch { peer, artifacts } => match cfg.peer(&peer) {
                Some(p) => Ask::Fetch(FetchPlan {
                    peer: p.clone(),
                    artifacts,
                    peer_audio: peer_audio(&reads, &peer, youtube_id),
                }),
                None => self.local_or_held(job, youtube_id, may_run).await,
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
                // #229 item C: held before anything about a run here is
                // recorded (the wait, a stand-in, the announcement).
                if !may_run {
                    return Ask::Held;
                }
                // A job that stands in runs here keeping its stand-in and a
                // spent bound its hand-off left (review rounds 5-7): its next
                // pick, meeting the peer's copy, runs here at once.
                if standing.is_some() {
                    return Ask::Local(self.run_here_standing(job, youtube_id).await);
                }
                log_local(youtube_id, job, why);
                let guard = self.run_here(job, youtube_id).await;
                // What runs here stands in for the copy of the peer this node
                // took the song from once it waited the bound for that peer
                // (read now or not).
                if why == LocalWhy::WaitedLongEnough
                    && let Some(peer) = song_source(&reads, song_from)
                {
                    self.stand_in(job, youtube_id, peer).await;
                }
                Ask::Local(guard)
            }
        }
    }

    /// #229 item C: whether `job` may run here now. A job that calls paid AI
    /// (`Job::paid_ai`) only while this node's switch is on
    /// (`paid_ai::enabled`, read live); any other job always.
    pub(crate) async fn may_run_here(&self, job: Job) -> bool {
        match job.paid_ai() {
            Some(_) => crate::paid_ai::enabled(&self.pool).await,
            None => true,
        }
    }

    /// #229 item C: row `video_id` of `job` for `youtube_id` may not run here
    /// now: it is picked again after `paid_ai::HELD_RECHECK`, no attempt
    /// counted (`defer`), and nothing is recorded about a run here; one INFO
    /// per song (`paid_ai::hold`).
    pub(crate) async fn hold(&self, job: Job, video_id: i64, youtube_id: &str) -> PeerStep {
        if let Some(what) = job.paid_ai() {
            crate::paid_ai::hold(what, youtube_id);
        }
        self.defer(job, video_id, crate::paid_ai::HELD_RECHECK)
            .await
    }

    /// `job` of `youtube_id` runs here (`run_here`), or, when it may not run
    /// here now (#229 item C), row `video_id` is held.
    pub(crate) async fn local(&self, job: Job, video_id: i64, youtube_id: &str) -> PeerStep {
        if !self.may_run_here(job).await {
            return self.hold(job, video_id, youtube_id).await;
        }
        PeerStep::Local(Some(self.run_here(job, youtube_id).await))
    }

    /// The job with no peer to ask (no peers, or settings that do not hold):
    /// announced here, or held when it may not run here (`may_run`).
    fn unasked(&self, job: Job, youtube_id: &str, may_run: bool) -> Ask {
        if may_run {
            Ask::Local(self.announce(youtube_id, job))
        } else {
            Ask::Held
        }
    }

    /// `run_here` as an answer, or held when the job may not run here.
    async fn local_or_held(&self, job: Job, youtube_id: &str, may_run: bool) -> Ask {
        if may_run {
            Ask::Local(self.run_here(job, youtube_id).await)
        } else {
            Ask::Held
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
    /// a fresh one, never inheriting this one's spent bound), a stand-in of
    /// it is over (`peer::standin`: the caller records a new one when what
    /// it makes stands in for a peer's copy), and [`Self::start_here`].
    pub(crate) async fn run_here(&self, job: Job, youtube_id: &str) -> JobGuard {
        self.end_wait(job, youtube_id).await;
        self.drop_standin(job, youtube_id).await;
        self.start_here(job, youtube_id).await
    }

    /// `job` of `youtube_id` runs here while what it makes stands in for a
    /// peer's copy (a run put back, a hand-off or a fetch given up): the
    /// stand-in stays as it was (its age and next look), and so does a
    /// spent bound its hand-off left, so the next pick that meets the
    /// peer's copy runs here at once (`peer::standin`, review rounds 5-7).
    pub(crate) async fn run_here_standing(&self, job: Job, youtube_id: &str) -> JobGuard {
        debug!(
            youtube_id,
            job = job.as_str(),
            "exchange: run here again - it still stands in for the peer's copy"
        );
        self.start_here(job, youtube_id).await
    }

    /// The records of a peer's copy of what `job` makes are dropped
    /// (`forget_origins`: the job replaces them), so are the parts a fetch
    /// of it left (`drop_job_parts`), and the job is announced while the
    /// returned guard lives.
    async fn start_here(&self, job: Job, youtube_id: &str) -> JobGuard {
        self.forget_origins(job, youtube_id).await;
        self.drop_job_parts(job, youtube_id).await;
        self.announce(youtube_id, job)
    }

    /// The wait of `job` of `youtube_id` is over (WARNed when it cannot be
    /// ended).
    pub(crate) async fn end_wait(&self, job: Job, youtube_id: &str) {
        if let Err(e) = models_peer::end_wait(&self.pool, youtube_id, job.as_str()).await {
            warn!(youtube_id, %e, "exchange: ending the wait failed");
        }
    }

    /// Row `video_id` of `job` is picked again after `wait`, no attempt
    /// counted, through that job's own recheck column.
    pub(crate) async fn defer(&self, job: Job, video_id: i64, wait: Duration) -> PeerStep {
        let deferred = match job {
            Job::Download => models_peer::defer_download(&self.pool, video_id, wait).await,
            Job::Stems => crate::db::models_stems::defer_stems(&self.pool, video_id, wait).await,
            Job::Lyrics => crate::db::models::record_lyrics_wait(&self.pool, video_id, wait).await,
        };
        if let Err(e) = deferred {
            warn!(video_id, job = job.as_str(), %e, "exchange: deferring the job failed");
        }
        PeerStep::Deferred
    }

    /// A fetch of `job` for row `video_id` from `peer` failed: the row is
    /// deferred while the job has waited less than the bound, else the job
    /// runs here (`fetch_failed`). What a job that waits on the song makes
    /// here then stands in for the peer's copy when this node took the
    /// song's audio from that peer; a stand-in it already had stays as it
    /// was, with its spent wait (`run_here_standing`).
    pub(crate) async fn after_failed_fetch(
        &self,
        job: Job,
        video_id: i64,
        youtube_id: &str,
        peer: &str,
        error: &PeerError,
    ) -> PeerStep {
        // #229 item C: a job that may not run here now waits for the peer's
        // copy with no bound, so no give-up WARN repeats at every pick.
        if !self.may_run_here(job).await {
            debug!(
                youtube_id,
                job = job.as_str(),
                peer,
                %error,
                "exchange: a peer's copy was not taken - held (paid AI off)"
            );
            return self.hold(job, video_id, youtube_id).await;
        }
        match self.fetch_failed(job, youtube_id, peer, error).await {
            Some(recheck) => self.defer(job, video_id, recheck).await,
            None => {
                // Defensive: the lyrics hook hands a standing job's Fetch to
                // the stand-in's look before any fetch (`peer::lyrics`), so
                // no fetch of it fails today (review round 8).
                if self.standin_peer(job, youtube_id).await.is_some() {
                    return PeerStep::Local(Some(self.run_here_standing(job, youtube_id).await));
                }
                let guard = self.run_here(job, youtube_id).await;
                if job.waits_while_a_peer_has_the_song()
                    && self
                        .song_from(youtube_id)
                        .await
                        .is_some_and(|(node, _)| node == peer)
                {
                    self.stand_in(job, youtube_id, peer).await;
                }
                PeerStep::Local(Some(guard))
            }
        }
    }

    /// Each peer's catalog (cached up to `CATALOG_TTL`), `None` where it could
    /// not be read; in `peers` order.
    pub(crate) async fn read_peers(&self, peers: &[PeerConfig]) -> Vec<Option<Arc<Catalog>>> {
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
    /// peer's copy that keeps failing (a refused track, a peer answering
    /// badly) is never retried forever. This node's own pause is waited out
    /// every 5 min and never gives up (`decide::after_failure`).
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
                "exchange: a peer's copy was not taken for 2 h - processing here"
            );
            return None;
        };
        if paused_here {
            // Expected for as long as the pause lasts: no WARN per row and tick.
            debug!(
                youtube_id,
                job = job.as_str(),
                peer,
                recheck_s = recheck.as_secs(),
                "exchange: transfers paused here - asking again later"
            );
        } else {
            warn!(
                youtube_id,
                job = job.as_str(),
                peer,
                %error,
                recheck_s = recheck.as_secs(),
                "exchange: a peer's copy was not taken - asking again later"
            );
        }
        Some(recheck)
    }

    /// `job` is done with what `peer` had: the wait ends, a stand-in of it
    /// is over (`peer::standin`), and each artifact's origin is recorded
    /// (`source = peer:<node>`).
    pub async fn fetched(&self, job: Job, youtube_id: &str, peer: &str, artifacts: &[Artifact]) {
        self.end_wait(job, youtube_id).await;
        self.drop_standin(job, youtube_id).await;
        self.record_origins(youtube_id, peer, artifacts).await;
        info!(
            youtube_id,
            job = job.as_str(),
            source = %format!("peer:{peer}"),
            artifacts = artifacts.len(),
            "exchange: done with a peer's copy"
        );
    }

    /// Each of `artifacts` of `youtube_id` came from `peer` (`peer_fetches`,
    /// the latest record wins).
    pub(crate) async fn record_origins(
        &self,
        youtube_id: &str,
        peer: &str,
        artifacts: &[Artifact],
    ) {
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
    }

    /// What `job` makes for `youtube_id` is this node's own from now on: the
    /// records of a peer's copy of it are dropped, so the old "fetched from
    /// the peer" record never vouches for this node's own audio
    /// (`peer::audio`).
    pub(crate) async fn forget_origins(&self, job: Job, youtube_id: &str) {
        if let Err(e) = models_peer::forget_fetches(&self.pool, youtube_id, job.makes()).await {
            warn!(youtube_id, %e, "exchange: forgetting a peer's copy failed");
        }
    }
}

/// The audio `peer`'s catalog (as read for the decision) lists for
/// `youtube_id`.
fn peer_audio(reads: &[PeerRead<'_>], peer: &str, youtube_id: &str) -> Option<Artifact> {
    reads
        .iter()
        .find(|r| r.peer == peer)
        .and_then(|r| r.catalog)
        .and_then(|c| listed_audio(c, youtube_id))
        .cloned()
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

#[cfg(test)]
#[path = "held_tests.rs"]
mod held_tests;
