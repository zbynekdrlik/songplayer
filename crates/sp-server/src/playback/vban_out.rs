//! The program's VBAN audio output (#210, B2 of EPIC #174). Design record:
//! #210 comment 5846308506 (Approach 1).
//!
//! FOH (VB-Matrix on fohabl) and lv1 take the program audio as VBAN. The
//! `SP-program` sender thread (`program_output.rs`) hands each submitted
//! pair's audio block (a forwarded source block or the standby silence) to
//! [`VbanOut::push`] right after its NDI submit. The hand-off is a bounded,
//! never-blocking queue: over [`VBAN_QUEUE_BOUND`] the OLDEST block is dropped
//! and counted. A dedicated thread ([`run_vban_loop`]) encodes each block into
//! 8 packets of 200 frames (`vban_packet.rs`) and sends packet `k` of the
//! boundary `B` at `due(B) + L + k/240 s`, where L is one slot
//! ([`VBAN_SEND_LATENCY_100NS`]). It paces on its own [`WallClock`], ticked
//! once per grid boundary like the program wall ([`WallVbanClock`]), so the
//! sends are one packet every 4.1667 ms and never a burst. The frame counter
//! grows by exactly 1 per packet across cuts and standby.
//!
//! One UDP socket sends to every resolved target. The settings (`vban_enabled`,
//! `vban_stream_name`, `vban_targets`) are re-read every
//! [`VBAN_SETTINGS_POLL`] by [`run_vban_config_task`]. DNS is resolved when
//! they change and re-resolved every [`VBAN_RESOLVE_EVERY`]; a target whose
//! re-resolve fails keeps its last good address. Telemetry is [`VbanStatus`],
//! served under `vban` on `GET /api/v1/program`.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use sp_core::config::{
    DEFAULT_VBAN_STREAM_NAME, SETTING_VBAN_ENABLED, SETTING_VBAN_STREAM_NAME, SETTING_VBAN_TARGETS,
};
use sp_core::genlock::GENLOCK_MAX_CATCHUP_INTERVALS;
use sp_ndi::AudioFrame;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::loop_stats::percentile_ceil;
use crate::playback::program_output::BoundaryTicker;
use crate::playback::vban_packet::{
    VBAN_BLOCK_SAMPLES, VBAN_CHANNELS, VBAN_SAMPLE_RATE_HZ, VBAN_SEND_LATENCY_100NS,
    VBAN_STREAM_NAME_LEN, VbanBlockPackets, VbanEncoder, empty_block_packets, packet_send_at_100ns,
    stream_name_bytes,
};
use crate::playback::wallclock::WallClock;

/// Queue bound: a full program catch-up (8 slots) plus the block behind it,
/// with one to spare — the program queue's own bound.
pub const VBAN_QUEUE_BOUND: usize = GENLOCK_MAX_CATCHUP_INTERVALS as usize + 2;

/// A packet sent more than this after its due time is a late send (2 ms).
pub const VBAN_LATE_100NS: i64 = 20_000;

/// Longest single wait before a packet (4 slots). A due time further ahead is
/// a clock mismatch; the thread never parks on it.
pub const VBAN_MAX_WAIT_100NS: i64 = 4 * VBAN_SEND_LATENCY_100NS;

/// Send intervals kept for the p99 (the last 5 s at 240 packets/s).
pub const VBAN_INTERVAL_WINDOW: usize = 1200;

/// How long the thread waits for a block before it checks for a stop again.
pub const VBAN_IDLE_WAIT: Duration = Duration::from_millis(500);

/// How often the settings are re-read (a dashboard save applies within this).
pub const VBAN_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// How often DNS is re-resolved when the settings did not change.
pub const VBAN_RESOLVE_EVERY: Duration = Duration::from_secs(60);

/// A repeating warning (overflow, substitution, send error) is logged on its
/// first occurrence and then every this many.
pub const VBAN_LOG_EVERY: u64 = 1000;

/// One program boundary's audio for VBAN.
#[derive(Clone, Debug, PartialEq)]
pub struct VbanBlock {
    /// The boundary the block belongs to (the pair's video stamp, 100 ns).
    pub due_100ns: i64,
    /// 3200 interleaved stereo samples; `None` = silence.
    pub samples: Option<Vec<f32>>,
    /// The pair's audio was not one program block and is sent as silence.
    pub substituted: bool,
}

