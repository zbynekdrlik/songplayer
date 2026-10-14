//! #233 (PP's SoundGrid pops, 11.10.2026): the production-environment guard.
//!
//! Proven on PP (A/B/A with a kernel trace): Windows Defender's real-time
//! scan of the processes SongPlayer launches (yt-dlp, deno) made the Waves
//! SoundGrid driver resync, an audible pop each time. The owner's rule: a
//! production PC runs Windows LTSC or Windows IoT Enterprise, with Defender's
//! real-time protection and every firewall profile OFF, and no startup check
//! or test may let a box run otherwise.
//!
//! This module turns the facts read from the box ([`EnvFacts`]: the raw
//! reads of `prod_env_win.rs`, on Windows with no process spawned, made into
//! facts by `prod_env_read.rs`; only the WMI reads are bounded) into a verdict
//! ([`verdict`]), publishes it ([`EnvStore`], `GET /api/v1/status.environment`
//! and `GET /api/v1/environment`) and logs it: one ERROR when a problem
//! appears or changes, one INFO when it clears ([`Reporter`]). [`run`] checks
//! at the start and every minute. An unreadable fact is a problem ("cannot
//! verify"), never fine. Off Windows nothing is checked: the status says so
//! (`note`) and holds. The vocabulary (codes, Slovak texts, the wire shape)
//! is `sp_core::prod_env`.
//!
//! #255 adds the production PC's audio profile ([`audio`]): the adapters the
//! SoundGrid Protocol is bound to on an NDIS driver with Waves' settings,
//! Windows Update's automatic updates off, MMCSS set for Pro Audio; the
//! Dante adapter, the driver exclusion and power are reported only.

use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use sp_core::prod_env::{
    EnvProblem, EnvironmentStatus, FirewallProfile, FirewallState, Unreadable,
};
use tracing::{error, info, warn};

/// How often the box is checked again.
pub const CHECK_EVERY: Duration = Duration::from_secs(60);

/// How long the WMI reads may take before they count as unreadable (a hung
/// WMI never freezes the published status; the registry is read directly).
pub const READ_BOUND: Duration = Duration::from_secs(30);

/// The LTSC editions (`EditionID`): Enterprise LTSC, its N edition, IoT
/// Enterprise LTSC and its subscription edition. An evaluation edition
/// (`EnterpriseSEval`) is no production edition.
pub const LTSC_EDITIONS: [&str; 4] = [
    "EnterpriseS",
    "EnterpriseSN",
    "IoTEnterpriseS",
    "IoTEnterpriseSK",
];

/// Windows IoT Enterprise and its subscription edition, allowed next to LTSC
/// (the owner's ruling, 11.10.2026: "11 IoT Enterprise") from
/// [`WINDOWS_11_BUILD`] on.
pub const IOT_ENTERPRISE_EDITIONS: [&str; 2] = ["IoTEnterprise", "IoTEnterpriseK"];

/// The first Windows 11 build: an IoT Enterprise edition below it is
/// Windows 10 IoT Enterprise, which is not allowed (an LTSC edition is, at
/// any build).
pub const WINDOWS_11_BUILD: u32 = 22_000;

/// The status note off Windows.
pub const NOT_WINDOWS: &str = "not Windows: the production environment is checked on Windows only";

/// WMI's "invalid namespace" (0x8004100E): Defender's namespace is absent.
pub const WBEM_E_INVALID_NAMESPACE: i32 = -2_147_217_394;

/// WMI's "invalid class" (0x80041010): Defender's status class is absent.
pub const WBEM_E_INVALID_CLASS: i32 = -2_147_217_392;

/// What was read from the box. `None` = could not be read (why: `read_errors`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvFacts {
    /// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` `EditionID`.
    pub edition_id: Option<String>,
    /// The same key's `ProductName` (shown only).
    pub product_name: Option<String>,
    /// The same key's `CurrentBuildNumber`.
    pub build_number: Option<u32>,
    /// Defender's real-time protection (WMI `MSFT_MpComputerStatus`); off
    /// when Defender is not installed or its service is stopped.
    pub defender_realtime: Option<bool>,
    /// Defender runs in a passive / EDR block mode (it still scans).
    pub defender_passive: Option<bool>,
    /// Defender's running mode (`AMRunningMode`, shown only).
    pub defender_mode: Option<String>,
    /// Defender's antivirus engine is on (`AntivirusEnabled`, shown only).
    pub defender_antivirus_enabled: Option<bool>,
    /// The other antivirus products that are on (`read::antivirus_on`).
    pub antivirus_on: Option<Vec<String>>,
    /// The domain, private and public firewall profile (the active policy
    /// store, `read::active_firewall`; the registry when it cannot be read).
    pub firewall: [Option<bool>; 3],
    /// #255: the audio profile (`audio::read::read_audio`).
    pub audio: audio::AudioFacts,
    /// Why a fact could not be read.
    pub read_errors: Vec<String>,
}

