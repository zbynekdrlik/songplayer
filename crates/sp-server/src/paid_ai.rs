//! #229 item C — the owner's ruling (8.10.2026), verbatim: "Celkovo v pp by
//! nemalo dochadzat k ziadnemu platenemu ai spracovaniu dokial to
//! nepovolim". The node's switch `paid_ai_enabled`
//! (`sp_core::config::paid_ai_enabled`: ON when unset, so a node that never
//! set it — SNV — is unchanged) is the ONE gate of every paid AI call this
//! node makes, read live from the database at each call ([`enabled`]):
//!
//! - the lyrics job (Gemini 3.5 Transcribe, Claude's clean-up, the Spotify
//!   resolution, the translation): `peer::Exchange::may_run_here`, asked at
//!   every "run here" of the exchange before the run's own records — while
//!   off the job still takes a peer's copy, else it is held;
//! - the two translation passes (Claude): held;
//! - the metadata chain (Claude, Gemini): `metadata::ProviderChain::providers`
//!   answers none — a download names the song by the title parser (free),
//!   marked for the repair; the repair takes a peer's title only; the probe
//!   asks no provider;
//! - the dub (Gemini Live-Translate): the dub worker holds its job;
//! - every Gemini key read: [`gemini_keys`] (none while off) — the lyrics
//!   tiers, the dub worker, the g35t probe.
//!
//! A new paid provider is gated here too. Held work counts no attempt and
//! logs ONE INFO per kind and song ([`hold`]), then DEBUG — never a WARN.
//! `GET /api/v1/status` names the switch and what it holds ([`status`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sp_core::config::SETTING_PAID_AI_ENABLED;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

/// How long held work waits before it is picked again: a peer's copy may
/// have come meanwhile, or the switch is on again.
pub const HELD_RECHECK: Duration = Duration::from_secs(30 * 60);

/// What a probe answers while paid AI is off: nothing was sent.
pub const OFF_REASON: &str =
    "paid AI is off on this node (paid_ai_enabled = false): nothing was sent";

/// The kinds of work that wait while paid AI is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Held {
    Lyrics,
    Translation,
    Metadata,
    Dub,
}

impl Held {
    /// The kind's stable name (`paid_ai_held`, the log's `job`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lyrics => "lyrics",
            Self::Translation => "translation",
            Self::Metadata => "metadata",
            Self::Dub => "dub",
        }
    }
}

/// Whether this node may call paid AI now (module doc). A switch that cannot
/// be read reads as off (WARN): the owner's money comes first.
pub async fn enabled(pool: &SqlitePool) -> bool {
    match crate::db::models::get_setting(pool, SETTING_PAID_AI_ENABLED).await {
        Ok(raw) => sp_core::config::paid_ai_enabled(raw.as_deref()),
        Err(e) => {
            warn!(%e, "paid AI: reading the switch failed - no paid AI call now");
            false
        }
    }
}

/// The `gemini_api_key` list for a paid call (split by the shared
/// `gemini_api::gemini_keys_from_setting`), or `None` while paid AI is off.
/// A key setting that cannot be read is no key (WARN), as before.
pub async fn gemini_keys(pool: &SqlitePool) -> Option<Vec<String>> {
    if !enabled(pool).await {
        return None;
    }
    let csv = crate::db::models::get_setting(pool, "gemini_api_key")
        .await
        .inspect_err(|e| warn!(%e, "paid AI: reading gemini_api_key failed"))
        .ok()
        .flatten()
        .unwrap_or_default();
    Some(crate::gemini_api::gemini_keys_from_setting(&csv))
}

/// The work held since the process started, per kind: the songs' YouTube
/// ids ("" for a whole pass). Never cleared: a song held again after the
/// switch was on in between logs at DEBUG (its first hold had the INFO).
static HOLDS: Mutex<BTreeMap<Held, BTreeSet<String>>> = Mutex::new(BTreeMap::new());

/// `what` of `key` (a song's YouTube id; "" for a whole pass) waits while
/// paid AI is off: ONE INFO the first time, then DEBUG.
pub fn hold(what: Held, key: &str) {
    let first = HOLDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(what)
        .or_default()
        .insert(key.to_string());
    if first {
        info!(
            job = what.as_str(),
            key,
            "paid AI is off on this node (paid_ai_enabled) - held: only a peer's copy is taken"
        );
    } else {
        debug!(job = what.as_str(), key, "paid AI is off - still held");
    }
}

/// The kinds holding work, in [`Held`] order.
fn held_kinds() -> Vec<String> {
    HOLDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .keys()
        .map(|kind| kind.as_str().to_string())
        .collect()
}

/// The switch as `GET /api/v1/status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidAiStatus {
    pub enabled: bool,
    /// The kinds holding work while off ([`Held::as_str`]); empty while on.
    pub held: Vec<String>,
}

/// The switch now and, while it is off, the kinds of work it holds.
pub async fn status(pool: &SqlitePool) -> PaidAiStatus {
    let enabled = enabled(pool).await;
    let held = if enabled { Vec::new() } else { held_kinds() };
    PaidAiStatus { enabled, held }
}

/// A settings PATCH value of `key`: `paid_ai_enabled` takes `true`, `false`
/// (trimmed, any case; stored lowercase) or `""` (unset: on), anything else
/// refuses the PATCH; every other setting passes as sent.
pub fn checked(key: &str, value: &str) -> Result<String, String> {
    if key != SETTING_PAID_AI_ENABLED {
        return Ok(value.to_string());
    }
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "true" | "false" => Ok(v),
        _ => Err(format!("{SETTING_PAID_AI_ENABLED} must be true or false")),
    }
}

#[cfg(test)]
#[path = "paid_ai_tests.rs"]
mod tests;