impl VbanBlock {
    /// The program's standby silence for `due_100ns`.
    pub fn silence(due_100ns: i64) -> Self {
        Self {
            due_100ns,
            samples: None,
            substituted: false,
        }
    }

    /// A forwarded pair's audio: exactly one 48 kHz stereo 1600-frame frame
    /// is moved in as is (no copy); anything else becomes silence, marked
    /// `substituted`.
    pub fn from_frames(due_100ns: i64, mut frames: Vec<AudioFrame>) -> Self {
        let samples = match (frames.len(), frames.pop()) {
            (1, Some(frame)) if is_program_block(&frame) => Some(frame.data),
            _ => None,
        };
        Self {
            due_100ns,
            substituted: samples.is_none(),
            samples,
        }
    }
}

/// `frame` is one program audio block: 48 kHz, stereo, 1600 frames.
pub fn is_program_block(frame: &AudioFrame) -> bool {
    frame.channels as usize == VBAN_CHANNELS
        && i64::from(frame.sample_rate) == VBAN_SAMPLE_RATE_HZ
        && frame.data.len() == VBAN_BLOCK_SAMPLES
}

/// The three VBAN settings as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VbanSettings {
    pub enabled: bool,
    pub stream_name: String,
    /// Comma-separated `host:port` list, as typed.
    pub targets: String,
}

impl Default for VbanSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            stream_name: DEFAULT_VBAN_STREAM_NAME.to_string(),
            targets: String::new(),
        }
    }
}

impl VbanSettings {
    /// The non-empty, trimmed `host:port` entries of `targets`.
    pub fn target_specs(&self) -> Vec<String> {
        self.targets
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }
}

/// Read the VBAN settings: `vban_enabled == "true"` enables, a blank stream
/// name falls back to [`DEFAULT_VBAN_STREAM_NAME`], absent targets = none.
pub async fn load_vban_settings(pool: &SqlitePool) -> Result<VbanSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let enabled = get_setting(pool, SETTING_VBAN_ENABLED)
        .await?
        .is_some_and(|v| v.trim() == "true");
    let stream_name = get_setting(pool, SETTING_VBAN_STREAM_NAME)
        .await?
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_VBAN_STREAM_NAME.to_string());
    let targets = get_setting(pool, SETTING_VBAN_TARGETS)
        .await?
        .unwrap_or_default();
    Ok(VbanSettings {
        enabled,
        stream_name,
        targets,
    })
}

/// One configured target and what it resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VbanTarget {
    /// The `host:port` entry as configured.
    pub spec: String,
    /// The address packets go to; `None` = never resolved.
    pub addr: Option<SocketAddr>,
    /// The last resolve error (the target may still keep an older address).
    pub error: Option<String>,
}

/// The first IPv4 address (the socket is IPv4).
pub fn pick_ipv4(addrs: impl IntoIterator<Item = SocketAddr>) -> Option<SocketAddr> {
    addrs.into_iter().find(SocketAddr::is_ipv4)
}

/// Resolve one `host:port` with the system resolver to its first IPv4 address.
pub fn system_resolve(spec: &str) -> Result<SocketAddr, String> {
    let addrs = spec.to_socket_addrs().map_err(|e| e.to_string())?;
    pick_ipv4(addrs).ok_or_else(|| "no IPv4 address".to_string())
}

/// Resolve every spec with `resolve`. A spec that fails keeps the address it
/// had in `previous` (a DNS hiccup never silences a working target) and
/// carries the error.
pub fn resolve_targets(
    specs: &[String],
    previous: &[VbanTarget],
    resolve: &mut dyn FnMut(&str) -> Result<SocketAddr, String>,
) -> Vec<VbanTarget> {
    specs
        .iter()
        .map(|spec| match resolve(spec.as_str()) {
            Ok(addr) => VbanTarget {
                spec: spec.clone(),
                addr: Some(addr),
                error: None,
            },
            Err(error) => VbanTarget {
                spec: spec.clone(),
                addr: previous
                    .iter()
                    .find(|t| t.spec == *spec)
                    .and_then(|t| t.addr),
                error: Some(error),
            },
        })
        .collect()
}