impl EnvFacts {
    /// Nothing could be read, for `why`.
    pub fn unread(why: impl Into<String>) -> Self {
        Self {
            read_errors: vec![why.into()],
            ..Self::default()
        }
    }
}

/// The verdict on the facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVerdict {
    /// No problem.
    pub ok: bool,
    /// The edition is an LTSC edition (`None`: not read).
    pub ltsc: Option<bool>,
    /// The edition is allowed (`None`: it, or the build an IoT Enterprise
    /// edition needs, was not read).
    pub edition_allowed: Option<bool>,
    /// Every problem: the edition, Defender, the other antivirus products,
    /// each firewall profile, then the audio profile's (`audio::problems`).
    pub problems: Vec<EnvProblem>,
}

/// An LTSC edition ([`LTSC_EDITIONS`], ASCII case ignored).
pub fn is_ltsc(edition_id: &str) -> bool {
    let e = edition_id.trim();
    LTSC_EDITIONS.iter().any(|l| l.eq_ignore_ascii_case(e))
}

/// An IoT Enterprise edition that is no LTSC ([`IOT_ENTERPRISE_EDITIONS`],
/// ASCII case ignored).
pub fn is_iot_enterprise(edition_id: &str) -> bool {
    let e = edition_id.trim();
    IOT_ENTERPRISE_EDITIONS
        .iter()
        .any(|i| i.eq_ignore_ascii_case(e))
}

/// Whether an edition is allowed in production: an LTSC edition at any
/// build, an IoT Enterprise edition on Windows 11 (`build` from
/// [`WINDOWS_11_BUILD`]), nothing else. `None`: an IoT Enterprise edition
/// whose build was not read.
pub fn edition_allowed(edition_id: &str, build: Option<u32>) -> Option<bool> {
    if is_ltsc(edition_id) {
        return Some(true);
    }
    if !is_iot_enterprise(edition_id) {
        return Some(false);
    }
    build.map(|b| b >= WINDOWS_11_BUILD)
}

/// The verdict: every fact that is wrong, or cannot be read, is a problem.
pub fn verdict(facts: &EnvFacts) -> EnvVerdict {
    let mut problems = Vec::new();
    let edition = facts
        .edition_id
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let allowed = edition.and_then(|e| edition_allowed(e, facts.build_number));
    match (edition, allowed) {
        (None, _) => problems.push(EnvProblem::Unreadable(Unreadable::Edition)),
        (Some(_), None) => problems.push(EnvProblem::Unreadable(Unreadable::Build)),
        (Some(e), Some(false)) => problems.push(EnvProblem::EditionNotAllowed {
            edition: e.to_string(),
            build: facts.build_number,
        }),
        (Some(_), Some(true)) => {}
    }
    match (facts.defender_realtime, facts.defender_passive) {
        (Some(realtime), Some(passive)) => {
            if realtime {
                problems.push(EnvProblem::DefenderRealtimeOn);
            }
            if passive {
                let mode = facts.defender_mode.clone().unwrap_or_default();
                problems.push(EnvProblem::DefenderPassive(mode));
            }
        }
        _ => problems.push(EnvProblem::Unreadable(Unreadable::Defender)),
    }
    match &facts.antivirus_on {
        None => problems.push(EnvProblem::Unreadable(Unreadable::Antivirus)),
        Some(names) => {
            for name in names {
                problems.push(EnvProblem::AntivirusOn(name.clone()));
            }
        }
    }
    for (profile, on) in FirewallProfile::ALL.into_iter().zip(facts.firewall) {
        match on {
            None => problems.push(EnvProblem::Unreadable(Unreadable::Firewall(profile))),
            Some(true) => problems.push(EnvProblem::FirewallOn(profile)),
            Some(false) => {}
        }
    }
    problems.extend(audio::problems(&facts.audio));
    EnvVerdict {
        ok: problems.is_empty(),
        ltsc: edition.map(is_ltsc),
        edition_allowed: allowed,
        problems,
    }
}

/// One check's input.
// #255: `EnvFacts` carries the audio profile (~500 bytes); one value a
// minute, never on a hot path, so boxing it would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Off Windows: nothing to check.
    NotWindows,
    /// The facts read on Windows.
    Read(EnvFacts),
}

