//! Per-host Resolume Arena driver — connects to one Resolume instance
//! and manages clip discovery and command handling.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::resolume::ResolumeCommand;
use crate::resolume::handlers;

const RESOLUTION_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// How long a full `/composition` clip-mapping refresh stays fresh before a
/// steady tick pulls it again. Distinct from `RESOLUTION_TTL` (DNS/endpoint
/// caching) even though both are 5 minutes — this one bounds how stale the
/// clip map may get between the light `/product` liveness probes (#157).
const FULL_REFRESH_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Minimum spacing between full `/composition` refresh ATTEMPTS (success or
/// failure). When `/product` answers but `/composition` keeps failing (Arena's
/// REST saturating), this stops the 14 MB fetch from being retried on every
/// ~10 s liveness tick — the retry storm the #157 review caught (#157).
const FULL_REFRESH_RETRY: Duration = Duration::from_secs(60);

/// Why a full `/composition` refresh is being performed. Drives the INFO
/// transition log and is the return type of the pure [`FullRefreshReason::decide`]
/// poll policy (#157). The steady state runs ONLY the light `/product` probe,
/// so a full refresh always has a specific, logged reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FullRefreshReason {
    /// The driver's first refresh at startup, or a later first-success while
    /// `last_full_ok` is still `None` (a startup where Arena's REST was dead
    /// but the breaker never opened — once it opens, recovery logs
    /// `BreakerClosed` instead, which `decide` checks first).
    Startup,
    /// An operator/engine `ResolumeCommand::RefreshMapping` forced it.
    Command,
    /// The 5-minute TTL since the last successful full refresh expired.
    Ttl,
    /// The circuit breaker just closed (Arena recovered) — resync the map once.
    BreakerClosed,
}

impl FullRefreshReason {
    /// Stable, log-friendly identifier for the INFO mode-transition line.
    #[cfg_attr(test, mutants::skip)] // log-only string mapping; no behavior is asserted on it
    fn as_str(self) -> &'static str {
        match self {
            FullRefreshReason::Startup => "startup",
            FullRefreshReason::Command => "command",
            FullRefreshReason::Ttl => "ttl",
            FullRefreshReason::BreakerClosed => "breaker-closed",
        }
    }

    /// Pure poll policy: decide whether — and why — a full `/composition`
    /// refresh should run on this tick. `now`, `last_full_ok` and
    /// `last_full_attempt` are monotonic `Instant`s so tests can construct
    /// synthetic forward-only clocks (the `is_expired_at` Windows-underflow-safe
    /// pattern).
    ///
    /// Precedence: a forced command wins immediately (bypasses the retry
    /// window), then a just-closed breaker, then the never-refreshed startup
    /// case, then TTL expiry. Returns `None` in the steady state (fresh cache,
    /// live REST). The three non-forced reasons additionally back off: a full
    /// refresh is never re-ATTEMPTED (success OR failure) within `retry_after`
    /// of the last attempt — the guard against a per-tick 14 MB retry storm
    /// when `/product` answers but `/composition` keeps failing (#157 review).
    fn decide(
        now: Instant,
        last_full_ok: Option<Instant>,
        last_full_attempt: Option<Instant>,
        ttl: Duration,
        retry_after: Duration,
        forced: bool,
        breaker_just_closed: bool,
    ) -> Option<FullRefreshReason> {
        if forced {
            return Some(FullRefreshReason::Command);
        }
        let reason = if breaker_just_closed {
            FullRefreshReason::BreakerClosed
        } else {
            match last_full_ok {
                None => FullRefreshReason::Startup,
                Some(last) => {
                    if now.duration_since(last) >= ttl {
                        FullRefreshReason::Ttl
                    } else {
                        return None;
                    }
                }
            }
        };
        // Retry backoff: suppress a demand-driven refresh that was ATTEMPTED
        // too recently, so a failing /composition is retried at most once per
        // window instead of on every liveness tick.
        if let Some(attempt) = last_full_attempt {
            if now.duration_since(attempt) < retry_after {
                return None;
            }
        }
        Some(reason)
    }
}

