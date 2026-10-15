//! #277: a video Media Foundation stops on, repaired in place (design
//! comment 6185256383, Approach 1).
//!
//! Some YouTube H.264 streams carry SEI NAL units after an access unit's
//! slice data ("late SEI"); Media Foundation's H.264 decoder hands out no
//! picture of them and reports end-of-stream. A stream copy that drops every
//! SEI NAL unit (`-bsf:v filter_units=remove_types=6`, no re-encode: SEI
//! carries no picture data) decodes normally.
//!
//! Two ways in, one remux and one check:
//!
//! - a play that ends early (`playback::early_end`) [`ask`]s for a repair of
//!   its video; the video-upgrade worker runs ONE request per tick
//!   ([`tick`]), behind #230's background hold, whatever
//!   `video_upgrade_enabled` says, once per video and process (a busy one
//!   again [`BUSY_RETRY_MS`] later). The requests live in memory: a restart
//!   forgets them, and the next failing play asks again. [`repair`] is
//!   S11-shaped: the codec (H.264 only), the
//!   remux into `{id}_video_repair_temp.mp4`, the checks a replacement
//!   shares ([`super::verify_repair`]), then [`super::swap::swap`] under the
//!   song-file lock, the old file kept as `<name>.prev` (S12's retention
//!   reads `video_upgrade_at`, which a repair records with
//!   `video_upgrade_state = 'repaired'`; the cap stays, so the song is still
//!   checked for an upgrade);
//! - a fresh download ([`check_download`]): its video read through the
//!   reader that plays it before any row names it; one the reader hands out
//!   no picture of at all ([`has_no_picture`], the late-SEI case) is
//!   remuxed and renamed over itself, or the download fails, so a late-SEI
//!   download never reaches a playlist. A file that stops later is the
//!   player's to catch, with the play rule's slack (`playback::early_end`).
//!
//! Every way out but a repair removes the temp; a refused repair leaves the
//! file as it was. `peer_hashes` follows the new bytes by itself: an entry
//! holds only while the file keeps its size and mtime.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use super::worker::BUSY_RETRY_MS;
use super::{NO_PICTURE, Steps, VideoFacts, cached, swap, verify_repair};
use crate::background_hold::{self, Job};

/// What a successful repair records as the V35 state.
pub(crate) const REPAIRED: &str = "repaired";

/// The temp a repair remuxes into (the startup sweep's `TEMP_RE` removes
/// one a crash left).
pub(crate) fn repair_temp(cache_dir: &Path, youtube_id: &str) -> PathBuf {
    cache_dir.join(format!("{youtube_id}_video_repair_temp.mp4"))
}

/// Whether Media Foundation's name of a video codec is H.264, the one codec
/// whose SEI a repair drops.
fn is_h264(codec: &str) -> bool {
    codec.eq_ignore_ascii_case("H264")
}

/// The ffmpeg arguments of a repair: the video stream of `input` copied
/// (`-c copy`, never a re-encode) into `output`, every SEI NAL unit (type 6)
/// dropped by the `filter_units` bitstream filter, the index up front.
/// H.264 only: `None` for any other codec, whose NAL type 6 means something
/// else (an AV1 OBU of type 6 is a whole frame).
pub(crate) fn remux_args(codec: &str, input: &Path, output: &Path) -> Option<Vec<OsString>> {
    if !is_h264(codec) {
        return None;
    }
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(input.into());
    args.extend(
        [
            "-map",
            "0:v:0",
            "-c",
            "copy",
            "-bsf:v",
            "filter_units=remove_types=6",
            "-movflags",
            "+faststart",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args.push(output.into());
    Some(args)
}

/// Whether the reader's facts of a file say it opened but handed out no
/// picture at all ([`NO_PICTURE`]): the late-SEI file Media Foundation
/// stops on at once. A file whose first picture decodes is the player's to
/// judge (its slack is 10 s, the facts' near-end read is 2 s), and any other
/// error (the open itself) is not one a remux mends.
pub(crate) fn has_no_picture(facts: &Result<VideoFacts, String>) -> bool {
    matches!(facts, Err(error) if error == NO_PICTURE)
}

/// How one repair ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepairOutcome {
    /// The remuxed video is in place; the old one is `<name>.prev`.
    Repaired,
    /// Not H.264: nothing written.
    Unsupported,
    /// The remuxed video failed the check: nothing changed.
    Refused,
    /// A player holds the file: nothing changed.
    Busy,
    /// A step failed (the rows, the codec, ffmpeg, the audio): nothing
    /// changed.
    Failed,
}

/// What one repair did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RepairReport {
    pub youtube_id: String,
    pub outcome: RepairOutcome,
    /// The cached video's codec, as Media Foundation names it.
    pub codec: Option<String>,
    /// The cached video's facts, when the reader reads a picture of it.
    pub old: Option<VideoFacts>,
    /// The remuxed video's facts, once read.
    pub new: Option<VideoFacts>,
    pub error: Option<String>,
}