/// The published status of one check, and its problems.
pub fn status_of(check: &Check, now_ms: i64) -> (EnvironmentStatus, Vec<EnvProblem>) {
    let facts = match check {
        Check::NotWindows => {
            let status = EnvironmentStatus {
                checked: true,
                ok: true,
                note: Some(NOT_WINDOWS.to_string()),
                checked_at_ms: Some(now_ms),
                ..EnvironmentStatus::default()
            };
            return (status, Vec::new());
        }
        Check::Read(facts) => facts,
    };
    let v = verdict(facts);
    let [domain, private, public] = facts.firewall;
    let status = EnvironmentStatus {
        checked: true,
        ok: v.ok,
        note: None,
        edition_id: facts.edition_id.clone(),
        product_name: facts.product_name.clone(),
        build_number: facts.build_number,
        ltsc: v.ltsc,
        edition_allowed: v.edition_allowed,
        defender_realtime: facts.defender_realtime,
        defender_passive: facts.defender_passive,
        defender_mode: facts.defender_mode.clone(),
        defender_antivirus_enabled: facts.defender_antivirus_enabled,
        antivirus_on: facts.antivirus_on.clone(),
        firewall: FirewallState {
            domain,
            private,
            public,
        },
        audio: audio::view(&facts.audio),
        problems: v.problems.iter().map(EnvProblem::view).collect(),
        read_errors: facts.read_errors.clone(),
        checked_at_ms: Some(now_ms),
    };
    (status, v.problems)
}

/// What a check logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogStep {
    /// The same problems as the last check (or none again): nothing.
    Quiet,
    /// The first check, and the box holds: one INFO.
    Holds,
    /// A problem appeared or changed: one ERROR naming every problem.
    Problems,
    /// The last problem cleared: one INFO.
    Cleared,
    /// The same problems, but a reason a fact could not be read changed (or
    /// a new one appeared, or one went): one WARN naming them.
    ReadErrors,
}

/// Remembers the last check's problems and read errors, so each change is
/// logged once.
#[derive(Debug, Default)]
pub struct Reporter {
    last: Option<(Vec<EnvProblem>, Vec<String>)>,
}

impl Reporter {
    /// What this check logs, given the last one.
    pub fn step(&mut self, problems: &[EnvProblem], read_errors: &[String]) -> LogStep {
        let now = (problems.to_vec(), read_errors.to_vec());
        let Some((last_problems, last_errors)) = self.last.replace(now) else {
            return if problems.is_empty() {
                LogStep::Holds
            } else {
                LogStep::Problems
            };
        };
        if last_problems.as_slice() != problems {
            return if problems.is_empty() {
                LogStep::Cleared
            } else {
                LogStep::Problems
            };
        }
        if last_errors.as_slice() != read_errors {
            LogStep::ReadErrors
        } else {
            LogStep::Quiet
        }
    }
}

/// The published status (one per process: [`global`]; a test makes its own).
#[derive(Debug, Default)]
pub struct EnvStore(RwLock<EnvironmentStatus>);

impl EnvStore {
    /// The status now (a copy: no lock is held after the call).
    pub fn current(&self) -> EnvironmentStatus {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the status.
    pub fn publish(&self, status: EnvironmentStatus) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = status;
    }
}

/// The process's status, read by `GET /api/v1/status` and
/// `GET /api/v1/environment`: not checked until [`run`]'s first check.
pub fn global() -> &'static EnvStore {
    static STORE: LazyLock<EnvStore> = LazyLock::new(EnvStore::default);
    &STORE
}

/// One check: publishes its status into `store` and logs what changed.
pub fn apply(store: &EnvStore, reporter: &mut Reporter, check: &Check, now_ms: i64) -> LogStep {
    let (status, problems) = status_of(check, now_ms);
    let step = reporter.step(&problems, &status.read_errors);
    log_step(step, &status, &problems);
    store.publish(status);
    step
}

/// The log line of a step.
#[cfg_attr(test, mutants::skip)] // log text only; the step is pinned by `Reporter`'s tests
fn log_step(step: LogStep, status: &EnvironmentStatus, problems: &[EnvProblem]) {
    match step {
        LogStep::Quiet => {}
        LogStep::Holds => match &status.note {
            Some(note) => info!(note = %note, "production environment: not checked"),
            None => info!(
                edition = ?status.edition_id,
                "production environment: holds (edition allowed, Defender and firewall off, \
                 audio profile set)"
            ),
        },
        LogStep::Problems => {
            let codes: Vec<String> = problems.iter().map(EnvProblem::code).collect();
            let texts: Vec<String> = problems.iter().map(EnvProblem::text_sk).collect();
            error!(
                problems = %codes.join(", "),
                edition = ?status.edition_id,
                read_errors = ?status.read_errors,
                "production environment: VIOLATED - {}",
                texts.join("; ")
            );
        }
        LogStep::Cleared => info!(
            edition = ?status.edition_id,
            read_errors = ?status.read_errors,
            "production environment: holds again (every problem cleared)"
        ),
        LogStep::ReadErrors => warn!(
            read_errors = ?status.read_errors,
            "production environment: what could not be read changed"
        ),
    }
}

