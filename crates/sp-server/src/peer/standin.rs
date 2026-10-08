//! #229 (PP audit, comment 6054582866): a lyrics track a node made itself
//! while a listed peer had the song STANDS IN for the peer's copy. A node's
//! own lyrics stay at its pipeline version for good (the lyrics queue never
//! takes such a row again, served or parked), and a node with no AI proxy
//! (PP) makes a degraded track; the peer's is made from the very audio this
//! node fetched. So the peer's copy replaces it once the peer has one.
//!
//! - Recorded (V31 `peer_standins`) when the lyrics job runs here after
//!   asking while the peer it took the song's audio from had the song:
//!   `Exchange::ask` → waited the 2 h bound (on any peer) with that peer
//!   listed (its catalog read now or not), or `Exchange::after_failed_fetch`
//!   giving up on that peer's copy. V31 also back-filled the ones made before this record
//!   existed.
//! - Kept as it was (its age and next look, and a spent wait its hand-off
//!   left: `Exchange::run_here_standing`) when the job runs here again while
//!   it stands in: a run put back, a hand-off or a fetch given up.
//! - Over when the peer's copy is taken (`Exchange::fetched`) or the job
//!   runs here for another reason (`Exchange::run_here`: an operator's ask,
//!   another audio). The lyrics hook never takes a copy into one row of a
//!   video that stands in: meeting a peer's copy it hands it to the look
//!   below (`Exchange::hand_to_standin`: the stand-in due now, the row put
//!   back, counted against the 2 h bound, after which the job runs here,
//!   still standing in, the spent bound kept until the look takes the copy
//!   or the stand-in is kept for good), which takes the copy into every row.
//! - While it stands in, the job's ask waits for no peer (it waited its
//!   bound once): a peer's copy is still taken, a run here keeps it.
//! - Looked at by the lyrics worker on each tick, after its kill switch
//!   ([`supersede_next`]): one due stand-in, rescheduled FIRST
//!   ([`standin_recheck`] of its age), then taken when a listed peer holds
//!   lyrics at this node's version made from this node's audio
//!   (`Exchange::audio_verdict`): the track replaces `{yt}_lyrics.json` and
//!   every row of the video takes the peer's lyrics columns (a row that did
//!   not keeps the stand-in, so the next look does it all again). Kept for
//!   good (the record dropped) when the peer's copy is made from another
//!   audio, when the video has an operator's text or ask or a dub, or when
//!   no row of it is left. Else it waits for the next recheck.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::Exchange;
use super::ask::{FetchPlan, PeerStep};
use super::audio::AudioVerdict;
use super::client::PeerError;
use super::config::NodeConfig;
use super::decide::{gives_up, holds, listed_audio, recheck_after};
use super::kind::{ArtifactKind, Job};
use super::wire::now_ms;
use crate::db::models_peer;

/// The shortest recheck of a stand-in…
pub const STANDIN_MIN_RECHECK: Duration = Duration::from_secs(600);
/// …and the longest: a peer that never makes the lyrics (it parked them)
/// costs four catalog lookups a day.
pub const STANDIN_MAX_RECHECK: Duration = Duration::from_secs(21_600);

/// The next look at a stand-in made `age` ago: a quarter of its age, 10 min
/// to 6 h.
pub fn standin_recheck(age: Duration) -> Duration {
    (age / 4).clamp(STANDIN_MIN_RECHECK, STANDIN_MAX_RECHECK)
}

impl Exchange {
    /// `job` of `youtube_id` runs here while `peer` has the song: what it
    /// makes stands in for the peer's copy (first looked at again after
    /// [`STANDIN_MIN_RECHECK`]).
    pub(crate) async fn stand_in(&self, job: Job, youtube_id: &str, peer: &str) {
        let now = now_ms();
        let next = now + duration_ms(STANDIN_MIN_RECHECK);
        let recorded =
            models_peer::record_standin(&self.pool, youtube_id, job.as_str(), peer, now, next)
                .await;
        if let Err(e) = recorded {
            warn!(youtube_id, %e, "exchange: recording a stand-in failed");
        }
        info!(
            youtube_id,
            job = job.as_str(),
            peer,
            "exchange: made here while a peer has the song - its copy replaces this one once it has it"
        );
    }

    /// The peer this node took `youtube_id`'s audio from and that audio's
    /// sha256 (its `peer_fetches` audio record); `None` for its own
    /// download, a copy, or a record that cannot be read (WARNed).
    pub(crate) async fn song_from(&self, youtube_id: &str) -> Option<(String, String)> {
        models_peer::fetch_record(&self.pool, youtube_id, ArtifactKind::Audio.as_str())
            .await
            .inspect_err(|e| warn!(youtube_id, %e, "exchange: reading the audio's origin failed"))
            .ok()
            .flatten()
            .map(|(node, _, sha256)| (node, sha256))
    }