impl RepairReport {
    fn new(youtube_id: &str) -> Self {
        Self {
            youtube_id: youtube_id.to_string(),
            outcome: RepairOutcome::Failed,
            codec: None,
            old: None,
            new: None,
            error: None,
        }
    }

    fn ended(mut self, outcome: RepairOutcome, error: Option<String>) -> Self {
        self.outcome = outcome;
        self.error = error;
        self
    }
}

/// Repair `youtube_id`'s cached video (module doc), recording a repair at
/// `now_ms`. Every way out but a repair removes the temp.
pub(crate) async fn repair<S: Steps>(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: &str,
    steps: &S,
    now_ms: i64,
) -> RepairReport {
    let temp = repair_temp(cache_dir, youtube_id);
    let report = attempt(pool, youtube_id, steps, &temp).await;
    if report.outcome != RepairOutcome::Repaired {
        let _ = std::fs::remove_file(&temp);
    } else if let Err(e) = super::record(pool, youtube_id, None, REPAIRED, now_ms).await {
        warn!(youtube_id, "video repair: the repair was not recorded: {e}");
    }
    report
}

async fn attempt<S: Steps>(
    pool: &SqlitePool,
    youtube_id: &str,
    steps: &S,
    temp: &Path,
) -> RepairReport {
    let mut report = RepairReport::new(youtube_id);
    let song = match cached(pool, youtube_id).await {
        Ok(Some(song)) => song,
        Ok(None) => {
            return report.ended(RepairOutcome::Failed, Some("not downloaded".into()));
        }
        Err(e) => {
            return report.ended(RepairOutcome::Failed, Some(format!("the rows: {e}")));
        }
    };
    report.old = steps.facts(&song.video).await.ok();
    if let Some((outcome, why)) =
        remux_checked(steps, &song.video, &song.audio, temp, &mut report).await
    {
        return report.ended(outcome, Some(why));
    }
    match swap::swap(pool, youtube_id, &song.video, temp).await {
        swap::Swapped::Done => report.ended(RepairOutcome::Repaired, None),
        swap::Swapped::Busy(why) => report.ended(RepairOutcome::Busy, Some(why)),
        swap::Swapped::Refused(why) => report.ended(RepairOutcome::Failed, Some(why)),
    }
}

/// `video` remuxed into `temp` and checked against `report.old` and the
/// audio at `audio`: `None` = `temp` may replace `video`, else how the
/// repair ended and why. Fills the report's codec and new facts.
async fn remux_checked<S: Steps>(
    steps: &S,
    video: &Path,
    audio: &Path,
    temp: &Path,
    report: &mut RepairReport,
) -> Option<(RepairOutcome, String)> {
    let codec = match steps.codec(video).await {
        Ok(codec) => codec,
        Err(e) => return Some((RepairOutcome::Failed, format!("the codec: {e}"))),
    };
    let Some(args) = remux_args(&codec, video, temp) else {
        let why = format!("{codec} is not H.264");
        report.codec = Some(codec);
        return Some((RepairOutcome::Unsupported, why));
    };
    report.codec = Some(codec);
    let _ = std::fs::remove_file(temp);
    if let Err(e) = steps.ffmpeg(&args).await {
        return Some((RepairOutcome::Failed, format!("the remux: {e}")));
    }
    let new = match steps.facts(temp).await {
        Ok(new) => new,
        Err(e) => {
            let why = format!("the repaired video: {e}");
            return Some((RepairOutcome::Refused, why));
        }
    };
    report.new = Some(new);
    let audio_ms = match steps.audio_ms(audio).await {
        Ok(ms) => ms,
        Err(e) => return Some((RepairOutcome::Failed, format!("the audio: {e}"))),
    };
    let checked = verify_repair(report.old.as_ref(), &new, audio_ms);
    checked.err().map(|why| (RepairOutcome::Refused, why))
}