/// Runs a blocking read with a bound: a read that does not finish within it
/// reads as unreadable, and the next call waits for that SAME read again
/// instead of starting another (a hung read never piles up threads). A read
/// that finished only after its call gave up is stale by then (up to a whole
/// check period old): the next call drops it and reads afresh, so a late
/// read is never published as a fresh one. #259: the wall check reads the
/// box through one too (`wall::run`).
pub struct Reader<T> {
    read: fn() -> T,
    /// What is read, in the errors ("WMI check", "wall read").
    what: &'static str,
    pending: Option<tokio::task::JoinHandle<T>>,
}

impl<T: Send + 'static> Reader<T> {
    /// A reader of the WMI facts `read`, run on the blocking pool.
    pub fn new(read: fn() -> T) -> Self {
        Self::named(read, "WMI check")
    }

    /// #259: a reader of `read`, run on the blocking pool, whose errors name
    /// `what`.
    pub fn named(read: fn() -> T, what: &'static str) -> Self {
        Self {
            read,
            what,
            pending: None,
        }
    }

    /// The read's value, or why it could not be had within `bound`.
    pub async fn next(&mut self, bound: Duration) -> Result<T, String> {
        let mut handle = match self.pending.take() {
            Some(late) if late.is_finished() => tokio::task::spawn_blocking(self.read),
            Some(running) => running,
            None => tokio::task::spawn_blocking(self.read),
        };
        match tokio::time::timeout(bound, &mut handle).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(format!("the {} failed: {e}", self.what)),
            Err(_) => {
                self.pending = Some(handle);
                Err(format!("the {} did not finish within {bound:?}", self.what))
            }
        }
    }
}

/// The Windows reads: the registry directly, WMI through the bounded reader.
pub struct Platform {
    direct: fn() -> DirectRead,
    wmi: Reader<WmiRead>,
}

impl Platform {
    /// The reads `direct` (at each check) and `wmi` (bounded).
    pub fn new(direct: fn() -> DirectRead, wmi: fn() -> WmiRead) -> Self {
        Self {
            direct,
            wmi: Reader::new(wmi),
        }
    }
}

/// This check's input: the facts on Windows (WMI within `bound`, the
/// registry as read now), none elsewhere.
pub async fn next_check(platform: Option<&mut Platform>, bound: Duration) -> Check {
    let Some(platform) = platform else {
        return Check::NotWindows;
    };
    let wmi = platform.wmi.next(bound).await;
    match tokio::task::spawn_blocking(platform.direct).await {
        Ok(direct) => Check::Read(facts_from(&direct, &wmi)),
        Err(e) => Check::Read(EnvFacts::unread(format!("the registry check failed: {e}"))),
    }
}

/// The platform's reads: Windows reads the facts (`prod_env_win.rs`).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // platform wiring; the reader's logic is tested
fn platform() -> Option<Platform> {
    Some(Platform::new(win::read_direct, win::read_wmi))
}

/// The platform's reads: nothing to read off Windows.
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // platform wiring; `next_check(None, ..)` is tested
fn platform() -> Option<Platform> {
    None
}

/// Checks the box at once and every [`CHECK_EVERY`], publishing into
/// [`global`], until the shutdown.
#[cfg_attr(test, mutants::skip)] // the loop; `apply`, `next_check` and `Reader` are tested
pub async fn run(mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let mut platform = platform();
    let mut reporter = Reporter::default();
    let mut tick = tokio::time::interval(CHECK_EVERY);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let check = next_check(platform.as_mut(), READ_BOUND).await;
                apply(global(), &mut reporter, &check, chrono::Utc::now().timestamp_millis());
            }
            _ = shutdown.recv() => break,
        }
    }
}

#[path = "prod_env_read.rs"]
pub mod read;

#[path = "prod_env_audio.rs"]
pub mod audio;
pub use read::{
    AvProduct, DefenderRow, DirectRead, FirewallRow, RegDword, RegString, ServiceRead, WmiRead,
    active_firewall, defender_realtime, facts_from, firewall_from_registry,
};

#[cfg(windows)]
#[path = "prod_env_win.rs"]
mod win;

#[cfg(test)]
#[path = "prod_env_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prod_env_reader_tests.rs"]
mod reader_tests;
