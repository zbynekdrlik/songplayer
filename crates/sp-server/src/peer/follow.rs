//! #264: a node follows its peers' lyrics. The owner's rule (13.10.2026,
//! at PP): a transcript or a translation SNV improves must reach PP — "aby
//! sa nestavalo ze v snv budeme pracne zlepsovat prepisy a preklady no nic
//! z toho sa do pp nedostane".
//!
//! The lyrics hook (`peer::lyrics`) asks a peer only for a row the lyrics
//! queue picks. A copy taken once leaves the queue for good (served at the
//! current version), and a parked row (`asr_gap`, `unsupported_source`, a
//! `no_source` …) is in no bucket, so nothing asked the peer again. The
//! lyrics worker's tick therefore looks at the peers' catalogs itself
//! ([`follow_next`], right after the stand-in's look, inside the same tick:
//! never beside the hook or a local run), only at a node that asks peers
//! (SNV lists none: nothing there), never while this node's transfers are
//! paused (the operator's pause, or #230's hold).
//!
//! - A video is followed ([`candidates`]) when this node's lyrics worker
//!   serves it (`queue_sql::LYRICS_ELIGIBLE`), no row of it is in the
//!   lyrics queue (buckets 1–3 stay the hook's), it stands in for no
//!   peer's copy (the stand-in's look owns those), and the first listed
//!   peer that holds its lyrics at this node's version lists a copy this
//!   node does not hold ([`wants`]): no lyrics here, a copy taken from a
//!   peer (`peer_fetches`) that is not the one listed now, or, only while
//!   this node's paid AI is off (it then makes no lyrics itself), a track
//!   no peer record names (a phase-0 copy, an old attempt here).
//! - ONE song per look: an operator's lyrics or a dub (`lyrics::kept_local`;
//!   a reprocess flag only while this node's paid AI is on, #264) or a copy
//!   of another audio (`Exchange::audio_verdict`) is passed over;
//!   else the copy goes into every row of the video
//!   (`lyrics::take_into_rows`: the old track is served until the
//!   verified copy is renamed over it), and only once every row took it
//!   is its origin recorded. A row that refuses its columns after the
//!   rename serves the new file under its old columns until the retry.
//!   A song passed over, whose take failed, or just taken (a record that
//!   failed is not fetched again on every tick) is looked at again after
//!   [`FOLLOW_KEPT`] / [`FOLLOW_RETRY`], or at once when the peer lists
//!   another copy.
//! - The next look is the next tick while a look finds work, else
//!   [`FOLLOW_IDLE`] later. `GET /api/v1/exchange/status` shows what was
//!   followed since this process started ([`FollowStatus`]).
//!
//! No paid AI: the follow only fetches. The stems need no follow (they
//! change only with `STEMS_VERSION`, which re-queues them here); the titles
//! follow in their own step, with this cadence and memory
//! (`peer::follow_title`, #269).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use super::Exchange;
use super::ask::FetchPlan;
use super::audio::AudioVerdict;
use super::client::{PeerError, lock};
use super::config::NodeConfig;
use super::decide::{holds, listed_audio};
use super::kind::{ArtifactKind, Job};
use super::lyrics::{audio_row, kept_local, take_into_rows, video_rows};
use super::queued::lyrics_rows;
use super::standin::duration_ms;
use super::wire::{Artifact, Catalog, ms_to_rfc3339, now_ms};
use crate::lyrics::queue_sql::{LYRICS_ELIGIBLE, queued_later_where};

/// The next look once a look found nothing to take: a copy a peer makes
/// again reaches this node within it (after the peer's own hash pass).
pub const FOLLOW_IDLE: Duration = Duration::from_secs(900);
/// A song whose take failed, or whose audio check cannot tell yet.
pub const FOLLOW_RETRY: Duration = Duration::from_secs(1_800);
/// A song kept here: an operator's lyrics or a dub, another audio.
pub const FOLLOW_KEPT: Duration = Duration::from_secs(21_600);

/// Why a video's lyrics follow a peer's copy, in the order a look takes
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowWhy {
    /// No row of the video serves lyrics here (a parked sentinel, or none):
    /// the wall shows nothing, so these come first.
    NoLyrics,
    /// The copy this node took from a peer (`peer_fetches`) is not the one
    /// the peer lists now: the peer made the lyrics again.
    Changed,
    /// A track no peer record names (a phase-0 copy, an old attempt here)
    /// while this node makes no lyrics itself (paid AI off).
    Untracked,
}