/// Re-resolve when the settings changed, on the first pass, or once
/// [`VBAN_RESOLVE_EVERY`] passed since the last resolve.
pub fn needs_resolve(changed: bool, since_last: Option<Duration>) -> bool {
    changed || since_last.is_none_or(|d| d >= VBAN_RESOLVE_EVERY)
}

/// A repeating warning is logged on its first occurrence and every
/// [`VBAN_LOG_EVERY`]th.
pub fn should_log(count: u64) -> bool {
    count == 1 || count % VBAN_LOG_EVERY == 0
}

/// The configuration the VBAN thread sends with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VbanConfig {
    pub enabled: bool,
    /// The name on the wire (ASCII, at most 16 characters).
    pub stream_name: String,
    pub name_bytes: [u8; VBAN_STREAM_NAME_LEN],
    pub targets: Vec<VbanTarget>,
}

impl VbanConfig {
    /// The config for `settings` over the resolved `targets`.
    pub fn new(settings: &VbanSettings, targets: Vec<VbanTarget>) -> Self {
        let name_bytes = stream_name_bytes(&settings.stream_name);
        let stream_name = name_bytes
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        Self {
            enabled: settings.enabled,
            stream_name,
            name_bytes,
            targets,
        }
    }

    /// Enabled with at least one resolved target: packets go out.
    pub fn is_active(&self) -> bool {
        self.enabled && self.targets.iter().any(|t| t.addr.is_some())
    }
}

impl Default for VbanConfig {
    fn default() -> Self {
        Self::new(&VbanSettings::default(), Vec::new())
    }
}

/// Resolve `settings` on the blocking pool (std DNS) into a config.
pub async fn resolve_config(settings: VbanSettings, previous: Vec<VbanTarget>) -> VbanConfig {
    let joined = tokio::task::spawn_blocking(move || {
        let mut resolve = system_resolve;
        let targets = resolve_targets(&settings.target_specs(), &previous, &mut resolve);
        VbanConfig::new(&settings, targets)
    })
    .await;
    joined.unwrap_or_else(|e| {
        warn!(%e, "vban output: the resolve task failed — output disabled");
        VbanConfig::default()
    })
}

/// A target as served under `vban.targets`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VbanTargetStatus {
    pub target: String,
    pub addr: Option<String>,
    pub error: Option<String>,
}

/// `GET /api/v1/program` → `vban`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VbanStatus {
    pub enabled: bool,
    pub stream_name: String,
    /// Packets sent (each to every resolved target).
    pub packets_sent: u64,
    /// UDP datagrams whose send failed.
    pub send_errors: u64,
    /// Blocks dropped on a full queue (the oldest one each time).
    pub blocks_dropped: u64,
    /// Pairs whose audio was not one program block, sent as silence.
    pub blocks_substituted: u64,
    /// Packets sent more than 2 ms after their due time.
    pub late_sends: u64,
    /// p99 of the last [`VBAN_INTERVAL_WINDOW`] packet-to-packet intervals.
    pub send_interval_p99_us: u64,
    /// `nuFrame` of the last packet sent.
    pub frame_counter: u32,
    pub targets: Vec<VbanTargetStatus>,
}

#[derive(Debug, Default)]
struct VbanCounters {
    packets_sent: u64,
    send_errors: u64,
    blocks_dropped: u64,
    blocks_substituted: u64,
    late_sends: u64,
    frame_counter: u32,
    intervals_us: VecDeque<u64>,
}

struct VbanQueue {
    blocks: VecDeque<VbanBlock>,
    stop: bool,
}

