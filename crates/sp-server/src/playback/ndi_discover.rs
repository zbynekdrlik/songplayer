//! #305 item 3: is `SP-program` DISCOVERABLE on the network? A liveness
//! signal, not a fix (the owner's #60 ruling: no sender re-create, no
//! runtime re-init).
//!
//! On resolume-pp (11.11.2026) the sender, its data port and its frame
//! counters stayed alive while its advertisement was gone: no receiver could
//! find `RESOLUME-PP (SP-program)`, and nothing in SongPlayer could tell. Now
//! one background task asks ONE long-lived NDI source finder
//! (`sp_ndi::RealNdiSourceFinder`, created once, never per poll) every
//! [`DISCOVER_POLL`] whether `<COMPUTERNAME> (SP-program)` is listed
//! ([`expected_name`], CLAUDE.md's `"MACHINE (stream)"` format, compared
//! ASCII-case-insensitively).
//!
//! The decisions are pure ([`Discovery::observe`], [`sighting`]):
//!
//! - `starting` — from the task's start; nothing is decided while the
//!   source is not seen inside [`DISCOVER_GRACE_MS`];
//! - `discoverable` — listed at the last read (a listing on a Dante subnet
//!   counts, reported as `on_dante`: the local finder on resolume-snv resolves
//!   `SP-program` to the Dante leg, the accepted case of ROZHODNUTÉ
//!   6806263675; a non-Dante entry is preferred when several match);
//! - `not_discoverable` — not listed for [`DISCOVER_LOST_READS`] reads in a
//!   row, past the grace: `degraded_reason` [`NOT_DISCOVERABLE_REASON`] on
//!   `GET /api/v1/program`, one WARN on the loss, one INFO on the return;
//! - `unavailable` — no NDI SDK, no finder, or no `COMPUTERNAME`;
//! - `stale` — no read for [`DISCOVER_STALE_MS`] (a stuck SDK read, a dead
//!   task): the last verdict is no reading any more.
//!
//! The grace and the staleness run on the box's monotonic clock ([`At`]),
//! so a date step moves neither.
//!
//! Served as `ndi.discoverable` on `GET /api/v1/program` (`NdiFeed::discover`).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use sp_ndi::{FoundSource, NdiSourceFinder};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::dante_guard::{Cidr, refuses};
use crate::playback::program_bus::PROGRAM_NDI_NAME;

/// How often the finder is read.
pub const DISCOVER_POLL: Duration = Duration::from_secs(10);
/// How long one read waits inside the SDK for a change of the list.
pub const DISCOVER_WAIT_MS: u32 = 1000;
/// From the task's start, a source not seen yet is not judged.
pub const DISCOVER_GRACE_MS: i64 = 30_000;
/// Reads in a row without the source that make it not discoverable.
pub const DISCOVER_LOST_READS: u32 = 3;
/// No read for this long (a stuck read, a dead task) = `stale`: the last
/// verdict is no longer a reading.
pub const DISCOVER_STALE_MS: i64 = 35_000;
/// `GET /api/v1/program` → `degraded_reason` while not discoverable.
pub const NOT_DISCOVERABLE_REASON: &str = "SP-program not discoverable on the network";

/// The state of `SP-program`'s discoverability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverState {
    #[default]
    Starting,
    Discoverable,
    NotDiscoverable,
    Unavailable,
    /// No read for [`DISCOVER_STALE_MS`]: the check itself is stuck.
    Stale,
}

/// `GET /api/v1/program` → `ndi.discoverable`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DiscoverStatus {
    pub state: DiscoverState,
    /// UTC ms the state was entered (`null` before the task started).
    pub since_ms: Option<i64>,
    /// UTC ms of the last read that listed the source.
    pub seen_at_ms: Option<i64>,
    /// UTC ms of the last read.
    pub read_at_ms: Option<i64>,
    /// The `host:port` it was last listed at.
    pub address: Option<String>,
    /// That address lies in a Dante subnet (no non-Dante entry was listed).
    pub on_dante: bool,
    /// Reads made, and reads in a row without the source.
    pub reads: u64,
    pub misses: u32,
    /// The name looked for, `"<COMPUTERNAME> (SP-program)"`.
    pub expected: Option<String>,
    /// Why the check is `unavailable`.
    pub reason: Option<String>,
}

/// One listing of the expected source ([`sighting`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    pub address: String,
    pub on_dante: bool,
}

/// An instant on both clocks: `mono_ms` (the box's monotonic clock) times
/// the grace and the staleness, so a date step moves neither; `utc_ms` is
/// what the status shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    pub mono_ms: i64,
    pub utc_ms: i64,
}