/// Poll interval for the light `/product` liveness probe: 10 s ± up to 2 s
/// of jitter, so multiple SongPlayer probers and the post-deploy E2E probe do
/// not hammer Arena's single-threaded REST in lockstep (#157). Derived from
/// the wall-clock sub-second nanos — no rng dependency; a ±2 s spread has no
/// deterministic unit oracle, so this helper is smoke-exercised only by the
/// live `run` loop.
#[cfg_attr(test, mutants::skip)] // non-deterministic jitter; no behavioral oracle, only the live loop uses it
fn jittered_poll_period() -> Duration {
    let subsec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter_ms = (subsec % 4001) as u64; // 0..=4000 ms
    Duration::from_millis(8000 + jitter_ms) // 8.0 s .. 12.0 s, centered ~10 s
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedEndpoint {
    pub base_url: String,
    pub host_header: Option<String>,
    pub resolved_at: Instant,
}

impl ResolvedEndpoint {
    fn from_ip(ip: &str, port: u16) -> Self {
        Self {
            base_url: format!("http://{ip}:{port}"),
            host_header: None,
            resolved_at: Instant::now(),
        }
    }

    fn from_resolved(ip: &str, hostname: &str, port: u16) -> Self {
        Self {
            base_url: format!("http://{ip}:{port}"),
            host_header: Some(format!("{hostname}:{port}")),
            resolved_at: Instant::now(),
        }
    }

    /// Return `true` if the endpoint should be re-resolved.
    ///
    /// Thin wrapper around `is_expired_at(Instant::now())`. Skipped from
    /// mutation testing because every observable behavior of this wrapper
    /// is already covered by `is_expired_at` tests (which use synthetic
    /// clocks to hit the `>` boundary exactly). A `-> false` mutant on
    /// this wrapper cannot be caught without waiting `RESOLUTION_TTL`
    /// real-time seconds or backdating `resolved_at` (which underflows
    /// Windows' monotonic clock on freshly booted CI runners).
    #[cfg_attr(test, mutants::skip)]
    fn is_expired(&self) -> bool {
        self.is_expired_at(Instant::now())
    }

    /// Pure function form of `is_expired` — takes an explicit `now` parameter
    /// so tests can construct synthetic "future" clocks without subtracting
    /// a large duration from `Instant::now()` (which underflows on Windows
    /// CI runners where the monotonic clock starts near zero).
    fn is_expired_at(&self, now: Instant) -> bool {
        now.duration_since(self.resolved_at) > RESOLUTION_TTL
    }
}

fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Information about a discovered Resolume clip.
#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub clip_id: i64,
    pub text_param_id: i64,
}