/// What this node holds of one video's lyrics ([`held_videos`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub youtube_id: String,
    /// A row of the video serves lyrics here (its rows share one track).
    pub has_lyrics: bool,
    /// The copy this node took from a peer: the `peer_fetches` lyrics
    /// record's node and sha256.
    pub took: Option<(String, String)>,
}

/// A video a look follows: `peer`'s copy `artifact`, and why (`W`: the
/// lyrics' [`FollowWhy`], or #269's title `follow_title::TitleWhy`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate<W = FollowWhy> {
    pub youtube_id: String,
    pub peer: String,
    pub why: W,
    pub artifact: Artifact,
}

/// Why `held`'s lyrics follow `listed`, the lyrics `peer` lists now; `None`
/// when this node holds that very copy, or a track of its own: one no peer
/// record names while this node makes lyrics itself (`untracked` false:
/// paid AI on), such as an operator's reprocess result or the full-mix
/// upgrade's ★ retry, which a peer's copy must not undo.
pub fn wants(held: &Held, peer: &str, listed: &Artifact, untracked: bool) -> Option<FollowWhy> {
    if !held.has_lyrics {
        return Some(FollowWhy::NoLyrics);
    }
    match &held.took {
        Some((node, sha256)) if node == peer && *sha256 == listed.sha256 => None,
        Some(_) => Some(FollowWhy::Changed),
        None => untracked.then_some(FollowWhy::Untracked),
    }
}

/// The videos of `held` whose lyrics follow a peer's copy, in the order a
/// look takes them ([`FollowWhy`]'s order, then the YouTube id). Per video
/// the first of `reads` (the listed peers' catalogs in `peers` order, `None`
/// where one could not be read) that holds its lyrics at this node's version
/// (`decide::holds`), and only when [`wants`] says so.
pub fn candidates(
    held: &[Held],
    reads: &[(&str, Option<&Catalog>)],
    untracked: bool,
) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = held
        .iter()
        .filter_map(|h| {
            let (peer, artifact) = reads.iter().find_map(|&(peer, catalog)| {
                let artifacts = holds(catalog?, Job::Lyrics, &h.youtube_id)?;
                Some((peer, artifacts.into_iter().next()?))
            })?;
            let why = wants(h, peer, &artifact, untracked)?;
            Some(Candidate {
                youtube_id: h.youtube_id.clone(),
                peer: peer.to_string(),
                why,
                artifact,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        a.why
            .cmp(&b.why)
            .then_with(|| a.youtube_id.cmp(&b.youtube_id))
    });
    found
}

/// `follow` (the lyrics) and `title_follow` (#269, the titles) in `GET
/// /api/v1/exchange/status`, since this process started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowStatus<W = FollowWhy> {
    /// Songs that followed a peer's copy.
    pub followed: u64,
    /// The last of them.
    pub last: Option<Followed<W>>,
    /// When the last look ran (RFC 3339, UTC); `None` before one.
    pub last_look: Option<String>,
    /// The songs the last look found wanting a peer's copy (one taken per
    /// lyrics tick)…
    pub wanted: usize,
    /// …and those it passed over for now (an operator's lyrics, a dub,
    /// another audio, a failed take).
    pub passed_over: usize,
    /// The last take that failed.
    pub last_error: Option<FollowError>,
}

impl<W> Default for FollowStatus<W> {
    fn default() -> Self {
        Self {
            followed: 0,
            last: None,
            last_look: None,
            wanted: 0,
            passed_over: 0,
            last_error: None,
        }
    }
}

/// A song that followed a peer's copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Followed<W = FollowWhy> {
    pub youtube_id: String,
    pub peer: String,
    pub why: W,
    /// RFC 3339, UTC.
    pub at: String,
}

/// A take that failed (asked again after [`FOLLOW_RETRY`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowError {
    pub youtube_id: String,
    pub peer: String,
    pub error: String,
    /// RFC 3339, UTC.
    pub at: String,
}

/// A follow's memory, in this process only: a restart looks at every song
/// again. The lyrics' (`W` = [`FollowWhy`]) and #269's titles' each have
/// their own (`Exchange::follow`, `Exchange::title_follow`).
#[derive(Debug)]
pub struct FollowState<W = FollowWhy> {
    /// No look before this instant (UTC ms).
    pub(super) next_look_ms: i64,
    /// The songs passed over for now: the copy passed over, and until when.
    pub(super) passed: HashMap<String, Passed>,
    pub(super) status: FollowStatus<W>,
}