/// What the download-time check did with a fresh video (#277).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checked {
    /// The reader reads a picture of it (or does not open it at all): left
    /// as it is, for the player to judge.
    Plays,
    /// The reader read no picture of it; the remuxed video took its place.
    Repaired,
}

/// #277: a fresh download's `video` read through the reader that plays it,
/// before any row names it. One it reads no picture of ([`has_no_picture`])
/// is remuxed into `temp`, checked against the normalized `audio` and
/// renamed over `video`. `Err` = it plays no picture and was not repaired:
/// the download fails, so it never reaches a playlist.
pub(crate) async fn check_download<S: Steps>(
    steps: &S,
    youtube_id: &str,
    video: &Path,
    audio: &Path,
    temp: &Path,
) -> Result<Checked, String> {
    let read = steps.facts(video).await;
    if !has_no_picture(&read) {
        return Ok(Checked::Plays);
    }
    info!(
        youtube_id,
        "download: the reader that plays the video reads no picture of it - repairing it (#277)"
    );
    let mut report = RepairReport::new(youtube_id);
    report.old = read.ok();
    if let Some((outcome, why)) = remux_checked(steps, video, audio, temp, &mut report).await {
        let _ = std::fs::remove_file(temp);
        return Err(format!(
            "the video plays no picture and was not repaired ({outcome:?}): {why}"
        ));
    }
    if let Err(e) = std::fs::rename(temp, video) {
        let _ = std::fs::remove_file(temp);
        return Err(format!("the repaired video did not take its place: {e}"));
    }
    info!(
        youtube_id,
        "download: the repaired video is in place (#277)"
    );
    Ok(Checked::Repaired)
}

/// The repairs plays asked for, in memory (module doc).
#[derive(Debug, Default)]
pub(crate) struct Requests {
    /// Every video row asked for in this process.
    asked: BTreeSet<i64>,
    /// Asked and not taken yet, oldest first: the video row and the instant
    /// (ms) before which it waits (0 = at once).
    pending: VecDeque<(i64, i64)>,
    /// The YouTube ids a repair ran for in this process: rows of one video
    /// share its file, so it is repaired once (a busy one again, below).
    tried: BTreeSet<String>,
    /// The YouTube ids whose repair a player's open file refused (`Busy`),
    /// with the instant (ms) of their one retry: until then no row of the
    /// video runs a repair.
    busy_until: BTreeMap<String, i64>,
}

impl Requests {
    pub(crate) const fn new() -> Self {
        Self {
            asked: BTreeSet::new(),
            pending: VecDeque::new(),
            tried: BTreeSet::new(),
            busy_until: BTreeMap::new(),
        }
    }

    /// Ask for a repair of video row `video_id`: `true` the first time in
    /// this process (it is queued), `false` after.
    pub(crate) fn ask(&mut self, video_id: i64) -> bool {
        let first = self.asked.insert(video_id);
        if first {
            self.pending.push_back((video_id, 0));
        }
        first
    }

    /// Whether video row `video_id` was asked for in this process.
    #[cfg(test)]
    pub(crate) fn is_asked(&self, video_id: i64) -> bool {
        self.asked.contains(&video_id)
    }

    /// The oldest request due at `now_ms`, taken.
    pub(crate) fn take(&mut self, now_ms: i64) -> Option<i64> {
        let at = self.pending.iter().position(|&(_, due)| due <= now_ms)?;
        self.pending.remove(at).map(|(video_id, _)| video_id)
    }

    /// A taken request held back (#230's hold): first again, due at once.
    fn put_back(&mut self, video_id: i64) {
        self.pending.push_front((video_id, 0));
    }

    /// A repair of `youtube_id` refused by a player that held the file
    /// (`Busy`): its video row asked again, behind the requests already
    /// queued, due at `due_ms`, when the video is tried once more.
    fn retry_later(&mut self, video_id: i64, youtube_id: &str, due_ms: i64) {
        self.busy_until.insert(youtube_id.to_string(), due_ms);
        self.pending.push_back((video_id, due_ms));
    }

