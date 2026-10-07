//! #229: ask first — whether a heavy job is fetched from a peer, waited for,
//! or run here. Pure: the peers' catalogs and the time already waited in, the
//! decision out (`ask.rs` does the reading and the recording). The first case
//! that applies wins:
//!
//! 1. A peer that holds every artifact the job needs, at a version this node
//!    takes → Fetch (even after the wait bound; a fetch that keeps failing
//!    is bounded by `Exchange::fetch_failed`, which counts as waiting).
//! 2. Waited ≥ [`MAX_PEER_WAIT`] → Local.
//! 3. A peer announcing a job (running or queued) that makes those kinds →
//!    Wait.
//! 4. A peer whose catalog could not be read → Wait (an outage or a refused
//!    key or token is not "nobody has it"; the same bound applies).
//! 5. Else → Local.
//!
//! Only the peers this node lists in its own `peers` setting are read, so a
//! node waits only for those: SNV lists none in phase 1, and asks nobody.
//!
//! A Fetch of stems or lyrics is taken only when this node's audio is the
//! audio the peer lists ([`same_audio`]; the stems and lyrics hooks ask it
//! through `Exchange::has_peers_audio`), else that job runs here.

use std::time::Duration;

use super::kind::{ArtifactKind, Job, acceptable};
use super::wire::{Artifact, Catalog};

/// How long a job waits for its peers before it runs here (spec: "~2 h").
pub const MAX_PEER_WAIT: Duration = Duration::from_secs(7_200);
/// The shortest re-check of a wait…
const MIN_RECHECK: Duration = Duration::from_secs(120);
/// …the longest…
const MAX_RECHECK: Duration = Duration::from_secs(1_200);
/// …and the last one before the bound, at the least.
const LAST_RECHECK: Duration = Duration::from_secs(60);

/// One peer's catalog as read now; `None` = it could not be read.
#[derive(Debug, Clone, Copy)]
pub struct PeerRead<'a> {
    /// The peer's CONFIGURED name (`PeerConfig::name`).
    pub peer: &'a str,
    pub catalog: Option<&'a Catalog>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Fetch `artifacts` (every kind the job needs, in [`Job::needs`] order)
    /// from `peer`.
    Fetch {
        peer: String,
        artifacts: Vec<Artifact>,
    },
    /// Defer the job: `peer` will have it, or could not be asked.
    Wait { peer: String, why: WaitWhy },
    /// Run the job here.
    Local(LocalWhy),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitWhy {
    /// The peer announces the job, running or queued.
    PeerRunsIt,
    /// The peer's catalog could not be read.
    PeerUnreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalWhy {
    NoPeers,
    NobodyHasIt,
    WaitedLongEnough,
}

/// The decision for `job` of `youtube_id` given `reads` (one per listed
/// peer) and how long the job has waited (`None` = not waiting).
pub fn decide(
    job: Job,
    youtube_id: &str,
    reads: &[PeerRead<'_>],
    waited: Option<Duration>,
) -> Decision {
    if reads.is_empty() {
        return Decision::Local(LocalWhy::NoPeers);
    }
    for r in reads {
        if let Some(c) = r.catalog
            && let Some(artifacts) = holds(c, job, youtube_id)
        {
            return Decision::Fetch {
                peer: r.peer.to_string(),
                artifacts,
            };
        }
    }
    if waited.is_some_and(gives_up) {
        return Decision::Local(LocalWhy::WaitedLongEnough);
    }
    let announcing = reads.iter().find(|r| {
        r.catalog
            .is_some_and(|c| c.announces(youtube_id, job.makes()))
    });
    if let Some(r) = announcing {
        return Decision::Wait {
            peer: r.peer.to_string(),
            why: WaitWhy::PeerRunsIt,
        };
    }
    if let Some(r) = reads.iter().find(|r| r.catalog.is_none()) {
        return Decision::Wait {
            peer: r.peer.to_string(),
            why: WaitWhy::PeerUnreadable,
        };
    }
    Decision::Local(LocalWhy::NobodyHasIt)
}

/// Every artifact `job` needs for `youtube_id`, in [`Job::needs`] order, when
/// `catalog` holds them all at a version this node takes.
pub fn holds(catalog: &Catalog, job: Job, youtube_id: &str) -> Option<Vec<Artifact>> {
    job.needs()
        .iter()
        .map(|kind| {
            catalog
                .artifacts
                .iter()
                .find(|a| {
                    a.youtube_id == youtube_id && a.kind == *kind && acceptable(*kind, a.version)
                })
                .cloned()
        })
        .collect()
}

/// The sha256 of the audio `catalog` lists for `youtube_id` (a node lists one
/// per video): the audio that node's stems and lyrics were made from.
pub fn listed_audio<'a>(catalog: &'a Catalog, youtube_id: &str) -> Option<&'a str> {
    catalog
        .artifacts
        .iter()
        .find(|a| a.youtube_id == youtube_id && a.kind == ArtifactKind::Audio)
        .map(|a| a.sha256.as_str())
}

/// This node's audio IS the audio `peer` lists now (`listed`, its sha256),
/// so that peer's stems and lyrics fit it: this node fetched its audio from
/// that peer at that very sha (`fetched`: the `peer_fetches` record's node
/// and sha), or its own hash of its audio is that sha (`hashed`: a
/// `peer_hashes` entry that still holds). Nothing listed there: no. Stems or
/// line timings made from another encode would drift against this node's
/// audio, so the job then runs here.
pub fn same_audio(
    peer: &str,
    listed: Option<&str>,
    fetched: Option<(&str, &str)>,
    hashed: Option<&str>,
) -> bool {
    listed.is_some_and(|sha| fetched == Some((peer, sha)) || hashed == Some(sha))
}

/// A job that has waited `waited` for its peers runs here now: the bound of
/// [`decide`]'s waits, and of a fetch that keeps failing (`Exchange::fetch_failed`).
pub fn gives_up(waited: Duration) -> bool {
    waited >= MAX_PEER_WAIT
}

/// The recheck of a fetch refused by this node's own pause
/// (`peer_transfers_paused`).
pub const PAUSED_RECHECK: Duration = Duration::from_secs(300);

/// After a failed fetch of a job that has waited `waited`: the next recheck,
/// or `None` = run the job here (the bound, [`gives_up`]). This node's own
/// pause (`paused_here`) is waited out, never a reason to run the job here:
/// the pause is about this node's bandwidth, and heavy work in its place
/// would defeat it.
pub fn after_failure(waited: Duration, paused_here: bool) -> Option<Duration> {
    if paused_here {
        return Some(PAUSED_RECHECK);
    }
    (!gives_up(waited)).then(|| recheck_after(waited))
}

/// The next re-check after waiting `waited`: a quarter of it, 2 to 20 min,
/// never past [`MAX_PEER_WAIT`], and at least a minute.
pub fn recheck_after(waited: Duration) -> Duration {
    let next = (waited / 4).clamp(MIN_RECHECK, MAX_RECHECK);
    let left = MAX_PEER_WAIT.saturating_sub(waited);
    next.min(left).max(LAST_RECHECK)
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