/// Per-host worker that communicates with a single Resolume Arena instance.
pub struct HostDriver {
    host: String,
    port: u16,
    client: reqwest::Client,
    /// Maps clip token (e.g. `"#sp-title"`) to list of matching clips.
    /// A single token can appear in multiple clips across layers/columns/decks
    /// and all of them are updated in parallel.
    pub(crate) clip_mapping: HashMap<String, Vec<ClipInfo>>,
    /// Cached DNS resolution for hostname-based hosts.
    endpoint_cache: Option<ResolvedEndpoint>,
    /// Set true after a successful refresh, false after a failure.
    last_refresh_ok: bool,
    /// Wall-clock timestamp of the last completed refresh attempt
    /// (success or failure). `None` until first attempt.
    last_refresh_ts: Option<chrono::DateTime<chrono::Utc>>,
    /// Number of consecutive refresh failures. Reset to 0 on success.
    consecutive_failures: u32,
    /// Whether the circuit breaker has tripped (≥30s of failures).
    circuit_breaker_open: bool,
    /// Round-trip of the last successful `/product` liveness probe, in ms.
    /// `None` after a failed probe or before the first one (#157).
    product_latency_ms: Option<u64>,
    /// Wall-clock timestamp of the last SUCCESSFUL full `/composition`
    /// refresh — surfaced in the health snapshot for before/after measuring.
    last_full_refresh_ts: Option<chrono::DateTime<chrono::Utc>>,
    /// Monotonic instant of the last successful full `/composition` refresh,
    /// used for the TTL decision. Separate from `last_full_refresh_ts` (which
    /// is a display timestamp) so TTL math stays on a monotonic clock (#157).
    last_full_refresh_ok_at: Option<Instant>,
    /// Monotonic instant of the last full-refresh ATTEMPT (success OR failure),
    /// used for the retry-backoff decision so a failing `/composition` is not
    /// re-fetched on every liveness tick (#157 review).
    last_full_attempt_at: Option<Instant>,
    /// Set via `with_recovery_channel` builder; never accessed directly.
    recovery_tx: Option<tokio::sync::broadcast::Sender<crate::resolume::RecoveryEvent>>,
    /// Set via `with_health_channel` builder; never accessed directly.
    health_tx: Option<tokio::sync::watch::Sender<crate::resolume::HostHealthSnapshot>>,
}

impl HostDriver {
    pub fn new(host: String, port: u16) -> Self {
        Self {
            host,
            port,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build reqwest client"),
            clip_mapping: HashMap::new(),
            endpoint_cache: None,
            last_refresh_ok: false,
            last_refresh_ts: None,
            consecutive_failures: 0,
            circuit_breaker_open: false,
            product_latency_ms: None,
            last_full_refresh_ts: None,
            last_full_refresh_ok_at: None,
            last_full_attempt_at: None,
            recovery_tx: None,
            health_tx: None,
        }
    }

    pub fn with_health_channel(
        mut self,
        tx: tokio::sync::watch::Sender<crate::resolume::HostHealthSnapshot>,
    ) -> Self {
        self.health_tx = Some(tx);
        self
    }

    pub fn with_recovery_channel(
        mut self,
        tx: tokio::sync::broadcast::Sender<crate::resolume::RecoveryEvent>,
    ) -> Self {
        self.recovery_tx = Some(tx);
        self
    }

