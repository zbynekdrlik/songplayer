//! #229 (PP audit, comment 6054582866): a lyrics track a node made itself
//! while a listed peer had the song STANDS IN for the peer's copy. A node's
//! own lyrics stay at its pipeline version for good (the lyrics queue never
//! takes such a row again, served or parked), and a node with no AI proxy
//! (PP) makes a degraded track; the peer's is made from the very audio this
//! node fetched. So the peer's copy replaces it once the peer has one.
//!
//! - Recorded (V31 `peer_standins`) when the lyrics job runs here after
//!   asking while a peer had the song: `Exchange::ask` → waited the 2 h
//!   bound with a peer that lists the video's audio, or
//!   `Exchange::after_failed_fetch` giving up on a peer's copy. V31 also
//!   back-filled the ones made before this record existed.
//! - Over when the peer's copy is taken (`Exchange::fetched`) or the job
//!   runs here for another reason (`Exchange::run_here`: an operator's ask,
//!   another audio, nothing newer).
//! - Looked at by the lyrics worker on each tick, after its kill switch
//!   ([`supersede_next`]): one due stand-in, rescheduled FIRST
//!   ([`standin_recheck`] of its age), then taken when a listed peer holds
//!   lyrics at this node's version made from this node's audio
//!   (`Exchange::audio_verdict`): the track replaces `{yt}_lyrics.json` and
//!   every row of the video takes the peer's lyrics columns. Kept for good
//!   (the record dropped) when the peer's copy is made from another audio,
//!   when the video has an operator's text or ask or a dub, or when no row
//!   of it is left. Else it waits for the next recheck.

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use super::Exchange;
use super::ask::FetchPlan;
use super::audio::AudioVerdict;
use super::client::PeerError;
use super::config::NodeConfig;
use super::decide::{holds, listed_audio};
use super::kind::Job;
use super::wire::now_ms;
use crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;
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
    // before its recheck (a peer that keeps failing is never hammered).
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
            ex.drop_standin(job, &youtube_id).await;
            info!(
                youtube_id,
                peer, why, "exchange: a stand-in is kept for good - the peer's copy is not taken"
            );
            Vec::new()
        }
    }
}

/// `?1` = the YouTube id, `?2` = the Live-Translate source.
const OPERATOR_OR_DUB: &str = "SELECT EXISTS (SELECT 1 FROM videos WHERE youtube_id = ?1 \
     AND (lyrics_manual_priority != 0 OR TRIM(COALESCE(lyrics_override_text, '')) != '' \
          OR dub_requested = 1 OR lyrics_source = ?2))";

/// The peer's copy in place of the stand-in of `youtube_id` (the module doc).
async fn supersede(ex: &Exchange, youtube_id: &str) -> Superseded {
    let rows: Vec<i64> =
        match sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ? ORDER BY id")
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
    let Some(&first) = rows.first() else {
        return Superseded::Never("no row of the video is left here");
    };
    let owned: Result<bool, sqlx::Error> = sqlx::query_scalar(OPERATOR_OR_DUB)
        .bind(youtube_id)
        .bind(SOURCE_LIVE_TRANSLATE)
        .fetch_one(&ex.pool)
        .await;
    match owned {
        Ok(false) => {}
        Ok(true) => return Superseded::Never("an operator's lyrics or a dub here"),
        Err(e) => {
            warn!(youtube_id, %e, "exchange: reading a stand-in's video failed");
            return Superseded::NotYet;
        }
    }
    let Some(plan) = peers_lyrics(ex, youtube_id).await else {
        return Superseded::NotYet;
    };
    match ex.audio_verdict(&plan, first, youtube_id).await {
        AudioVerdict::Same => {}
        AudioVerdict::Other => return Superseded::Never("the peer's copy is of another audio"),
        AudioVerdict::NotNow(_) => return Superseded::NotYet,
    }
    let (artifact, lyrics) = match super::lyrics::peer_row(ex, youtube_id, &plan).await {
        Ok(row) => row,
        Err(e) => return not_taken(youtube_id, &plan, &e),
    };
    let placed = super::lyrics::place(ex, youtube_id, &plan, artifact, &lyrics).await;
    if let Err(e) = placed {
        return not_taken(youtube_id, &plan, &e);
    }
    let mut adopted = Vec::with_capacity(rows.len());
    for id in rows {
        match models_peer::adopt_lyrics(&ex.pool, id, &lyrics).await {
            Ok(()) => adopted.push(id),
            Err(e) => warn!(video_id = id, %e, "exchange: a row did not take the peer's lyrics"),
        }
    }
    ex.fetched(Job::Lyrics, youtube_id, &plan.peer.name, &plan.artifacts)
        .await;
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
/// listed peer does (or none is listed, or the settings do not hold).
async fn peers_lyrics(ex: &Exchange, youtube_id: &str) -> Option<FetchPlan> {
    let cfg = NodeConfig::load(&ex.pool).await.ok()?;
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