impl<W> Default for FollowState<W> {
    fn default() -> Self {
        Self {
            next_look_ms: 0,
            passed: HashMap::new(),
            status: FollowStatus::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Passed {
    pub(super) sha256: String,
    pub(super) until_ms: i64,
}

impl<W: Copy> FollowState<W> {
    /// A look may run at `now_ms`.
    pub(super) fn due(&self, now_ms: i64) -> bool {
        now_ms >= self.next_look_ms
    }

    /// No look before `after` from `now_ms`.
    pub(super) fn look_after(&mut self, now_ms: i64, after: Duration) {
        self.next_look_ms = now_ms.saturating_add(duration_ms(after));
    }

    /// A look at `now_ms` found `found` ([`candidates`]): the first song not
    /// passed over is taken now; with none the next look is [`FOLLOW_IDLE`]
    /// later. What the look found goes to the status.
    pub(super) fn pick(&mut self, found: &[Candidate<W>], now_ms: i64) -> Option<Candidate<W>> {
        self.passed.retain(|_, p| now_ms < p.until_ms);
        let left: Vec<&Candidate<W>> = found.iter().filter(|c| !self.passes_over(c)).collect();
        self.status.last_look = Some(ms_to_rfc3339(now_ms));
        self.status.wanted = left.len();
        self.status.passed_over = found.len() - left.len();
        let first = left.first().copied().cloned();
        if first.is_none() {
            self.look_after(now_ms, FOLLOW_IDLE);
        }
        first
    }

    /// `c` is passed over for now: while its peer lists the same copy, for
    /// `retry` from `now_ms`.
    pub(super) fn pass(&mut self, c: &Candidate<W>, now_ms: i64, retry: Duration) {
        let passed = Passed {
            sha256: c.artifact.sha256.clone(),
            until_ms: now_ms.saturating_add(duration_ms(retry)),
        };
        self.passed.insert(c.youtube_id.clone(), passed);
    }

    /// `c` is passed over: the same copy is passed over still.
    fn passes_over(&self, c: &Candidate<W>) -> bool {
        self.passed
            .get(&c.youtube_id)
            .is_some_and(|p| p.sha256 == c.artifact.sha256)
    }

    /// `c` followed the peer's copy at `now_ms`.
    pub(super) fn followed(&mut self, c: &Candidate<W>, now_ms: i64) {
        self.status.followed += 1;
        self.status.last = Some(Followed {
            youtube_id: c.youtube_id.clone(),
            peer: c.peer.clone(),
            why: c.why,
            at: ms_to_rfc3339(now_ms),
        });
    }

    /// `c`'s take failed at `now_ms` (`error`): asked again after
    /// [`FOLLOW_RETRY`].
    pub(super) fn failed(&mut self, c: &Candidate<W>, now_ms: i64, error: &PeerError) {
        self.pass(c, now_ms, FOLLOW_RETRY);
        self.status.last_error = Some(FollowError {
            youtube_id: c.youtube_id.clone(),
            peer: c.peer.clone(),
            error: error.to_string(),
            at: ms_to_rfc3339(now_ms),
        });
    }
}

impl Exchange {
    /// `follow` for `GET /api/v1/exchange/status`.
    pub fn follow_status(&self) -> FollowStatus {
        lock(&self.follow).status.clone()
    }
}

/// The lyrics worker's tick, right after the stand-in's look (the module
/// doc): at most ONE video's lyrics follow a peer's copy. Answers the rows
/// that took it: the worker tells the dashboard (`LyricsCompleted`), as for
/// its own lyrics.
pub async fn follow_next(ex: Option<&Arc<Exchange>>) -> Vec<i64> {
    let Some(ex) = ex else {
        return Vec::new();
    };
    let now = now_ms();
    let Some(cfg) = look_config(ex, &ex.follow, now).await else {
        return Vec::new();
    };
    let held = match held_videos(&ex.pool).await {
        Ok(held) => held,
        Err(e) => {
            warn!(%e, "exchange: the lyrics follow could not read this node's rows");
            lock(&ex.follow).look_after(now, FOLLOW_IDLE);
            return Vec::new();
        }
    };
    let catalogs = ex.read_peers(&cfg.peers).await;
    let reads = peer_reads(&cfg, &catalogs);
    let untracked = !ex.may_run_here(Job::Lyrics).await;
    let found = candidates(&held, &reads, untracked);
    let picked = lock(&ex.follow).pick(&found, now);
    let Some(c) = picked else {
        return Vec::new();
    };
    let Some(plan) = plan_for(&cfg, &reads, &c) else {
        return Vec::new();
    };
    let outcome = follow(ex, &c, &plan).await;
    settle(ex, &c, outcome, now_ms())
}

/// A follow's first step, the lyrics' and the titles' (#269) alike: the
/// settings a look of `state` runs with at `now`. `None` = no look now: not
/// due yet; this node's transfers paused (nothing read, looked at again on
/// the next tick); or it asks no peer (SNV) or its settings do not hold (the
/// next look [`FOLLOW_IDLE`] later).
pub(super) async fn look_config<W: Copy>(
    ex: &Exchange,
    state: &std::sync::Mutex<FollowState<W>>,
    now: i64,
) -> Option<NodeConfig> {
    let due = lock(state).due(now);
    if !due || ex.transfers_paused().await {
        return None;
    }
    let cfg = asking_config(ex).await;
    if cfg.is_none() {
        lock(state).look_after(now, FOLLOW_IDLE);
    }
    cfg
}

/// The listed peers' catalogs as a look reads them: each peer's name with
/// its catalog, `None` where it could not be read (`Exchange::read_peers`,
/// in `cfg.peers` order).
pub(super) fn peer_reads<'a>(
    cfg: &'a NodeConfig,
    catalogs: &'a [Option<Arc<Catalog>>],
) -> Vec<(&'a str, Option<&'a Catalog>)> {
    cfg.peers
        .iter()
        .zip(catalogs)
        .map(|(peer, catalog)| (peer.name.as_str(), catalog.as_deref()))
        .collect()
}

/// This node's exchange settings when it asks peers; `None` when it asks
/// none (SNV) or they do not hold (WARNed).
async fn asking_config(ex: &Exchange) -> Option<NodeConfig> {
    match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg.asking().then_some(cfg),
        Err(e) => {
            warn!(error = %e, "exchange: a follow waits - the settings do not hold");
            None
        }
    }
}

/// A row of [`held_videos`]' query.
type HeldRow = (String, i64, Option<String>, Option<String>);

/// Every video this node's lyrics worker serves (`LYRICS_ELIGIBLE`), with no
/// row in its queue (`queued_later_where`: the lyrics hook's) and no
/// stand-in (the stand-in's look's), by YouTube id: whether a row serves
/// lyrics, and the copy taken from a peer (`peer_fetches`). Bare `?` only
/// (`rust-workspace.md`): kind, kind, job.
async fn held_videos(pool: &SqlitePool) -> Result<Vec<Held>, sqlx::Error> {
    let queued: HashSet<String> = lyrics_rows(pool, &queued_later_where())
        .await?
        .into_iter()
        .collect();
    let lyrics = ArtifactKind::Lyrics.as_str();
    let rows: Vec<HeldRow> = sqlx::query_as(&format!(
        "SELECT v.youtube_id, MAX(COALESCE(v.has_lyrics, 0) = 1), \
         (SELECT f.node FROM peer_fetches f WHERE f.youtube_id = v.youtube_id AND f.kind = ?), \
         (SELECT f.sha256 FROM peer_fetches f WHERE f.youtube_id = v.youtube_id AND f.kind = ?) \
         FROM videos v JOIN playlists p ON p.id = v.playlist_id \
         WHERE {LYRICS_ELIGIBLE} AND NOT EXISTS (SELECT 1 FROM peer_standins s \
             WHERE s.youtube_id = v.youtube_id AND s.job = ?) \
         GROUP BY v.youtube_id ORDER BY v.youtube_id"
    ))
    .bind(lyrics)
    .bind(lyrics)
    .bind(Job::Lyrics.as_str())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(youtube_id, ..)| !queued.contains(youtube_id))
        .map(|(youtube_id, has, node, sha256)| Held {
            youtube_id,
            has_lyrics: has == 1,
            took: node.zip(sha256),
        })
        .collect())
}