impl At {
    /// Now, on both clocks.
    #[cfg_attr(test, mutants::skip)] // clock reads; the decisions take injected instants
    pub fn now() -> At {
        At {
            mono_ms: crate::audio_host::mono::mono_100ns() / 10_000,
            utc_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// What a read is logged as ([`Discovery::observe`] decides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoverLog {
    /// Seen for the first time since the start (an INFO).
    FirstSeen,
    /// Not listed past the grace (a WARN, once per loss).
    Lost,
    /// Listed again after a loss (an INFO).
    Back,
    Nothing,
}

/// The name `SP-program` is listed under on a machine named `host`; `None`
/// for a missing or blank name.
pub fn expected_name(host: Option<&str>) -> Option<String> {
    let host = host?.trim();
    (!host.is_empty()).then(|| format!("{host} ({PROGRAM_NDI_NAME})"))
}

/// Whether a finder's `host:port` (`[v6]:port` too) lies in one of the
/// Dante subnets.
pub fn on_dante(url: &str, dante: &[Cidr]) -> bool {
    let host = url.rsplit_once(':').map_or(url, |(h, _)| h).trim();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse().is_ok_and(|ip| refuses(dante, ip))
}

/// The listing of `expected` among `sources`: a non-Dante entry first,
/// else the first Dante one; `None` when it is not listed. (The SDK lists
/// one entry per name, so several matches are not expected; the choice is
/// defensive.)
pub fn sighting(sources: &[FoundSource], expected: &str, dante: &[Cidr]) -> Option<Sighting> {
    let mut found = sources
        .iter()
        .filter(|s| s.name.trim().eq_ignore_ascii_case(expected))
        .map(|s| Sighting {
            address: s.url.clone(),
            on_dante: on_dante(&s.url, dante),
        });
    let first = found.next()?;
    if !first.on_dante {
        return Some(first);
    }
    Some(found.find(|s| !s.on_dante).unwrap_or(first))
}

/// The state machine (see the module doc). `Default` = `starting`, not begun.
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    status: DiscoverStatus,
    /// Monotonic ms the task began; `None` = no read is judged yet.
    started_mono: Option<i64>,
    /// Monotonic ms of the last read.
    read_mono: Option<i64>,
}

impl Discovery {
    /// The task began `at`, looking for `expected`.
    pub fn begin(&mut self, expected: String, at: At) {
        self.status = DiscoverStatus {
            since_ms: Some(at.utc_ms),
            expected: Some(expected),
            ..DiscoverStatus::default()
        };
        self.started_mono = Some(at.mono_ms);
        self.read_mono = None;
    }

    /// The check cannot run (`reason`).
    pub fn unavailable(&mut self, reason: &str, at: At) {
        self.status = DiscoverStatus {
            state: DiscoverState::Unavailable,
            since_ms: Some(at.utc_ms),
            reason: Some(reason.to_string()),
            ..DiscoverStatus::default()
        };
        self.started_mono = None;
    }

    /// One read `at`: the source `seen` or not. Nothing before
    /// [`Self::begin`].
    pub fn observe(&mut self, seen: Option<Sighting>, at: At) -> DiscoverLog {
        let Some(started) = self.started_mono else {
            return DiscoverLog::Nothing;
        };
        self.read_mono = Some(at.mono_ms);
        let now_ms = at.utc_ms;
        let s = &mut self.status;
        s.reads += 1;
        s.read_at_ms = Some(now_ms);
        let before = s.state;
        let Some(seen) = seen else {
            s.misses += 1;
            let lost = s.misses >= DISCOVER_LOST_READS && at.mono_ms - started >= DISCOVER_GRACE_MS;
            if !lost || before == DiscoverState::NotDiscoverable {
                return DiscoverLog::Nothing;
            }
            s.state = DiscoverState::NotDiscoverable;
            s.since_ms = Some(now_ms);
            return DiscoverLog::Lost;
        };
        s.misses = 0;
        s.seen_at_ms = Some(now_ms);
        s.address = Some(seen.address);
        s.on_dante = seen.on_dante;
        if before == DiscoverState::Discoverable {
            return DiscoverLog::Nothing;
        }
        s.state = DiscoverState::Discoverable;
        s.since_ms = Some(now_ms);
        if before == DiscoverState::NotDiscoverable {
            DiscoverLog::Back
        } else {
            DiscoverLog::FirstSeen
        }
    }

    /// The status as read at monotonic `mono_ms`: `stale` once a begun
    /// check made no read for [`DISCOVER_STALE_MS`] (counted from the start
    /// before its first read).
    pub fn status_at(&self, mono_ms: i64) -> DiscoverStatus {
        let mut status = self.status.clone();
        let last = self.read_mono.or(self.started_mono);
        if self.started_mono.is_some() && last.is_some_and(|t| mono_ms - t > DISCOVER_STALE_MS) {
            status.state = DiscoverState::Stale;
        }
        status
    }

    pub fn expected(&self) -> Option<String> {
        self.status.expected.clone()
    }
}

impl DiscoverStatus {
    /// [`NOT_DISCOVERABLE_REASON`] while not discoverable.
    pub fn degraded_reason(&self) -> Option<&'static str> {
        (self.state == DiscoverState::NotDiscoverable).then_some(NOT_DISCOVERABLE_REASON)
    }
}