    /// Main run loop: processes commands, periodically refreshes clip mapping,
    /// and shuts down on signal.
    /// Top-level worker loop. Tested via integration / live verification on
    /// win-resolume rather than unit-mutation tests — the loop integrates
    /// tokio::select!, the refresh interval, and the command channel.
    #[cfg_attr(test, mutants::skip)]
    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<ResolumeCommand>,
        mut shutdown: broadcast::Receiver<()>,
    ) {
        // Startup: one full clip-mapping refresh (like the legacy behavior).
        self.run_full_refresh(FullRefreshReason::Startup, Instant::now())
            .await;

        // Jittered liveness cadence. `sleep_until` a stable per-cycle deadline
        // (recomputed only when the probe actually fires) so command traffic
        // never resets or delays the next liveness probe.
        let mut next_probe = tokio::time::Instant::now() + jittered_poll_period();

        loop {
            tokio::select! {
                Some(cmd) = rx.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = tokio::time::sleep_until(next_probe) => {
                    self.on_tick_at(Instant::now()).await;
                    next_probe = tokio::time::Instant::now() + jittered_poll_period();
                }
                _ = shutdown.recv() => {
                    info!(host = %self.host, "Resolume driver shutting down");
                    break;
                }
            }
        }
    }

    /// One liveness tick: a light `/product` probe, then a full `/composition`
    /// refresh ONLY when the poll policy calls for one AND the REST is alive
    /// (never pile the ~14 MB fetch onto a dead/saturated single-thread REST).
    /// `now` is an explicit parameter so the TTL branch is testable with a
    /// synthetic forward-only clock (the `is_expired_at` pattern).
    async fn on_tick_at(&mut self, now: Instant) {
        let breaker_just_closed = self.probe_liveness().await;
        if !self.last_refresh_ok {
            return;
        }
        if let Some(reason) = FullRefreshReason::decide(
            now,
            self.last_full_refresh_ok_at,
            self.last_full_attempt_at,
            FULL_REFRESH_TTL,
            FULL_REFRESH_RETRY,
            false,
            breaker_just_closed,
        ) {
            self.run_full_refresh(reason, now).await;
        }
    }

    /// Record the attempt, log the mode transition, and run a full
    /// `/composition` refresh. `now` stamps `last_full_attempt_at` (the retry-
    /// backoff clock) so it stays on the same monotonic clock as the `decide`
    /// call that scheduled this refresh.
    #[cfg_attr(test, mutants::skip)] // the refresh call + attempt-stamp are covered by the retry/steady/ttl wiremock request-count tests; the info!/warn! lines are log-only
    async fn run_full_refresh(&mut self, reason: FullRefreshReason, now: Instant) {
        self.last_full_attempt_at = Some(now);
        info!(
            host = %self.host,
            reason = reason.as_str(),
            "Resolume full /composition refresh"
        );
        if let Err(e) = self.refresh_mapping().await {
            warn!(host = %self.host, %e, "Resolume full refresh failed");
        }
    }

    /// Light liveness probe: `GET /api/v1/product`. Records the round-trip
    /// latency and feeds the shared failure/breaker machinery. Returns whether
    /// the circuit breaker transitioned open→closed on this probe (reason d).
    async fn probe_liveness(&mut self) -> bool {
        let started = Instant::now();
        match self.fetch_product().await {
            Ok(()) => {
                let latency_ms = started.elapsed().as_millis() as u64;
                self.product_latency_ms = Some(latency_ms);
                debug!(host = %self.host, latency_ms, "Resolume /product liveness ok");
                self.apply_outcome(true)
            }
            Err(e) => {
                self.product_latency_ms = None;
                debug!(host = %self.host, %e, "Resolume /product liveness failed");
                self.apply_outcome(false)
            }
        }
    }

    /// `GET /api/v1/product` — a small JSON payload used only to confirm the
    /// Arena REST server is alive, without pulling the ~14 MB composition.
    async fn fetch_product(&mut self) -> Result<(), anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!("{}/api/v1/product", ep.base_url);
        let req = self.client.get(&url);
        Self::apply_host_header(req, &ep)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Shared per-attempt bookkeeping for BOTH the liveness probe and the full
    /// refresh: updates the failure counter, circuit breaker, recovery event,
    /// and the health snapshot. Returns whether the breaker transitioned
    /// open→closed on this call (so the caller can trigger a resync refresh).
    /// The WARN / circuit-breaker log lines are kept byte-identical to the
    /// pre-#157 inline machinery.
    fn apply_outcome(&mut self, ok: bool) -> bool {
        let mut breaker_just_closed = false;
        self.last_refresh_ts = Some(chrono::Utc::now());
        if ok {
            self.last_refresh_ok = true;
            let was_failing = self.consecutive_failures > 0;
            self.consecutive_failures = 0;
            if self.circuit_breaker_open {
                self.circuit_breaker_open = false;
                breaker_just_closed = true;
                info!(host = %self.host, "circuit breaker closed — Resolume recovered");
            }
            if was_failing {
                if let Some(tx) = &self.recovery_tx {
                    let _ = tx.send(crate::resolume::RecoveryEvent {
                        host: self.host.clone(),
                    });
                }
                info!(host = %self.host, "Resolume recovery — RecoveryEvent fired");
            }
        } else {
            self.last_refresh_ok = false;
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if Self::should_emit_repeated_failure_warn(self.consecutive_failures) {
                warn!(
                    host = %self.host,
                    consecutive_failures = self.consecutive_failures,
                    "Resolume refresh failing repeatedly"
                );
            }
            if self.consecutive_failures >= Self::CIRCUIT_OPEN_THRESHOLD
                && !self.circuit_breaker_open
            {
                self.circuit_breaker_open = true;
                self.clip_mapping = HashMap::new();
                warn!(host = %self.host, "circuit breaker opened — clip cache evicted");
            }
        }
        self.publish_health();
        breaker_just_closed
    }

    /// Publish the current health snapshot on the watch channel (if wired).
    fn publish_health(&self) {
        if let Some(tx) = &self.health_tx {
            let snapshot = crate::resolume::HostHealthSnapshot {
                host: self.host.clone(),
                last_refresh_ts: self.last_refresh_ts,
                last_refresh_ok: self.last_refresh_ok,
                consecutive_failures: self.consecutive_failures,
                circuit_breaker_open: self.circuit_breaker_open,
                product_latency_ms: self.product_latency_ms,
                last_full_refresh_ts: self.last_full_refresh_ts,
                // Only SongPlayer-relevant tokens. The driver scans the
                // entire composition for `#`-prefixed names, but operators
                // have many of their own tokens (#bible-*, #timer,
                // #translate-*-u-re, etc.) that are noise to this dashboard.
                clips_by_token: [
                    crate::resolume::TITLE_TOKEN,
                    crate::resolume::SUBS_TOKEN,
                    crate::resolume::SUBS_NEXT_TOKEN,
                    crate::resolume::SUBS_SK_TOKEN,
                ]
                .iter()
                .map(|t| {
                    (
                        (*t).to_string(),
                        self.clip_mapping.get(*t).map(|v| v.len()).unwrap_or(0),
                    )
                })
                .collect(),
            };
            let _ = tx.send(snapshot);
        }
    }

    /// Handle a single command. Pure dispatch to handlers — each branch is
    /// covered by wiremock tests in `handlers.rs`.
    #[cfg_attr(test, mutants::skip)]
    async fn handle_command(&mut self, cmd: ResolumeCommand) {
        match cmd {
            ResolumeCommand::ShowTitle { song, artist } => {
                if let Err(e) = handlers::show_title(self, &song, &artist).await {
                    warn!(host = %self.host, %e, "show_title failed");
                }
            }
            ResolumeCommand::HideTitle => {
                if let Err(e) = handlers::hide_title(self).await {
                    warn!(host = %self.host, %e, "hide_title failed");
                }
            }
            ResolumeCommand::ShowSubtitles {
                en,
                next_en,
                sk,
                next_sk,
                suppress_en,
            } => {
                if let Err(e) = handlers::set_subtitles(
                    self,
                    &en,
                    &next_en,
                    sk.as_deref(),
                    next_sk.as_deref(),
                    suppress_en,
                )
                .await
                {
                    warn!(host = %self.host, %e, "subtitle set failed");
                }
            }
            ResolumeCommand::HideSubtitles => {
                if let Err(e) = handlers::clear_subtitles(self).await {
                    warn!(host = %self.host, %e, "subtitle clear failed");
                }
            }
            ResolumeCommand::RefreshMapping => {
                // A command forces a full refresh regardless of TTL/liveness/
                // retry window.
                let now = Instant::now();
                if let Some(reason) = FullRefreshReason::decide(
                    now,
                    self.last_full_refresh_ok_at,
                    self.last_full_attempt_at,
                    FULL_REFRESH_TTL,
                    FULL_REFRESH_RETRY,
                    true,
                    false,
                ) {
                    self.run_full_refresh(reason, now).await;
                }
            }
            ResolumeCommand::Shutdown => {
                info!(host = %self.host, "received shutdown command");
            }
        }
    }

    const FAIL_WARN_THRESHOLD: u32 = 2;
    const CIRCUIT_OPEN_THRESHOLD: u32 = 3;

    #[cfg_attr(test, mutants::skip)] // log-only diagnostic; CIRCUIT_OPEN_THRESHOLD covers system-critical boundary
    fn should_emit_repeated_failure_warn(failures: u32) -> bool {
        failures >= Self::FAIL_WARN_THRESHOLD
    }

    /// Fetch composition JSON from Resolume and build clip mapping from
    /// `#token` tags found in clip names.
    ///
    /// `GET /api/v1/composition`
    pub(crate) async fn refresh_mapping(&mut self) -> Result<(), anyhow::Error> {
        match self.fetch_mapping_inner().await {
            Ok(new_mapping) => {
                self.last_full_refresh_ok_at = Some(Instant::now());
                self.last_full_refresh_ts = Some(chrono::Utc::now());
                if new_mapping != self.clip_mapping {
                    let total: usize = new_mapping.values().map(|v| v.len()).sum();
                    info!(
                        host = %self.host,
                        tokens = new_mapping.len(),
                        clips = total,
                        "updated Resolume clip mapping"
                    );
                    self.clip_mapping = new_mapping;
                }
                self.apply_outcome(true);
                Ok(())
            }
            Err(e) => {
                self.apply_outcome(false);
                Err(e)
            }
        }
    }

    async fn fetch_mapping_inner(
        &mut self,
    ) -> Result<HashMap<String, Vec<ClipInfo>>, anyhow::Error> {
        let ep = self.endpoint().await?;
        let url = format!("{}/api/v1/composition", ep.base_url);
        let req = self.client.get(&url);
        let resp = Self::apply_host_header(req, &ep).send().await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(parse_composition(&body))
    }

    /// Ensure the endpoint cache is populated. Call before parallel operations
    /// that need to use `set_text`/`set_clip_opacity` concurrently via `&self`.
    pub(crate) async fn ensure_endpoint(&mut self) -> Result<(), anyhow::Error> {
        let _ = self.endpoint().await?;
        Ok(())
    }

    /// Get the cached endpoint (must call `ensure_endpoint` first).
    fn cached_endpoint(&self) -> Option<&ResolvedEndpoint> {
        self.endpoint_cache.as_ref().filter(|ep| !ep.is_expired())
    }

    /// Resolve the host to an endpoint, caching the result for 5 minutes.
    /// For IP literals, no DNS lookup is needed. For hostnames, we resolve
    /// via DNS and store the IP in the URL with the original hostname in the
    /// Host header (required by Resolume when addressed by hostname).
    async fn endpoint(&mut self) -> Result<ResolvedEndpoint, anyhow::Error> {
        if let Some(ref cached) = self.endpoint_cache {
            if !cached.is_expired() {
                return Ok(cached.clone());
            }
        }
        let ep = if is_ip_literal(&self.host) {
            ResolvedEndpoint::from_ip(&self.host, self.port)
        } else {
            let lookup = format!("{}:{}", self.host, self.port);
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host(&lookup).await?.collect();
            let addr = addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or(addrs.first())
                .ok_or_else(|| {
                    anyhow::anyhow!("DNS lookup returned no addresses for {}", self.host)
                })?;
            ResolvedEndpoint::from_resolved(&addr.ip().to_string(), &self.host, self.port)
        };
        self.endpoint_cache = Some(ep.clone());
        Ok(ep)
    }

    /// Build a request with the `Host` header set if the endpoint requires it.
    ///
    /// Uses the typed `reqwest::header::HOST` constant which reqwest/hyper
    /// treats as a replacement for the auto-generated Host header derived from
    /// the URL authority. Passing the header as a raw string `"Host"` would
    /// append rather than replace, leading to undefined behavior.
    fn apply_host_header(
        builder: reqwest::RequestBuilder,
        ep: &ResolvedEndpoint,
    ) -> reqwest::RequestBuilder {
        if let Some(ref host) = ep.host_header {
            builder.header(reqwest::header::HOST, host)
        } else {
            builder
        }
    }

    /// Set text on a clip parameter.
    ///
    /// `PUT /api/v1/parameter/by-id/{param_id}`
    ///
    /// Takes `&self` so multiple calls can be driven in parallel via
    /// `FuturesUnordered`. Caller MUST have called `ensure_endpoint` first.
    pub(crate) async fn set_text(&self, param_id: i64, text: &str) -> Result<(), anyhow::Error> {
        let ep = self
            .cached_endpoint()
            .ok_or_else(|| anyhow::anyhow!("endpoint cache empty - call ensure_endpoint first"))?
            .clone();
        let url = format!("{}/api/v1/parameter/by-id/{param_id}", ep.base_url);
        let req = self
            .client
            .put(&url)
            .json(&serde_json::json!({ "value": text }));
        Self::apply_host_header(req, &ep)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Set the opacity of a clip.
    ///
    /// `PUT /api/v1/composition/clips/by-id/{clip_id}`
    ///
    /// Takes `&self` so multiple calls can be driven in parallel via
    /// `FuturesUnordered`. Caller MUST have called `ensure_endpoint` first.
    pub(crate) async fn set_clip_opacity(
        &self,
        clip_id: i64,
        opacity: f64,
    ) -> Result<(), anyhow::Error> {
        let ep = self
            .cached_endpoint()
            .ok_or_else(|| anyhow::anyhow!("endpoint cache empty - call ensure_endpoint first"))?
            .clone();
        let url = format!("{}/api/v1/composition/clips/by-id/{clip_id}", ep.base_url);
        let req = self
            .client
            .put(&url)
            .json(&serde_json::json!({"video":{"opacity":{"value": opacity}}}));
        Self::apply_host_header(req, &ep)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Extract the text parameter ID from a clip by scanning `video.sourceparams`
/// for the entry with `"valuetype": "ParamText"`.
///
/// Resolume Arena versions use different key names (`Text`, `Text1`, etc.)
/// so we cannot rely on a fixed key — instead we match on `valuetype`.
fn extract_text_param_id(clip: &serde_json::Value) -> Option<i64> {
    let params = clip["video"]["sourceparams"].as_object()?;
    for (_key, param) in params {
        if param["valuetype"].as_str() == Some("ParamText") {
            return param["id"].as_i64();
        }
    }
    None
}

/// Parse a Resolume composition JSON and extract clip tokens.
///
/// Scans `layers[].clips[].name.value` for words starting with `#`. Each
/// token is mapped to the clip's ID and the text source parameter ID
/// (found by scanning `sourceparams` for `valuetype == "ParamText"`).
pub fn parse_composition(composition: &serde_json::Value) -> HashMap<String, Vec<ClipInfo>> {
    let mut mapping: HashMap<String, Vec<ClipInfo>> = HashMap::new();

    let layers = match composition["layers"].as_array() {
        Some(l) => l,
        None => return mapping,
    };

    for layer in layers {
        let clips = match layer["clips"].as_array() {
            Some(c) => c,
            None => continue,
        };

        for clip in clips {
            let clip_id = match clip["id"].as_i64() {
                Some(id) => id,
                None => continue,
            };

            let name = match clip["name"]["value"].as_str() {
                Some(n) => n,
                None => continue,
            };

            // Extract #tokens from the name.
            let tokens: Vec<&str> = name
                .split_whitespace()
                .filter(|w| w.starts_with('#'))
                .collect();

            if tokens.is_empty() {
                continue;
            }

            // Find the text source parameter ID by valuetype scan.
            let text_param_id = match extract_text_param_id(clip) {
                Some(id) => id,
                None => continue,
            };

            for token in tokens {
                mapping
                    .entry(token.to_string())
                    .or_default()
                    .push(ClipInfo {
                        clip_id,
                        text_param_id,
                    });
            }
        }
    }

    mapping
}

// Implement PartialEq for ClipInfo so we can compare mappings.
impl PartialEq for ClipInfo {
    fn eq(&self, other: &Self) -> bool {
        self.clip_id == other.clip_id && self.text_param_id == other.text_param_id
    }
}

impl Eq for ClipInfo {}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "driver_poll_tests.rs"]
mod poll_tests;