/// What [`VbanOut::take_timeout`] returned.
#[derive(Debug, PartialEq)]
pub enum VbanTake {
    Block(VbanBlock),
    /// The wait timed out with nothing queued.
    Idle,
    /// Stopped and drained.
    Stopped,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The shared VBAN output: the hand-off queue, the current config and the
/// counters. Every method holds a lock for µs only.
pub struct VbanOut {
    queue: Mutex<VbanQueue>,
    ready: Condvar,
    config: Mutex<Arc<VbanConfig>>,
    stats: Mutex<VbanCounters>,
}

impl Default for VbanOut {
    fn default() -> Self {
        Self::new()
    }
}

impl VbanOut {
    /// Disabled, no targets, nothing queued.
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VbanQueue {
                blocks: VecDeque::with_capacity(VBAN_QUEUE_BOUND + 1),
                stop: false,
            }),
            ready: Condvar::new(),
            config: Mutex::new(Arc::new(VbanConfig::default())),
            stats: Mutex::new(VbanCounters::default()),
        }
    }

    /// Hand one block over. Never blocks; over [`VBAN_QUEUE_BOUND`] the
    /// oldest block is dropped and counted.
    pub fn push(&self, block: VbanBlock) {
        let dropped = {
            let mut q = lock(&self.queue);
            q.blocks.push_back(block);
            let over = q.blocks.len() > VBAN_QUEUE_BOUND;
            if over {
                q.blocks.pop_front();
            }
            over
        };
        self.ready.notify_one();
        if dropped {
            let n = {
                let mut s = lock(&self.stats);
                s.blocks_dropped += 1;
                s.blocks_dropped
            };
            if should_log(n) {
                warn!(
                    blocks_dropped = n,
                    bound = VBAN_QUEUE_BOUND,
                    "vban output: queue full — dropped the oldest block (the VBAN thread fell behind)"
                );
            }
        }
    }

    /// The next block, waiting at most `wait` for one. Queued blocks are
    /// drained before [`VbanTake::Stopped`].
    pub fn take_timeout(&self, wait: Duration) -> VbanTake {
        let q = lock(&self.queue);
        let (mut q, _) = self
            .ready
            .wait_timeout_while(q, wait, |q| q.blocks.is_empty() && !q.stop)
            .unwrap_or_else(|p| p.into_inner());
        match q.blocks.pop_front() {
            Some(block) => VbanTake::Block(block),
            None if q.stop => VbanTake::Stopped,
            None => VbanTake::Idle,
        }
    }

    /// Blocks waiting in the queue.
    pub fn queued(&self) -> usize {
        lock(&self.queue).blocks.len()
    }

    /// Stop the thread once the queue is drained (process shutdown).
    pub fn stop(&self) {
        lock(&self.queue).stop = true;
        self.ready.notify_all();
    }

    /// Replace the config (the settings task).
    pub fn set_config(&self, config: VbanConfig) {
        *lock(&self.config) = Arc::new(config);
    }

    /// The current config.
    pub fn config(&self) -> Arc<VbanConfig> {
        lock(&self.config).clone()
    }

    /// Count one substituted block; returns the total.
    fn record_substituted(&self) -> u64 {
        let mut s = lock(&self.stats);
        s.blocks_substituted += 1;
        s.blocks_substituted
    }

    /// Count one sent packet; returns the total send errors.
    fn record_packet(&self, sent: SentPacket) -> u64 {
        let mut s = lock(&self.stats);
        s.packets_sent += 1;
        s.send_errors += sent.errors;
        if sent.late {
            s.late_sends += 1;
        }
        if let Some(us) = sent.interval_us {
            if s.intervals_us.len() == VBAN_INTERVAL_WINDOW {
                s.intervals_us.pop_front();
            }
            s.intervals_us.push_back(us);
        }
        s.frame_counter = sent.counter;
        s.send_errors
    }

    /// The telemetry for the API.
    pub fn status(&self) -> VbanStatus {
        let cfg = self.config();
        let s = lock(&self.stats);
        VbanStatus {
            enabled: cfg.enabled,
            stream_name: cfg.stream_name.clone(),
            packets_sent: s.packets_sent,
            send_errors: s.send_errors,
            blocks_dropped: s.blocks_dropped,
            blocks_substituted: s.blocks_substituted,
            late_sends: s.late_sends,
            send_interval_p99_us: percentile_ceil(&s.intervals_us, 99),
            frame_counter: s.frame_counter,
            targets: cfg
                .targets
                .iter()
                .map(|t| VbanTargetStatus {
                    target: t.spec.clone(),
                    addr: t.addr.map(|a| a.to_string()),
                    error: t.error.clone(),
                })
                .collect(),
        }
    }
}

/// One sent packet, for the counters.
struct SentPacket {
    late: bool,
    interval_us: Option<u64>,
    errors: u64,
    counter: u32,
}

