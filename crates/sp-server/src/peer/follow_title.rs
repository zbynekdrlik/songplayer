//! #269: a node follows its peers' titles, as #264 follows their lyrics
//! (`peer::follow`). The owner's rule (13.10.2026): what SNV improves must
//! reach PP. An operator's title correction at SNV
//! (`metadata::manual::apply_to_video`, `'manual'`) changes SNV's catalog
//! entry, yet PP asks a peer for a title only at its own download
//! (`peer::download`) and in its metadata repair (`peer::repair`, the rows
//! of `REPAIR_QUEUE_WHERE`), so the correction never reached a song PP
//! already held.
//!
//! The lyrics worker's tick runs [`follow_next`] right after the lyrics
//! follow, with the same cadence and memory (`follow::FollowState`, its own
//! copy: `Exchange::title_follow`): ONE video per look, the next look the
//! next tick while a look finds one, else `FOLLOW_IDLE` later. Nothing is
//! read while this node's transfers are paused (the operator's pause, #230's
//! hold), nor at a node that asks no peer (SNV).
//!
//! - A video follows ([`candidates`]) when this node holds it downloaded (a
//!   row with `normalized = 1` that the queues take: not removed, not the
//!   test item), none of its rows is in the metadata repair queue (the
//!   repair's: it takes a peer's title first), and the first listed peer
//!   whose catalog lists its title at a version this node takes (a
//!   provider's or an operator's, `kind::acceptable`) lists one [`wants`]
//!   takes.
//! - [`wants`]: a title byte for byte the peer's with no record of it (a
//!   phase-0 copy) is recorded as the peer's copy, nothing taken
//!   (ROZHODNUTÉ on #269); never over a correction made here (a `'manual'`
//!   title that is not the copy recorded from that peer); else a title of a
//!   higher rank (`kind::metadata_version`: an operator's over a provider's
//!   over a parser's), or, when this node's title IS the copy it recorded
//!   from that peer, the one the peer lists now (corrected or improved
//!   there).
//! - A download run here keeps the title's record while the title is still
//!   that copy (`Exchange::forget_origins`, [`title_is_recorded_copy`]).
//! - The take (`take`): the peer's `/videos` row (`PeerTitle::of`), its
//!   sha256 checked against the catalog entry as the repair checks it; then
//!   under `cache::SONG_FILES` the look's answer is checked again (a
//!   correction made here meanwhile stays), every row's files are renamed
//!   after the title (`metadata::manual::rename_rows`), and only when every
//!   set moved is the title written on every row with the peer's label
//!   (`metadata::manual::spread_title`: a peer's operator title stays
//!   `'manual'` here and keeps its rank) and its origin recorded
//!   (`download::record_title`, the look after sees it as the peer's copy).
//!   A file that could not move (held open on Windows) leaves the title as
//!   it was, so the next look after `FOLLOW_RETRY` tries again.
//!
//! No paid AI: the follow only reads the peer.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use super::Exchange;
use super::catalog::metadata_for;
use super::client::{PeerError, lock};
use super::config::PeerConfig;
use super::download::{PeerTitle, record_title};
use super::follow::{Candidate, FOLLOW_IDLE, FOLLOW_RETRY, FollowStatus, look_config, peer_reads};
use super::kind::{ArtifactKind, METADATA_MANUAL, acceptable};
use super::wire::{Artifact, Catalog, PeerMetadata, now_ms};
use crate::db::models_peer;
use crate::metadata::health::REPAIR_QUEUE_WHERE;
use crate::metadata::manual::{rename_rows, spread_title};

/// Why a video's title follows a peer's, in the order a look takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleWhy {
    /// The peer's title ranks above this node's: an operator's over a
    /// provider's or a parser's, a provider's over a parser's.
    Higher,
    /// This node's title is the copy it took from that peer, and the peer
    /// names the video otherwise now.
    Changed,
    /// This node's title is byte for byte the one the peer lists, with no
    /// record of it (a phase-0 copy, an identical correction): its origin is
    /// recorded and nothing is taken (ROZHODNUTÉ on #269), so the peer's
    /// later corrections follow.
    Same,
}

/// What this node holds of one video's title ([`held_titles`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldTitle {
    pub youtube_id: String,
    /// Its metadata version (`kind::metadata_version`) as this node's
    /// catalog lists it.
    pub version: u32,
    /// The sha256 of the title as this node's catalog lists it
    /// (`PeerMetadata::to_bytes`).
    pub sha256: String,
    /// The title this node took from a peer: the `peer_fetches` metadata
    /// record's node and sha256 (of the peer's bytes).
    pub took: Option<(String, String)>,
}