    /// Whether a repair of `youtube_id` may run at `now_ms`: the first time
    /// in this process, or once its busy retry is due (a request of another
    /// row of the video before then runs nothing: the retry covers the
    /// shared file).
    pub(crate) fn first_try(&mut self, youtube_id: &str, now_ms: i64) -> bool {
        if let Some(&due_ms) = self.busy_until.get(youtube_id) {
            let due = due_ms <= now_ms;
            if due {
                self.busy_until.remove(youtube_id);
            }
            return due;
        }
        self.tried.insert(youtube_id.to_string())
    }
}

/// The process's requests (the engine asks, the worker takes).
static REQUESTS: Mutex<Requests> = Mutex::new(Requests::new());

/// The process's requests.
pub(crate) fn requests() -> &'static Mutex<Requests> {
    &REQUESTS
}

/// The engine's ask (`failure_retry::video_failed`): a repair of video row
/// `video_id`, whose play ended early.
pub(crate) fn ask(video_id: i64) {
    let first = requests()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .ask(video_id);
    if first {
        info!(
            video_id,
            "video repair: asked - the video ended early (#277)"
        );
    } else {
        debug!(video_id, "video repair: asked before in this process");
    }
}

/// The YouTube id of video row `video_id`.
async fn youtube_id_of(pool: &SqlitePool, video_id: i64) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT youtube_id FROM videos WHERE id = ?")
        .bind(video_id)
        .fetch_optional(pool)
        .await
}

/// One repair step of the worker (module doc): the oldest request due,
/// behind #230's background hold (a held request waits for the next tick),
/// once per video and process; a repair a player's open file refused
/// (`Busy`) is asked again [`BUSY_RETRY_MS`] later. `None` = no repair ran.
pub(crate) async fn tick<S: Steps>(
    pool: &SqlitePool,
    cache_dir: &Path,
    requests: &Mutex<Requests>,
    steps: &S,
    now_ms: i64,
) -> Option<RepairReport> {
    let lock = || requests.lock().unwrap_or_else(PoisonError::into_inner);
    let video_id = lock().take(now_ms)?;
    if background_hold::holds(pool, Job::VideoUpgrade).await {
        lock().put_back(video_id);
        return None;
    }
    let youtube_id = match youtube_id_of(pool, video_id).await {
        Ok(Some(youtube_id)) => youtube_id,
        Ok(None) => {
            warn!(
                video_id,
                "video repair: the row is gone - nothing to repair"
            );
            return None;
        }
        Err(e) => {
            warn!(video_id, "video repair: the row could not be read: {e}");
            return None;
        }
    };
    if !lock().first_try(&youtube_id, now_ms) {
        debug!(youtube_id = %youtube_id, "video repair: tried before in this process, or busy");
        return None;
    }
    info!(youtube_id = %youtube_id, video_id, "video repair: start");
    let report = repair(pool, cache_dir, &youtube_id, steps, now_ms).await;
    if report.outcome == RepairOutcome::Busy {
        lock().retry_later(video_id, &youtube_id, now_ms + BUSY_RETRY_MS);
    }
    log_outcome(&report);
    Some(report)
}

/// The one line a repair ends with: INFO when the remuxed video is in place
/// (with the first picture's time the reader reads, the one check skipped
/// when no picture of the old file was read), else a WARN naming why (a
/// video is tried once per process, a busy one again every 10 min).
#[cfg_attr(test, mutants::skip)] // log only; the outcomes are tested
fn log_outcome(report: &RepairReport) {
    let rows = |facts: &Option<VideoFacts>| facts.map(|facts| facts.height);
    if report.outcome == RepairOutcome::Repaired {
        info!(
            youtube_id = %report.youtube_id,
            codec = ?report.codec,
            old_rows = ?rows(&report.old),
            new_rows = ?rows(&report.new),
            new_first_ms = ?report.new.map(|facts| facts.first_ms),
            "video repair: done - the remuxed video is in place, the old one kept as .prev"
        );
    } else {
        warn!(
            youtube_id = %report.youtube_id,
            outcome = ?report.outcome,
            codec = ?report.codec,
            error = report.error.as_deref().unwrap_or(""),
            "video repair: not repaired - tried again only when busy (10 min), else after a restart"
        );
    }
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