/// The clock the VBAN thread paces on (the program's wall domain).
pub trait VbanClock {
    /// Now, 100 ns since the Unix epoch.
    fn now_100ns(&mut self) -> i64;
    /// Sleep `d_100ns`.
    fn sleep_100ns(&mut self, d_100ns: i64);
}

/// Where packets go (a UDP socket in production).
pub trait VbanSink {
    fn send_packet(&mut self, packet: &[u8], addr: SocketAddr) -> io::Result<usize>;
}

impl VbanSink for UdpSocket {
    fn send_packet(&mut self, packet: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.send_to(packet, addr)
    }
}

/// Production clock: a [`WallClock`] ticked once per grid boundary passed
/// ([`BoundaryTicker`], the program wall's cadence), so it slews a UTC step in
/// at the same rate as the program and the source walls — one clock domain.
pub struct WallVbanClock {
    wall: WallClock,
    ticker: BoundaryTicker,
}

impl WallVbanClock {
    pub fn new(wall: WallClock) -> Self {
        Self {
            wall,
            ticker: BoundaryTicker::default(),
        }
    }
}

impl VbanClock for WallVbanClock {
    fn now_100ns(&mut self) -> i64 {
        let now = self.wall.now_100ns();
        for _ in 0..self.ticker.advance(now) {
            self.wall.tick();
        }
        self.wall.now_100ns()
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        std::thread::sleep(Duration::from_nanos(d_100ns.max(0) as u64 * 100));
    }
}

/// How long to wait from `now_100ns` for a packet due at `at_100ns`: never
/// negative, never more than [`VBAN_MAX_WAIT_100NS`].
pub fn plan_wait_100ns(now_100ns: i64, at_100ns: i64) -> i64 {
    (at_100ns - now_100ns).clamp(0, VBAN_MAX_WAIT_100NS)
}

/// Packet-to-packet interval in µs (0 on a backward clock read).
pub fn interval_us(prev_100ns: i64, now_100ns: i64) -> u64 {
    (now_100ns - prev_100ns).max(0) as u64 / 10
}

/// The sending side of one VBAN stream: the encoder (frame counter), the
/// reusable packet buffer and the last send instant.
pub struct VbanSender {
    encoder: VbanEncoder,
    packets: Box<VbanBlockPackets>,
    last_send_100ns: Option<i64>,
    latency_100ns: i64,
}

impl Default for VbanSender {
    fn default() -> Self {
        Self {
            encoder: VbanEncoder::default(),
            packets: empty_block_packets(),
            last_send_100ns: None,
            latency_100ns: VBAN_SEND_LATENCY_100NS,
        }
    }
}

impl VbanSender {
    /// The `nuFrame` the next packet carries.
    pub fn next_counter(&self) -> u32 {
        self.encoder.next_counter()
    }

    /// Send one block's 8 packets on their schedule to every resolved target,
    /// or nothing while the output is disabled or has no resolved target.
    /// Returns the packets sent.
    pub fn send_block(
        &mut self,
        out: &VbanOut,
        block: &VbanBlock,
        sink: &mut dyn VbanSink,
        clock: &mut dyn VbanClock,
    ) -> usize {
        if block.substituted {
            let n = out.record_substituted();
            if should_log(n) {
                warn!(
                    blocks_substituted = n,
                    due_100ns = block.due_100ns,
                    "vban output: a program pair's audio was not one 48 kHz stereo 1600-frame block — sent silence"
                );
            }
        }
        let cfg = out.config();
        if !cfg.is_active() {
            self.last_send_100ns = None; // a disabled gap is not a send interval
            return 0;
        }
        let first = self.encoder.next_counter();
        self.encoder
            .encode_block(&cfg.name_bytes, block.samples.as_deref(), &mut self.packets);
        for (k, packet) in self.packets.iter().enumerate() {
            let at = packet_send_at_100ns(block.due_100ns, self.latency_100ns, k);
            let wait = plan_wait_100ns(clock.now_100ns(), at);
            if wait > 0 {
                clock.sleep_100ns(wait);
            }
            let sent_at = clock.now_100ns();
            let mut errors = 0;
            let mut last_error = None;
            for addr in cfg.targets.iter().filter_map(|t| t.addr) {
                if let Err(e) = sink.send_packet(packet, addr) {
                    errors += 1;
                    last_error = Some((addr, e));
                }
            }
            let total_errors = out.record_packet(SentPacket {
                late: sent_at - at > VBAN_LATE_100NS,
                interval_us: self.last_send_100ns.map(|prev| interval_us(prev, sent_at)),
                errors,
                counter: first.wrapping_add(k as u32),
            });
            self.last_send_100ns = Some(sent_at);
            if let Some((addr, e)) = last_error
                && should_log(total_errors)
            {
                warn!(%e, %addr, send_errors = total_errors, "vban output: UDP send failed");
            }
        }
        self.packets.len()
    }
}

