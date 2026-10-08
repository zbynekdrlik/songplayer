//! #229 item C — the owner's ruling (8.10.2026), verbatim: "Celkovo v pp by
//! nemalo dochadzat k ziadnemu platenemu ai spracovaniu dokial to
//! nepovolim". The node's switch `paid_ai_enabled`
//! (`sp_core::config::paid_ai_enabled`: ON when unset, so a node that never
//! set it — SNV — is unchanged) is the ONE switch every paid AI call this
//! node makes asks at its call site, read live from the database at each
//! call ([`enabled`]); there is no transport-level backstop, so a new call
//! site asks it too (a test per path pins each one):
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
//!   tiers, the dub worker, the g35t probe;
//! - the lyrics source probe (`POST /api/v1/lyrics/probe-sources`): its
//!   description probe asks Claude only while on.
//!
//! A new paid provider is gated here too. Held work counts no attempt and
//! logs ONE INFO per kind and song ([`hold`]), then DEBUG — never a WARN.
//! `GET /api/v1/status` names the switch and the kinds holding work now
//! ([`status`]: a hold under 40 min old).

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sp_core::config::SETTING_PAID_AI_ENABLED;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

/// How long held work waits before it is picked again (30 min): a peer's
/// copy may have come meanwhile, or the switch is on again. A literal: a
/// `30 * 60` would list two mutants nothing could tell apart.
pub const HELD_RECHECK: Duration = Duration::from_secs(1_800);

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

/// How long a held kind shows on the status after its last hold: held work
/// is picked again within [`HELD_RECHECK`] (a dub job at its worker's next
/// tick), so a kind still holding work is held again within it. A
/// translation is held again at the next translation pass, which runs only
/// while the lyrics queue is idle; with many lyrics held the queue stays
/// busy, so a translation can drop out of the status while it still waits
/// (review round 15; display only).
const HELD_SHOWN: Duration = Duration::from_secs(2_400);

/// The work held, per kind: the songs' YouTube ids and when each was last
/// held. Kept for the process' life: a song held again after the switch
/// was on in between logs at DEBUG (its first hold had the INFO).
static HOLDS: Mutex<BTreeMap<Held, BTreeMap<String, Instant>>> = Mutex::new(BTreeMap::new());

/// `what` of `key` (a song's YouTube id) waits while paid AI is off: ONE
/// INFO the first time, then DEBUG.
pub fn hold(what: Held, key: &str) {
    let first = HOLDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(what)
        .or_default()
        .insert(key.to_string(), Instant::now())
        .is_none();
    if first {
        info!(
            job = what.as_str(),
            key,
            "paid AI is off on this node (paid_ai_enabled) - held: no paid AI call until it is on"
        );
    } else {
        debug!(job = what.as_str(), key, "paid AI is off - still held");
    }
}

/// Whether a hold made `held_at` still shows at `now` ([`HELD_SHOWN`]).
fn shown(held_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(held_at) <= HELD_SHOWN
}

/// The kinds holding work at `now` (held within [`HELD_SHOWN`]), in
/// [`Held`] order.
fn held_kinds(now: Instant) -> Vec<String> {
    HOLDS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(_, keys)| keys.values().any(|at| shown(*at, now)))
        .map(|(kind, _)| kind.as_str().to_string())
        .collect()
}

/// The switch as `GET /api/v1/status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaidAiStatus {
    pub enabled: bool,
    /// The kinds holding work while off ([`Held::as_str`]: held within the
    /// last 40 min); empty while on.
    pub held: Vec<String>,
}

/// The switch now and, while it is off, the kinds of work it holds.
pub async fn status(pool: &SqlitePool) -> PaidAiStatus {
    let enabled = enabled(pool).await;
    let held = if enabled {
        Vec::new()
    } else {
        held_kinds(Instant::now())
    };
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
