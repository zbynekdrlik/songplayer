//! #250: the lyrics worker's register check (`register_check.rs`), split
//! from `worker.rs` (1000-line cap). ONE check per track, made while it is
//! processed — right after its translation, before it is persisted — and once
//! for every served track checked under an older version (or never), by the
//! lowest-priority stale pass. Nothing is checked at display.

use std::time::{Duration, Instant};

use sp_core::lyrics::{LyricsTrack, RegisterCheck, RegisterVerdict};
use tracing::{debug, info, warn};

use crate::db::models_register::{
    fetch_next_stale_register, fetch_stale_registers, stamp_register,
};
use crate::lyrics::register_check::{self, REGISTER_CHECK_VERSION};
use crate::lyrics::worker::LyricsWorker;

/// What happened to a track's register check this pass.
#[derive(Debug, PartialEq)]
pub(crate) enum CheckOutcome {
    /// Checked (or refused): the check is in the track.
    Checked,
    /// No AI client, or this node's paid AI is off: nothing asked.
    NotAsked,
    /// The call failed (the proxy down, a 5xx): asked again later, never
    /// counted toward giving the song up.
    Failed,
    /// Claude answered in a shape the check cannot read: asked again later,
    /// given up after [`MAX_FAILED_CHECKS`].
    Unreadable,
}

/// Is `track`'s stored check already the current one?
pub(crate) fn checked_now(track: &LyricsTrack) -> bool {
    track
        .register
        .as_ref()
        .is_some_and(|c| c.version >= REGISTER_CHECK_VERSION)
}

/// The INFO / WARN line of a finished check.
#[cfg_attr(test, mutants::skip)] // a log line only
fn log_check(youtube_id: &str, check: &RegisterCheck) {
    match check.verdict {
        RegisterVerdict::Ok => info!(youtube_id, "register check: every line reads right"),
        RegisterVerdict::Fixed => info!(
            youtube_id,
            fixed = check.fixed.len(),
            "register check: misheard lines fixed"
        ),
        RegisterVerdict::Flagged => warn!(
            youtube_id,
            fixed = check.fixed.len(),
            flagged = check.flagged.len(),
            "register check: lines flagged for the operator"
        ),
        RegisterVerdict::Refused => warn!(
            youtube_id,
            "register check: Claude refused the check - the track is kept as it is"
        ),
    }
}

impl LyricsWorker {
    /// Check `track` (EN + SK) and keep the check in it. Asked once its
    /// translation is done; the track is persisted with whatever came of it.
    #[cfg_attr(test, mutants::skip)] // the call glue; the parts are tested
    pub(crate) async fn check_register(
        &self,
        track: &mut LyricsTrack,
        youtube_id: &str,
    ) -> CheckOutcome {
        let Some(ai_client) = &self.ai_client else {
            return CheckOutcome::NotAsked;
        };
        // #229 item C: the check is a Claude call (paid AI), held with the
        // translation it belongs to.
        if !self.translation_allowed(youtube_id).await {
            return CheckOutcome::NotAsked;
        }
        match register_check::check_via_claude(ai_client, track).await {
            Ok(check) => {
                log_check(youtube_id, &check);
                track.register = Some(check);
                CheckOutcome::Checked
            }
            Err(e) => {
                warn!(
                    youtube_id,
                    "register check failed - checked again later: {e:#}"
                );
                if register_check::unreadable(&e) {
                    CheckOutcome::Unreadable
                } else {
                    CheckOutcome::Failed
                }
            }
        }
    }

    /// Record `track`'s check on every row of `video_id`'s video: its version
    /// and verdict, or nothing when the track carries none (the stale pass
    /// checks it later).
    pub(crate) async fn stamp_register_of(&self, video_id: i64, track: &LyricsTrack) {
        let Some(check) = &track.register else {
            return;
        };
        if let Err(e) = stamp_register(
            &self.pool,
            video_id,
            check.version,
            Some(check.verdict.as_str()),
        )
        .await
        {
            warn!(video_id, "register check: the stamp failed: {e}");
        }
    }