    /// The peer whose copy what `job` made of `youtube_id` stands in for;
    /// `None` when it stands in for none, or the record cannot be read
    /// (WARNed).
    pub(crate) async fn standin_peer(&self, job: Job, youtube_id: &str) -> Option<String> {
        models_peer::standin_peer(&self.pool, youtube_id, job.as_str())
            .await
            .inspect_err(|e| warn!(youtube_id, %e, "exchange: reading a stand-in failed"))
            .ok()
            .flatten()
    }

    /// The lyrics hook met a peer's copy for `youtube_id`, whose track here
    /// stands in (for `peer`'s): the copy goes into every row of the video through
    /// the stand-in's own look, made due now, and row `video_id` is put back
    /// (`recheck_after`, no attempt). Counted as waiting (`peer_waits`, the
    /// 2 h bound; review round 4): a look that keeps not taking the copy (a
    /// refused track, an audio check that cannot tell) never keeps the row
    /// out for good; once the job has waited the bound it runs here, still
    /// standing in. `fetched` ends the wait when the look takes the copy.
    pub(crate) async fn hand_to_standin(
        &self,
        job: Job,
        video_id: i64,
        youtube_id: &str,
        peer: &str,
    ) -> PeerStep {
        let now = now_ms();
        if let Err(e) = models_peer::start_wait(&self.pool, youtube_id, job.as_str(), now).await {
            warn!(youtube_id, %e, "exchange: recording the wait failed");
        }
        let waited = models_peer::waited(&self.pool, youtube_id, job.as_str(), now)
            .await
            .inspect_err(|e| warn!(youtube_id, %e, "exchange: reading the wait failed"))
            .ok()
            .flatten()
            .unwrap_or_default();
        if gives_up(waited) {
            info!(
                youtube_id,
                job = job.as_str(),
                peer,
                "exchange: a stand-in's copy was not taken for 2 h - processing here, still standing in"
            );
            // The spent bound is kept: a run put back runs here again at
            // once (review round 5); the look's `fetched`, or the stand-in
            // kept for good, ends it.
            return PeerStep::Local(Some(self.run_here_standing(job, youtube_id).await));
        }
        self.standin_due_now(job, youtube_id).await;
        self.defer(job, video_id, recheck_after(waited)).await
    }

    /// The stand-in of `job` of `youtube_id` is looked at on the lyrics
    /// worker's next tick (the hook met it while its peer has a copy).
    pub(crate) async fn standin_due_now(&self, job: Job, youtube_id: &str) {
        let due =
            models_peer::recheck_standin(&self.pool, youtube_id, job.as_str(), now_ms()).await;
        if let Err(e) = due {
            warn!(youtube_id, %e, "exchange: making a stand-in due failed");
        }
    }

    /// `job` of `youtube_id` stands in for no peer's copy any more.
    pub(crate) async fn drop_standin(&self, job: Job, youtube_id: &str) {
        if let Err(e) = models_peer::forget_standin(&self.pool, youtube_id, job.as_str()).await {
            warn!(youtube_id, %e, "exchange: forgetting a stand-in failed");
        }
    }
}

