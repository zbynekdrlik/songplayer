//! #250: the lyrics worker's register check (`register_check.rs`), split
//! from `worker.rs` (1000-line cap). ONE check per track, made while it is
//! processed — right after its translation, before it is persisted — and once
//! for every served track checked under an older version (or never), by the
//! lowest-priority stale pass. Nothing is checked at display.

use std::time::{Duration, Instant};

use sp_core::lyrics::{LyricsTrack, RegisterCheck, RegisterVerdict};
use tracing::{debug, info, warn};

use crate::db::models_register::{fetch_next_stale_register, stamp_register};
use crate::lyrics::register_check::{self, REGISTER_CHECK_VERSION};
use crate::lyrics::worker::LyricsWorker;

/// What happened to a track's register check this pass.
#[derive(Debug, PartialEq)]
pub(crate) enum CheckOutcome {
    /// Checked (or refused): the check is in the track.
    Checked,
    /// No AI client, or this node's paid AI is off: nothing asked.
    NotAsked,
    /// The call failed for another reason: asked again later.
    Failed,
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
                CheckOutcome::Failed
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
    /// unreachable Claude is not asked every tick.
    #[cfg_attr(test, mutants::skip)] // the pass glue; its paths are tested end to end
    pub(crate) async fn recheck_next_stale_register(&self) {
        if self.ai_client.is_none() {
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
        let track = tokio::fs::read_to_string(&path)
            .await
            .ok()
            .and_then(|text| serde_json::from_str::<LyricsTrack>(&text).ok());
        let Some(mut track) = track else {
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
                    warn!(youtube_id, "register check: write failed: {e}");
                    return;
                }
                self.stamp_register_of(video_id, &track).await;
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = 0;
                backoff.silent_until = None;
            }
            CheckOutcome::NotAsked => {}
            CheckOutcome::Failed => {
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = backoff.consecutive_failures.saturating_add(1);
                let attempt = backoff.consecutive_failures.saturating_sub(1).min(4);
                let secs = 60u64.saturating_mul(1u64 << attempt).min(600);
                backoff.silent_until = Some(Instant::now() + Duration::from_secs(secs));
            }
        }
    }
}

#[cfg(test)]
#[path = "worker_register_tests.rs"]
mod tests;