/// The shared [`Discovery`] (`NdiFeed::discover`): the task writes, the API
/// reads.
#[derive(Default)]
pub struct NdiDiscover {
    inner: Mutex<Discovery>,
}

impl NdiDiscover {
    fn lock(&self) -> MutexGuard<'_, Discovery> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn begin(&self, expected: String, at: At) {
        self.lock().begin(expected, at);
    }

    pub fn unavailable(&self, reason: &str, at: At) {
        self.lock().unavailable(reason, at);
    }

    /// The status at monotonic `mono_ms` ([`Discovery::status_at`]).
    pub fn status_at(&self, mono_ms: i64) -> DiscoverStatus {
        self.lock().status_at(mono_ms)
    }

    /// The status now (the API's read).
    #[cfg_attr(test, mutants::skip)] // a clock read around `status_at`
    pub fn status(&self) -> DiscoverStatus {
        self.status_at(At::now().mono_ms)
    }

    /// One read of `finder`, judged at `at()` taken after it (blocking up to
    /// [`DISCOVER_WAIT_MS`] inside the SDK, so it runs on the blocking
    /// pool); `dante` = the Dante subnets. Returns what to log with the
    /// status after the read.
    pub fn poll(
        &self,
        finder: &dyn NdiSourceFinder,
        dante: &[Cidr],
        at: impl FnOnce() -> At,
    ) -> (DiscoverLog, DiscoverStatus) {
        let Some(expected) = self.lock().expected() else {
            return (DiscoverLog::Nothing, self.lock().status.clone());
        };
        let sources = finder.current_sources(DISCOVER_WAIT_MS);
        let seen = sighting(&sources, &expected, dante);
        let at = at();
        let mut d = self.lock();
        let log = d.observe(seen, at);
        (log, d.status_at(at.mono_ms))
    }
}

/// The log line of a read ([`Discovery::observe`] decides). Logging only.
#[cfg_attr(test, mutants::skip)]
fn log_read(log: DiscoverLog, status: &DiscoverStatus) {
    let expected = status.expected.as_deref().unwrap_or("");
    let address = status.address.as_deref().unwrap_or("");
    match log {
        DiscoverLog::FirstSeen => info!(
            expected,
            address,
            on_dante = status.on_dante,
            "ndi discover: SP-program is discoverable on the network"
        ),
        DiscoverLog::Back => info!(
            expected,
            address,
            on_dante = status.on_dante,
            "ndi discover: SP-program is discoverable again"
        ),
        DiscoverLog::Lost => warn!(
            expected,
            misses = status.misses,
            last_seen_ms = ?status.seen_at_ms,
            last_address = address,
            "ndi discover: {NOT_DISCOVERABLE_REASON} — receivers cannot find it (#305)"
        ),
        DiscoverLog::Nothing => {}
    }
}

/// Start the check: the finder built ONCE from the engine's NDI SDK (none
/// off Windows / without the SDK = `unavailable`), then one read every
/// [`DISCOVER_POLL`] until shutdown.
#[cfg_attr(test, mutants::skip)]
pub fn start(
    lib: Option<Arc<sp_ndi::NdiLib>>,
    discover: Arc<NdiDiscover>,
    shutdown: &broadcast::Sender<()>,
) {
    let finder = lib
        .and_then(sp_ndi::RealNdiSourceFinder::new)
        .map(|f| Arc::new(f) as Arc<dyn NdiSourceFinder>);
    let host = std::env::var("COMPUTERNAME").ok();
    tokio::spawn(run(finder, host, discover, shutdown.subscribe()));
}

/// The task: `unavailable` without a finder or a machine name, else a read
/// every [`DISCOVER_POLL`].
#[cfg_attr(test, mutants::skip)]
async fn run(
    finder: Option<Arc<dyn NdiSourceFinder>>,
    host: Option<String>,
    discover: Arc<NdiDiscover>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let Some(finder) = finder else {
        warn!(
            "ndi discover: no NDI source finder (no NDI SDK) — SP-program's discoverability is not checked"
        );
        discover.unavailable("no NDI source finder (no NDI SDK)", At::now());
        return;
    };
    let Some(expected) = expected_name(host.as_deref()) else {
        warn!(
            "ndi discover: COMPUTERNAME is not set — SP-program's discoverability is not checked"
        );
        discover.unavailable("COMPUTERNAME is not set", At::now());
        return;
    };
    info!(expected = %expected, "ndi discover: checking SP-program's discoverability every 10 s");
    discover.begin(expected, At::now());
    loop {
        let (f, d) = (finder.clone(), discover.clone());
        let dante = crate::dante_guard::global().subnets();
        let read = tokio::task::spawn_blocking(move || d.poll(&*f, &dante, At::now)).await;
        match read {
            Ok((log, status)) => log_read(log, &status),
            Err(e) => warn!(%e, "ndi discover: a read failed"),
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(DISCOVER_POLL) => {}
        }
    }
    info!("ndi discover: stopped");
}

#[cfg(test)]
#[path = "ndi_discover_tests.rs"]
mod tests;