/// `d` in whole milliseconds (saturating).
fn duration_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// What a due stand-in came to.
#[derive(Debug, PartialEq, Eq)]
enum Superseded {
    /// The peer's copy replaced it: the rows that took it.
    Taken(Vec<i64>),
    /// Not now: asked again at its recheck.
    NotYet,
    /// Kept for good (why): the record is dropped.
    Never(&'static str),
}

/// The lyrics worker's tick (after its kill switch): one due lyrics
/// stand-in, the one due the longest (the module doc). Answers the rows that
/// took the peer's lyrics: the worker tells the dashboard
/// (`LyricsCompleted`), as for its own.
pub async fn supersede_next(ex: Option<&Arc<Exchange>>) -> Vec<i64> {
    let Some(ex) = ex else {
        return Vec::new();
    };
    let job = Job::Lyrics;
    let now = now_ms();
    let due = models_peer::due_standin(&ex.pool, job.as_str(), now).await;
    let (youtube_id, peer, made_at_ms) = match due {
        Ok(Some(due)) => due,
        Ok(None) => return Vec::new(),
        Err(e) => {
            warn!(%e, "exchange: reading the stand-ins failed");
            return Vec::new();
        }
    };
    // Rescheduled first: whatever happens below, it is not looked at again
    // before its recheck (a peer that keeps failing is never hammered; the
    // lyrics hook makes it due again at most per its own recheck, within
    // its 2 h bound: `hand_to_standin`).
    let age = Duration::from_millis(u64::try_from(now - made_at_ms).unwrap_or(0));
    let next = now.saturating_add(duration_ms(standin_recheck(age)));
    let rescheduled = models_peer::recheck_standin(&ex.pool, &youtube_id, job.as_str(), next).await;
    if let Err(e) = rescheduled {
        warn!(youtube_id, %e, "exchange: rescheduling a stand-in failed");
        return Vec::new();
    }
    match supersede(ex, &youtube_id).await {
        Superseded::Taken(rows) => rows,
        Superseded::NotYet => Vec::new(),
        Superseded::Never(why) => {
            // A spent wait a hand-off kept goes with it (review round 5).
            ex.end_wait(job, &youtube_id).await;
            ex.drop_standin(job, &youtube_id).await;
            info!(
                youtube_id,
                peer, why, "exchange: a stand-in is kept for good - the peer's copy is not taken"
            );
            Vec::new()
        }
    }
}

/// The peer's copy in place of the stand-in of `youtube_id` (the module doc).
async fn supersede(ex: &Exchange, youtube_id: &str) -> Superseded {
    let rows: Vec<(i64, bool)> = match sqlx::query_as(
        "SELECT id, audio_file_path IS NOT NULL FROM videos WHERE youtube_id = ? ORDER BY id",
    )
    .bind(youtube_id)
    .fetch_all(&ex.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!(youtube_id, %e, "exchange: reading a stand-in's rows failed");
            return Superseded::NotYet;
        }
    };
    // The audio check reads a row that records an audio (the video's rows
    // share it, review round 1), else the lowest.
    let Some(&(first, _)) = rows.iter().find(|(_, audio)| *audio).or(rows.first()) else {
        return Superseded::Never("no row of the video is left here");
    };
    match super::lyrics::kept_local(&ex.pool, youtube_id).await {
        Ok(false) => {}
        Ok(true) => return Superseded::Never("an operator's lyrics or a dub here"),
        Err(e) => {
            warn!(youtube_id, %e, "exchange: reading a stand-in's video failed");
            return Superseded::NotYet;
        }
    }
    let Some(plan) = peers_lyrics(ex, youtube_id).await else {
        debug!(
            youtube_id,
            "exchange: a stand-in waits - no listed peer has its lyrics yet"
        );
        return Superseded::NotYet;
    };
    match ex.audio_verdict(&plan, first, youtube_id).await {
        AudioVerdict::Same => {}
        AudioVerdict::Other => return Superseded::Never("the peer's copy is of another audio"),
        AudioVerdict::NotNow(why) => {
            debug!(youtube_id, %why, "exchange: a stand-in waits - its audio check cannot tell now");
            return Superseded::NotYet;
        }
    }
    let (artifact, lyrics) = match super::lyrics::peer_row(ex, youtube_id, &plan).await {
        Ok(row) => row,
        Err(e) => return not_taken(youtube_id, &plan, &e),
    };
    let placed = super::lyrics::place(ex, youtube_id, &plan, artifact, &lyrics).await;
    if let Err(e) = placed {
        return not_taken(youtube_id, &plan, &e);
    }
    let all = rows.len();
    let mut adopted = Vec::with_capacity(all);
    for (id, _) in rows {
        match models_peer::adopt_lyrics(&ex.pool, id, &lyrics).await {
            Ok(()) => adopted.push(id),
            Err(e) => warn!(video_id = id, %e, "exchange: a row did not take the peer's lyrics"),
        }
    }
    // Over once every row took the copy; else the stand-in stays (already
    // rescheduled) and its next look places the copy into every row again
    // (review round 2).
    if adopted.len() == all {
        ex.fetched(Job::Lyrics, youtube_id, &plan.peer.name, &plan.artifacts)
            .await;
    }
    Superseded::Taken(adopted)
}

/// A peer's copy of a stand-in was not taken now (`e`): asked again at its
/// recheck.
fn not_taken(youtube_id: &str, plan: &FetchPlan, e: &PeerError) -> Superseded {
    warn!(
        youtube_id,
        peer = %plan.peer.name,
        %e,
        "exchange: a peer's copy of a stand-in was not taken - asking again later"
    );
    Superseded::NotYet
}

/// The first listed peer whose catalog holds the lyrics of `youtube_id` at
/// this node's version, as a plan with the audio it lists; `None` when no
/// listed peer does (or none is listed, or the settings do not hold:
/// WARNed).
async fn peers_lyrics(ex: &Exchange, youtube_id: &str) -> Option<FetchPlan> {
    let cfg = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(youtube_id, error = %e, "exchange: a stand-in waits - the settings do not hold");
            return None;
        }
    };
    if !cfg.asking() {
        return None;
    }
    let catalogs = ex.read_peers(&cfg.peers).await;
    cfg.peers.iter().zip(&catalogs).find_map(|(peer, catalog)| {
        let catalog = catalog.as_deref()?;
        let artifacts = holds(catalog, Job::Lyrics, youtube_id)?;
        Some(FetchPlan {
            peer: peer.clone(),
            artifacts,
            peer_audio: listed_audio(catalog, youtube_id).cloned(),
        })
    })
}

#[cfg(test)]
#[path = "standin_tests.rs"]
mod tests;