impl HeldTitle {
    /// The title here is the copy this node recorded from `peer`, and
    /// nothing here changed it since.
    fn is_copy_of(&self, peer: &str) -> bool {
        self.took
            .as_ref()
            .is_some_and(|(node, sha256)| node == peer && *sha256 == self.sha256)
    }
}

/// Why `held` takes `listed`, the title `peer` lists now (the module doc);
/// `None` = this node keeps its own.
pub fn wants(held: &HeldTitle, peer: &str, listed: &Artifact) -> Option<TitleWhy> {
    if !acceptable(ArtifactKind::Metadata, listed.version) {
        return None;
    }
    let copy = held.is_copy_of(peer);
    // The very bytes the peer lists (so the same rank too): recorded once.
    if listed.sha256 == held.sha256 {
        return (!copy).then_some(TitleWhy::Same);
    }
    // An operator's correction made here is final.
    if held.version == METADATA_MANUAL && !copy {
        return None;
    }
    if listed.version > held.version {
        return Some(TitleWhy::Higher);
    }
    copy.then_some(TitleWhy::Changed)
}

/// The videos of `held` whose title follows a peer's, in the order a look
/// takes them ([`TitleWhy`]'s order, then the YouTube id). Per video the
/// first of `reads` (the listed peers' catalogs in `peers` order, `None`
/// where one could not be read) that lists its title at a version this node
/// takes, and only when [`wants`] says so.
pub fn candidates(
    held: &[HeldTitle],
    reads: &[(&str, Option<&Catalog>)],
) -> Vec<Candidate<TitleWhy>> {
    let mut found: Vec<Candidate<TitleWhy>> = held
        .iter()
        .filter_map(|h| {
            let (peer, listed) = reads.iter().find_map(|&(peer, catalog)| {
                Some((peer, listed_title(catalog?, &h.youtube_id)?))
            })?;
            let why = wants(h, peer, listed)?;
            Some(Candidate {
                youtube_id: h.youtube_id.clone(),
                peer: peer.to_string(),
                why,
                artifact: listed.clone(),
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

/// The title `catalog` lists for `youtube_id` at a version this node takes.
fn listed_title<'a>(catalog: &'a Catalog, youtube_id: &str) -> Option<&'a Artifact> {
    catalog.artifacts.iter().find(|a| {
        a.youtube_id == youtube_id
            && a.kind == ArtifactKind::Metadata
            && acceptable(a.kind, a.version)
    })
}

impl Exchange {
    /// `title_follow` for `GET /api/v1/exchange/status`.
    pub fn title_follow_status(&self) -> FollowStatus<TitleWhy> {
        lock(&self.title_follow).status.clone()
    }
}

/// The lyrics worker's tick, right after the lyrics follow (the module
/// doc): at most ONE video's title follows a peer's. Answers the rows that
/// took it.
pub async fn follow_next(ex: Option<&Arc<Exchange>>) -> Vec<i64> {
    let Some(ex) = ex else {
        return Vec::new();
    };
    let now = now_ms();
    let Some(cfg) = look_config(ex, &ex.title_follow, now).await else {
        return Vec::new();
    };
    let held = match held_titles(&ex.pool, None).await {
        Ok(held) => held,
        Err(e) => {
            warn!(%e, "exchange: the title follow could not read this node's rows");
            lock(&ex.title_follow).look_after(now, FOLLOW_IDLE);
            return Vec::new();
        }
    };
    let catalogs = ex.read_peers(&cfg.peers).await;
    let found = candidates(&held, &peer_reads(&cfg, &catalogs));
    let picked = lock(&ex.title_follow).pick(&found, now);
    let Some(c) = picked else {
        return Vec::new();
    };
    let Some(peer) = cfg.peer(&c.peer) else {
        return Vec::new();
    };
    let outcome = take(ex, &c, peer).await;
    settle(ex, &c, outcome, now_ms())
}

/// A row the queues take (`queue_row!()`: not removed, not the test item).
const QUEUE_ROW: &str = crate::db::queue_row!();

/// A row of [`held_titles`]' query: the YouTube id and the `peer_fetches`
/// metadata record's node and sha256.
type HeldRow = (String, Option<String>, Option<String>);

/// Every video this node holds downloaded (a row with `normalized = 1` the
/// queues take) with none of its rows in the metadata repair queue (the
/// repair's own: `REPAIR_QUEUE_WHERE`, read in its subquery's scope), by
/// YouTube id, with its title as this node's catalog lists it
/// (`catalog::metadata_for`: a video with no song has none and is left out)
/// and the copy it took from a peer (`peer_fetches`); `youtube_id` = that
/// video alone. Bare `?` only (`rust-workspace.md`): kind, kind, the id
/// twice.
pub(crate) async fn held_titles(
    pool: &SqlitePool,
    youtube_id: Option<&str>,
) -> Result<Vec<HeldTitle>, sqlx::Error> {
    let metadata = ArtifactKind::Metadata.as_str();
    let rows: Vec<HeldRow> = sqlx::query_as(&format!(
        "SELECT v.youtube_id, \
         (SELECT f.node FROM peer_fetches f WHERE f.youtube_id = v.youtube_id AND f.kind = ?), \
         (SELECT f.sha256 FROM peer_fetches f WHERE f.youtube_id = v.youtube_id AND f.kind = ?) \
         FROM videos v WHERE v.normalized = 1 AND {QUEUE_ROW} \
         AND NOT EXISTS (SELECT 1 FROM videos r WHERE r.youtube_id = v.youtube_id \
             AND {REPAIR_QUEUE_WHERE}) \
         AND (? IS NULL OR v.youtube_id = ?) \
         GROUP BY v.youtube_id ORDER BY v.youtube_id"
    ))
    .bind(metadata)
    .bind(metadata)
    .bind(youtube_id)
    .bind(youtube_id)
    .fetch_all(pool)
    .await?;
    let own: HashMap<String, PeerMetadata> = metadata_for(pool, youtube_id)
        .await?
        .into_iter()
        .map(|m| (m.youtube_id.clone(), m))
        .collect();
    Ok(rows
        .into_iter()
        .filter_map(|(youtube_id, node, sha256)| {
            let title = own.get(&youtube_id)?;
            Some(HeldTitle {
                youtube_id,
                version: title.version(),
                sha256: title.sha256(),
                took: node.zip(sha256),
            })
        })
        .collect())
}

/// This node's title of `youtube_id` is the copy it recorded from a peer:
/// the `peer_fetches` metadata record's sha256 is the one of the title as
/// this node's catalog lists it. A download run here keeps that record
/// (ROZHODNUTÉ on #269, `Exchange::forget_origins`): the title it keeps is
/// still the peer's, so the peer's later corrections still follow.
pub(crate) async fn title_is_recorded_copy(
    pool: &SqlitePool,
    youtube_id: &str,
) -> Result<bool, sqlx::Error> {
    let metadata = ArtifactKind::Metadata.as_str();
    let Some((_, _, recorded)) = models_peer::fetch_record(pool, youtube_id, metadata).await?
    else {
        return Ok(false);
    };
    let own = metadata_for(pool, Some(youtube_id)).await?;
    Ok(own.first().is_some_and(|title| title.sha256() == recorded))
}

/// What a look did with its video.
#[derive(Debug)]
enum Outcome {
    /// Every row took the peer's title (these rows; none for
    /// [`TitleWhy::Same`], whose title already was the peer's); its origin
    /// is recorded.
    Taken(Vec<i64>),
    /// Not now (why): asked again after [`FOLLOW_RETRY`].
    Later(PeerError),
    /// The take failed: asked again after [`FOLLOW_RETRY`].
    Failed(PeerError),
}

/// `c`'s title follows the peer's (the module doc).
async fn take(ex: &Exchange, c: &Candidate<TitleWhy>, peer: &PeerConfig) -> Outcome {
    if c.why == TitleWhy::Same {
        return match record_same(ex, c).await {
            Ok(true) => Outcome::Taken(Vec::new()),
            Ok(false) => Outcome::Later(PeerError::NotYet(
                "this node's title changed meanwhile".into(),
            )),
            Err(e) => Outcome::Failed(e.into()),
        };
    }
    let video = match ex.client.video(peer, &c.youtube_id).await {
        Ok(video) => video,
        Err(e) => return Outcome::Failed(e),
    };
    let Some(taken) = PeerTitle::of(&peer.name, &video.metadata) else {
        let why = "the peer's row names no title this node takes";
        return Outcome::Failed(PeerError::BadResponse(why.into()));
    };
    // A read between two changes there: the catalog read after the cached
    // one lists the newer title.
    if taken.sha256 != c.artifact.sha256 {
        return Outcome::Failed(PeerError::ShaMismatch {
            expected: c.artifact.sha256.clone(),
            got: taken.sha256,
        });
    }
    match apply(ex, c, &taken).await {
        Ok(Applied::Rows(rows)) => {
            record_title(ex, &c.youtube_id, &taken).await;
            Outcome::Taken(rows)
        }
        Ok(Applied::Busy) => Outcome::Later(PeerError::NotYet(
            "a file of the song could not be renamed (in use)".into(),
        )),
        Ok(Applied::Kept) => Outcome::Later(PeerError::NotYet(
            "this node's title changed meanwhile".into(),
        )),
        Err(e) => Outcome::Failed(e.into()),
    }
}

/// What [`apply`] did.
enum Applied {
    /// Every row took the title (these rows).
    Rows(Vec<i64>),
    /// A file could not move: no title written, every set recorded where it
    /// is.
    Busy,
    /// The look's answer no longer holds (a correction made here
    /// meanwhile, a row now in the repair queue): nothing touched.
    Kept,
}

/// `taken` into every row of `c`'s video, under `cache::SONG_FILES` (a title
/// PATCH, the repair and the re-link hold it too, so none interleaves):
/// the look's answer checked again, the files renamed first, and the title
/// written only once every set moved (the module doc).
async fn apply(
    ex: &Exchange,
    c: &Candidate<TitleWhy>,
    taken: &PeerTitle,
) -> Result<Applied, sqlx::Error> {
    let _files = crate::downloader::cache::SONG_FILES.lock().await;
    if wants_now(ex, c).await?.is_none() {
        return Ok(Applied::Kept);
    }
    let t = &taken.title;
    let title = Some((t.song.as_str(), t.artist.as_str()));
    if !rename_rows(&ex.pool, &ex.cache_dir, &c.youtube_id, title).await? {
        return Ok(Applied::Busy);
    }
    let artist = Some(t.artist.as_str());
    let rows = spread_title(&ex.pool, &c.youtube_id, &t.song, artist, t.source).await?;
    Ok(Applied::Rows(rows))
}

/// `c`'s title is byte for byte the peer's ([`TitleWhy::Same`]): only its
/// origin is recorded (`peer_fetches` kind `metadata`, the peer and the sha
/// its catalog lists), checked again under `cache::SONG_FILES` like a take
/// (a correction made here meanwhile is no copy of the peer's). `false` =
/// the look's answer no longer holds, nothing recorded.
async fn record_same(ex: &Exchange, c: &Candidate<TitleWhy>) -> Result<bool, sqlx::Error> {
    let _files = crate::downloader::cache::SONG_FILES.lock().await;
    if wants_now(ex, c).await? != Some(TitleWhy::Same) {
        return Ok(false);
    }
    let metadata = ArtifactKind::Metadata.as_str();
    let (peer, version, sha256) = (&c.peer, c.artifact.version, &c.artifact.sha256);
    models_peer::record_fetch(
        &ex.pool,
        &c.youtube_id,
        metadata,
        peer,
        version,
        sha256,
        now_ms(),
    )
    .await?;
    log_same(c);
    Ok(true)
}

/// The look's answer for `c`, read again from this node's rows (the caller
/// holds `cache::SONG_FILES`).
async fn wants_now(
    ex: &Exchange,
    c: &Candidate<TitleWhy>,
) -> Result<Option<TitleWhy>, sqlx::Error> {
    let held = held_titles(&ex.pool, Some(c.youtube_id.as_str())).await?;
    Ok(held.first().and_then(|h| wants(h, &c.peer, &c.artifact)))
}

/// `outcome` of `c` into the follow's memory and the log; the rows that took
/// the peer's title.
fn settle(ex: &Exchange, c: &Candidate<TitleWhy>, outcome: Outcome, now_ms: i64) -> Vec<i64> {
    let mut state = lock(&ex.title_follow);
    match outcome {
        Outcome::Taken(rows) => {
            // Wanted no more: the title here is now the peer's, of its rank
            // (a record that failed, WARNed by `record_title`, leaves it
            // this node's own: never read again on every tick either).
            state.followed(c, now_ms);
            log_followed(c, rows.len());
            rows
        }
        Outcome::Later(why) => {
            log_passed(c, &why);
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

/// A followed title, beside `record_title`'s one INFO. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_followed(c: &Candidate<TitleWhy>, rows: usize) {
    debug!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        why = ?c.why,
        rows,
        sha256 = %c.artifact.sha256.get(..12).unwrap_or_default(),
        "exchange: followed the peer's title"
    );
}

/// The one INFO of a title recorded as the peer's copy. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_same(c: &Candidate<TitleWhy>) {
    info!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        sha256 = %c.artifact.sha256.get(..12).unwrap_or_default(),
        "exchange: this node's title is the peer's - its origin recorded"
    );
}

/// A video passed over for now. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_passed(c: &Candidate<TitleWhy>, why: &PeerError) {
    debug!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        %why,
        "exchange: the title follow passes a song over for now"
    );
}

/// A take that failed. Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_failed(c: &Candidate<TitleWhy>, error: &PeerError) {
    warn!(
        youtube_id = %c.youtube_id,
        peer = %c.peer,
        %error,
        "exchange: a peer's newer title was not taken - asking again later"
    );
}

#[cfg(test)]
#[path = "follow_title_tests.rs"]
mod tests;