    /// Check ONE served track not checked under the current version: read
    /// it, check it, write it back in place, stamp it. Lowest priority (run
    /// with the translation passes); shares their backoff, so a refusing or
    /// unreachable Claude is not asked every tick. A node that checks
    /// nothing itself (no AI client, paid AI off: PP) only stamps the checks
    /// its peers' copies carry ([`Self::stamp_peer_checks`]).
    #[cfg_attr(test, mutants::skip)] // the pass glue; its paths are tested end to end
    pub(crate) async fn recheck_next_stale_register(&self) {
        if self.ai_client.is_none() || !crate::paid_ai::enabled(&self.pool).await {
            self.stamp_peer_checks().await;
            return;
        }
        {
            let backoff = self.retry_backoff.lock().await;
            if backoff
                .silent_until
                .is_some_and(|until| Instant::now() < until)
            {
                return;
            }
        }
        let Ok(Some((video_id, youtube_id))) =
            fetch_next_stale_register(&self.pool, REGISTER_CHECK_VERSION).await
        else {
            return;
        };
        let path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let Some(mut track) = read_track(&path).await else {
            // Nothing to check: stamped forward so the pass moves on.
            debug!(
                youtube_id,
                "register check: no readable track - stamped forward"
            );
            let _ = stamp_register(&self.pool, video_id, REGISTER_CHECK_VERSION, None).await;
            return;
        };
        if checked_now(&track) {
            // A peer's copy already carries the check (PP takes SNV's).
            self.stamp_register_of(video_id, &track).await;
            return;
        }
        match self.check_register(&mut track, &youtube_id).await {
            CheckOutcome::Checked => {
                let json = serde_json::to_vec(&track).unwrap_or_default();
                if let Err(e) = tokio::fs::write(&path, &json).await {
                    // Never a new Claude call every tick for a file that
                    // cannot be written: it backs off and counts.
                    warn!(youtube_id, "register check: write failed: {e}");
                    self.count_failed_check(video_id, &youtube_id).await;
                    return;
                }
                self.stamp_register_of(video_id, &track).await;
                let mut backoff = self.retry_backoff.lock().await;
                backoff.register_failures.remove(&video_id);
                let refused = track
                    .register
                    .as_ref()
                    .is_some_and(|c| c.verdict == RegisterVerdict::Refused);
                if refused {
                    // Every refusal can cool the proxy's credential down:
                    // the next song waits like after a failure.
                    back_off(&mut backoff);
                } else {
                    backoff.consecutive_failures = 0;
                    backoff.silent_until = None;
                }
            }
            CheckOutcome::NotAsked => {}
            CheckOutcome::Failed => {
                let mut backoff = self.retry_backoff.lock().await;
                back_off(&mut backoff);
            }
            CheckOutcome::Unreadable => self.count_failed_check(video_id, &youtube_id).await,
        }
    }

    /// A check of `video_id` that came to nothing (an answer it could not
    /// read, a write that failed): back off, and after [`MAX_FAILED_CHECKS`]
    /// stamp the song forward with no verdict so the pass moves on.
    async fn count_failed_check(&self, video_id: i64, youtube_id: &str) {
        let mut backoff = self.retry_backoff.lock().await;
        back_off(&mut backoff);
        let failures = backoff.register_failures.entry(video_id).or_insert(0);
        *failures += 1;
        if gives_up(*failures) {
            backoff.register_failures.remove(&video_id);
            warn!(
                youtube_id,
                "register check: came to nothing {MAX_FAILED_CHECKS} times - left unchecked"
            );
            let _ = stamp_register(&self.pool, video_id, REGISTER_CHECK_VERSION, None).await;
        }
    }

    /// A node that checks nothing itself: stamp every stale song whose track
    /// already carries the current check (a peer's copy), at most once per
    /// [`PEER_SCAN_EVERY`]. Asks no model and holds nothing.
    pub(crate) async fn stamp_peer_checks(&self) {
        {
            let mut backoff = self.retry_backoff.lock().await;
            let now = Instant::now();
            if !scan_due(backoff.register_scan_at, now) {
                return;
            }
            backoff.register_scan_at = Some(now);
        }
        let Ok(rows) = fetch_stale_registers(&self.pool, REGISTER_CHECK_VERSION).await else {
            return;
        };
        for (video_id, youtube_id) in rows {
            let path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
            if let Some(track) = read_track(&path).await
                && checked_now(&track)
            {
                self.stamp_register_of(video_id, &track).await;
            }
        }
    }
}

/// How often a node that checks nothing itself stamps its peers' checks.
pub(crate) const PEER_SCAN_EVERY: Duration = Duration::from_secs(600);

/// Is a peer-check scan due at `now`, the last one made at `last`?
pub(crate) fn scan_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= PEER_SCAN_EVERY)
}

/// Failed checks of one song (answers it could not read) before the pass
/// leaves it unchecked and moves on.
pub(crate) const MAX_FAILED_CHECKS: u32 = 3;

/// Has a song failed its check often enough to be left unchecked?
pub(crate) fn gives_up(failures: u32) -> bool {
    failures >= MAX_FAILED_CHECKS
}

/// The shared translation backoff after a failed (or refused) call: 1, 2,
/// 4, 8, 10 min.
#[cfg_attr(test, mutants::skip)] // the translation passes' own schedule
fn back_off(backoff: &mut crate::lyrics::worker::RetryBackoff) {
    backoff.consecutive_failures = backoff.consecutive_failures.saturating_add(1);
    let attempt = backoff.consecutive_failures.saturating_sub(1).min(4);
    let secs = 60u64.saturating_mul(1u64 << attempt).min(600);
    backoff.silent_until = Some(Instant::now() + Duration::from_secs(secs));
}

/// The track at `path`, or `None` when it cannot be read.
async fn read_track(path: &std::path::Path) -> Option<LyricsTrack> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
#[path = "worker_register_tests.rs"]
mod tests;