/// The VBAN thread body: send every queued block on its schedule until the
/// output is stopped and drained. Returns the sender (its frame counter).
pub fn run_vban_loop(
    out: &VbanOut,
    sink: &mut dyn VbanSink,
    clock: &mut dyn VbanClock,
) -> VbanSender {
    let mut sender = VbanSender::default();
    loop {
        match out.take_timeout(VBAN_IDLE_WAIT) {
            VbanTake::Block(block) => {
                sender.send_block(out, &block, sink, clock);
            }
            VbanTake::Idle => {}
            VbanTake::Stopped => break,
        }
    }
    info!(
        packets_sent = out.status().packets_sent,
        next_counter = sender.next_counter(),
        "vban output: stopped"
    );
    sender
}

/// Windows: bind one UDP socket and run [`run_vban_loop`] on its own thread
/// (`vban-output`), paced on a [`WallVbanClock`].
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn spawn_vban_thread(out: Arc<VbanOut>) {
    let spawned = std::thread::Builder::new()
        .name("vban-output".into())
        .spawn(move || {
            crate::playback::pipeline_paced::request_high_res_timer();
            let mut socket = match UdpSocket::bind(("0.0.0.0", 0)) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "vban output: binding the UDP socket failed — no VBAN");
                    return;
                }
            };
            info!(local = ?socket.local_addr().ok(), "vban output thread started");
            let mut clock = WallVbanClock::new(WallClock::system());
            run_vban_loop(&out, &mut socket, &mut clock);
        });
    if let Err(e) = spawned {
        tracing::error!(%e, "vban output: spawning the thread failed");
    }
}

/// Keep the config in step with the settings: re-read them every
/// [`VBAN_SETTINGS_POLL`], resolve DNS when they change and every
/// [`VBAN_RESOLVE_EVERY`], log what changed and every resolve failure.
#[cfg_attr(test, mutants::skip)]
pub async fn run_vban_config_task(
    pool: SqlitePool,
    out: Arc<VbanOut>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut applied: Option<VbanSettings> = None;
    let mut resolved_at: Option<Instant> = None;
    loop {
        match load_vban_settings(&pool).await {
            Ok(settings) => {
                let changed = applied.as_ref() != Some(&settings);
                if needs_resolve(changed, resolved_at.map(|t| t.elapsed())) {
                    let cfg = resolve_config(settings.clone(), out.config().targets.clone()).await;
                    for t in cfg.targets.iter().filter(|t| t.error.is_some()) {
                        warn!(
                            spec = %t.spec,
                            error = t.error.as_deref().unwrap_or_default(),
                            kept = ?t.addr,
                            "vban output: resolving a target failed"
                        );
                    }
                    if changed {
                        let resolved: Vec<_> =
                            cfg.targets.iter().map(|t| (&t.spec, t.addr)).collect();
                        info!(
                            enabled = cfg.enabled,
                            stream_name = %cfg.stream_name,
                            resolved = ?resolved,
                            active = cfg.is_active(),
                            "vban output: settings applied"
                        );
                    }
                    out.set_config(cfg);
                    applied = Some(settings);
                    resolved_at = Some(Instant::now());
                }
            }
            Err(e) => warn!(%e, "vban output: reading the settings failed"),
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(VBAN_SETTINGS_POLL) => {}
        }
    }
    info!("vban output: settings task stopped");
}

#[cfg(test)]
#[path = "vban_out_tests.rs"]
pub(crate) mod tests;