/// `c`'s fetch plan: its peer, its lyrics artifact, and the audio the same
/// catalog lists for the video (`decide::listed_audio`, the audio guard's
/// input).
fn plan_for(
    cfg: &NodeConfig,
    reads: &[(&str, Option<&Catalog>)],
    c: &Candidate,
) -> Option<FetchPlan> {
    let peer = cfg.peer(&c.peer)?.clone();
    let catalog = reads
        .iter()
        .find(|(name, _)| *name == c.peer)
        .and_then(|(_, catalog)| *catalog);
    Some(FetchPlan {
        peer,
        artifacts: vec![c.artifact.clone()],
        peer_audio: catalog
            .and_then(|catalog| listed_audio(catalog, &c.youtube_id))
            .cloned(),
    })
}

/// What a look did with its song.
#[derive(Debug)]
enum Outcome {
    /// Every row took the peer's copy (these rows); its origin is recorded.
    Taken(Vec<i64>),
    /// Only these rows took it (each other one WARNed): no origin recorded,
    /// asked again after [`FOLLOW_RETRY`].
    Partly(Vec<i64>),
    /// Kept here (why): asked again after [`FOLLOW_KEPT`].
    Kept(&'static str),
    /// Not now (why): asked again after [`FOLLOW_RETRY`].
    Later(PeerError),
    /// The take failed: asked again after [`FOLLOW_RETRY`].
    Failed(PeerError),
}

/// `c`'s lyrics follow `plan`, the peer's copy (the module doc).
async fn follow(ex: &Exchange, c: &Candidate, plan: &FetchPlan) -> Outcome {
    let youtube_id = c.youtube_id.as_str();
    let rows = match video_rows(&ex.pool, youtube_id).await {
        Ok(rows) => rows,
        Err(e) => return Outcome::Failed(e.into()),
    };
    let Some(check) = audio_row(&rows) else {
        return Outcome::Later(PeerError::NotYet("no row of the video is left".into()));
    };
    match kept_local(&ex.pool, youtube_id).await {
        Ok(false) => {}
        Ok(true) => return Outcome::Kept("an operator's lyrics or a dub here"),
        Err(e) => return Outcome::Failed(e.into()),
    }
    match ex.audio_verdict(plan, check, youtube_id).await {
        AudioVerdict::Same => {}
        AudioVerdict::Other => return Outcome::Kept("the peer's copy is of another audio"),
        AudioVerdict::NotNow(why) => return Outcome::Later(why),
    }
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    match take_into_rows(ex, youtube_id, plan, &ids).await {
        Ok(adopted) if adopted.len() == ids.len() => {
            ex.took(Job::Lyrics, youtube_id, &plan.peer.name, &plan.artifacts)
                .await;
            Outcome::Taken(adopted)
        }
        Ok(adopted) => Outcome::Partly(adopted),
        Err(e) => Outcome::Failed(e),
    }
}

/// `outcome` of `c` into the follow's memory and the log; the rows that took
/// the peer's copy.
fn settle(ex: &Exchange, c: &Candidate, outcome: Outcome, now_ms: i64) -> Vec<i64> {
    let mut state = lock(&ex.follow);
    match outcome {
        Outcome::Taken(rows) => {
            state.followed(c, now_ms);
            // Once its origin is recorded the copy is wanted no more; when
            // the record failed (WARNed by `took`), it waits like a failed
            // take instead of being fetched again on every tick.
            state.pass(c, now_ms, FOLLOW_RETRY);
            log_followed(c, rows.len());
            rows
        }
        Outcome::Partly(rows) => {
            let error = PeerError::Io("a row did not take the peer's lyrics".into());
            log_failed(c, &error);
            state.failed(c, now_ms, &error);
            rows
        }
        Outcome::Kept(why) => {
            log_passed(c, why);
            state.pass(c, now_ms, FOLLOW_KEPT);
            Vec::new()
        }
        Outcome::Later(why) => {
            log_passed(c, &why.to_string());
            state.pass(c, now_ms, FOLLOW_RETRY);
            Vec::new()
        }
        Outcome::Failed(error) => {
            log_failed(c, &error);
            state.failed(c, now_ms, &error);
            Vec::new()
        }
    }
}

/// The one INFO of a followed song. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_followed(c: &Candidate, rows: usize) {
    info!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        why = ?c.why,
        rows,
        sha256 = %c.artifact.sha256.get(..12).unwrap_or_default(),
        "exchange: followed the peer's lyrics"
    );
}

/// A song passed over for now. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_passed(c: &Candidate, why: &str) {
    debug!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        why,
        "exchange: the lyrics follow passes a song over for now"
    );
}

/// A take that failed. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_failed(c: &Candidate, error: &PeerError) {
    warn!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        %error,
        "exchange: a peer's newer lyrics were not taken - asking again later"
    );
}

#[cfg(test)]
#[path = "follow_tests.rs"]
mod tests;
