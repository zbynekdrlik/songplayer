//! The program's VBAN audio output (#210, B2 of EPIC #174). Design record:
//! #210 comment 5846308506 (Approach 1).
//!
//! FOH (VB-Matrix on fohabl) and lv1 take the program audio as VBAN. The
//! `SP-program` sender thread (`program_output.rs`) hands each submitted
//! pair's audio block (a forwarded source block, a mixed block or the
//! standby silence) to the outputs' fan-out (`audio_out.rs`, #233), which
//! pushes it into every VBAN output's [`VbanOut::push`] right BEFORE its NDI
//! submit, so the video side of its own boundary never delays it (#210). The
//! hand-off is a bounded, never-blocking queue: over its bound
//! ([`VBAN_QUEUE_BOUND`], plus the slots a delay holds) the OLDEST block is
//! dropped and counted. A dedicated thread per output ([`run_vban_loop`])
//! encodes each block in its destination's format (`vban_packet.rs`; at
//! 48 kHz INT24, 8 packets of 200 frames) and sends packet `k` of the
//! boundary `B` at `due(B) + L + delay + offset(k)`, where L is two slots
//! ([`VBAN_SEND_LATENCY_100NS`]). It paces on its own
//! [`WallClock`](crate::playback::wallclock::WallClock), ticked
//! once per grid boundary like the program wall ([`WallVbanClock`]) — also
//! while nothing is sent, so its anchor never goes stale — and the
//! on-time packets go out one every 4.1667 ms (48 kHz INT24; #233: one every
//! 1 / (30 · packets per block) s per destination), one wait each, never as a burst.
//! A block that arrives after its first packet is due (a program fill after
//! the 3-slot grace, a real stall) sends its past-due packets back-to-back and
//! counts each as a late send. The frame counter grows by exactly 1 per packet
//! across cuts and standby.
//!
//! A fleet date step (#224 part 2): the walls relabel, so the program's
//! timeline moves only by the remainder r (≤ one slot, or a residue hold of
//! at most ~4 ms) and VBAN never sees the whole slots N. VBAN has no timecode:
//! its receiver paces by arrival, so a jump of r would still send r of audio
//! at once, and a hold would leave a gap. Its clock
//! ([`WallVbanClock::slewing`], policy [`RemainderSlew`], in `vban_clock.rs`)
//! therefore neither jumps nor stops at the follow: it owes the movement and
//! pays it back at [`VBAN_SLEW_PPM`], so every packet interval stays within
//! its spacing ± 100 ppm (4.1667 ms at 48 kHz INT24) — no burst, no gap, no
//! drop, no crossfade
//! (`slew_owed_us` on the status, signed).
//!
//! #233: one `VbanOut` per VBAN entry of the output list (`audio_out_task.rs`
//! builds it from the entry, resolves its target and spawns its thread): its
//! own format ([`VbanFormat`]: rate index, sample type, packet geometry), rate
//! converter (`vban_rate.rs`, bypassed at 48 kHz), delay (added to the send
//! latency; the queue bound grows with it, [`queue_bound`]) and ONE target.
//! The outputs task re-resolves its DNS every [`VBAN_RESOLVE_EVERY`]; a
//! target whose re-resolve fails keeps its last good address. Telemetry is
//! [`VbanStatus`], served under `outputs[i].vban` on `GET /api/v1/program`.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use sp_core::audio_outputs::{OutputEntry, VbanDest, effective_rate};
use sp_core::config::DEFAULT_VBAN_STREAM_NAME;
use tracing::{info, warn};

use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::loop_stats::percentile_ceil;
use crate::playback::program_output_timing::utc_label;
use crate::playback::vban_packet::{
    VBAN_SEND_LATENCY_100NS, VBAN_STREAM_NAME_LEN, VbanEncoder, VbanFormat, empty_packets,
    packet_send_at_in, stream_name_bytes,
};
use crate::playback::vban_rate::VbanRateConverter;
use crate::playback::vban_stall::{VbanLateEvent, VbanStallLog, VbanStallWarn};
// #224 part 2: VBAN's (and the NDI input's) wall clock and VBAN's slew live
// in `vban_clock.rs` (review round 1: this file neared the 1000-line cap).
pub use crate::playback::vban_clock::{RemainderSlew, VBAN_SLEW_PPM, WallVbanClock};

/// Queue bound: the program queue's own bound — a full program catch-up (8
/// slots) plus the block behind it, with one to spare.
pub const VBAN_QUEUE_BOUND: usize = crate::playback::program_bus::PROGRAM_QUEUE_BOUND;

/// #233: one grid slot in 100 ns (a literal: `UNITS_PER_SECOND /
/// GENLOCK_GRID_FPS`, pinned by `the_queue_holds_the_delay`).
pub const SLOT_100NS: u64 = 333_333;

/// #233: the queue bound of an output delayed by `delay_100ns`: the program
/// queue's bound plus the slots the delay holds back.
pub fn queue_bound(delay_100ns: i64) -> usize {
    VBAN_QUEUE_BOUND + (delay_100ns.max(0) as u64).div_ceil(SLOT_100NS) as usize
}

/// #233: `host:port` of a destination (the target's spec and status label).
pub fn target_spec(dest: &VbanDest) -> String {
    format!("{}:{}", dest.host, dest.port)
}

/// A packet sent more than this after its due time is a late send (2 ms).
pub const VBAN_LATE_100NS: i64 = 20_000;

/// Longest wait before a packet: 4 × L = 8 slots. #233: an output's wait
/// may be this plus its delay (`plan_wait_up_to`). A due time further ahead
/// is a clock mismatch; the thread never parks on it.
pub const VBAN_MAX_WAIT_100NS: i64 = 4 * VBAN_SEND_LATENCY_100NS;

/// The longest single sleep (#233): 7 slots, so one sleep plus an oversleep
/// of under a slot passes at most 8 boundaries, the wall's tick cap per read
/// (`BoundaryTicker`). A longer wait is slept in steps (`sleep_until`).
pub const VBAN_SLEEP_STEP_100NS: i64 = 2_333_331;

/// The sleeps one packet's wait may take (#233): the longest wait, 8 slots +
/// the longest delay, in steps of at most [`VBAN_SLEEP_STEP_100NS`].
pub const VBAN_WAIT_STEPS: usize = 10;
const _: () = assert!(
    VBAN_WAIT_STEPS as i64 * VBAN_SLEEP_STEP_100NS
        >= VBAN_MAX_WAIT_100NS + sp_core::audio_outputs::MAX_DELAY_MS as i64 * 10_000
);

/// Send intervals kept for the p99 (the last 5 s at 240 packets/s).
pub const VBAN_INTERVAL_WINDOW: usize = 1200;

/// How long the thread waits for a block before it reads its clock and checks
/// for a stop again — 3 slots, under the 8-boundary tick cap per read
/// (`BoundaryTicker`), so an idle wall still ticks once per boundary.
pub const VBAN_IDLE_WAIT: Duration = Duration::from_millis(100);
const _: () = assert!(
    VBAN_IDLE_WAIT.as_nanos() / 100
        < (crate::playback::program_output::MAX_TICKS_PER_WAKE * VBAN_SEND_LATENCY_100NS) as u128
);

/// How often DNS is re-resolved when the settings did not change.
pub const VBAN_RESOLVE_EVERY: Duration = Duration::from_secs(60);

/// A repeating warning (overflow, substitution, send error) is logged on its
/// first occurrence and then every this many.
pub const VBAN_LOG_EVERY: u64 = 1000;

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

/// Re-resolve when the settings changed (the first pass counts as a change),
/// or, while enabled, once [`VBAN_RESOLVE_EVERY`] passed since the last
/// resolve — a disabled output never re-resolves a dead target every minute.
pub fn needs_resolve(changed: bool, enabled: bool, since_last: Option<Duration>) -> bool {
    changed || (enabled && since_last.is_none_or(|d| d >= VBAN_RESOLVE_EVERY))
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
    /// #233: the config of one destination over its resolved target.
    pub fn for_dest(dest: &VbanDest, enabled: bool, targets: Vec<VbanTarget>) -> Self {
        Self::named(enabled, &dest.stream_name, targets)
    }

    fn named(enabled: bool, stream_name: &str, targets: Vec<VbanTarget>) -> Self {
        let name_bytes = stream_name_bytes(stream_name);
        let stream_name = name_bytes
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect();
        Self {
            enabled,
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
    /// Disabled, the default stream name, no target.
    fn default() -> Self {
        Self::named(false, DEFAULT_VBAN_STREAM_NAME, Vec::new())
    }
}

/// #233: resolve one destination on the blocking pool (std DNS); a failed
/// re-resolve keeps the last good address (`resolve_targets`).
pub async fn resolve_dest(dest: VbanDest, enabled: bool, previous: Vec<VbanTarget>) -> VbanConfig {
    let joined = tokio::task::spawn_blocking(move || {
        let mut resolve = system_resolve;
        let targets = resolve_targets(&[target_spec(&dest)], &previous, &mut resolve);
        VbanConfig::for_dest(&dest, enabled, targets)
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

/// `GET /api/v1/program` → `outputs[i].vban` (#233; #210's top-level `vban`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VbanStatus {
    pub enabled: bool,
    /// The VBAN thread is running (Windows; started by the outputs task).
    pub running: bool,
    pub stream_name: String,
    /// #233: blocks sent (each its packets to the resolved target).
    pub blocks_sent: u64,
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
    /// #210 part 2: the worst packet's lateness (µs, `vban_stall.rs`) over
    /// the last 14 400–28 800 packets sent: 60–120 s of sending at 48 kHz
    /// INT24 (#233: the buckets count packets, so a destination sending more
    /// packets a second covers less time). It does not age while nothing is
    /// sent.
    pub late_max_us: u64,
    /// #210 part 2: the last 32 packets sent more than 5 ms late, oldest
    /// first: `{utc_ms, late_us}`.
    pub late_events: Vec<VbanLateEvent>,
    /// p99 of the last [`VBAN_INTERVAL_WINDOW`] packet-to-packet intervals.
    pub send_interval_p99_us: u64,
    /// `nuFrame` of the last packet sent.
    pub frame_counter: u32,
    /// #224 part 2: the date-step movement VBAN's clock still owes (µs,
    /// signed): r right after a follow (negative after a residue hold), then
    /// toward 0 at [`VBAN_SLEW_PPM`]; 0 otherwise.
    pub slew_owed_us: i64,
    pub targets: Vec<VbanTargetStatus>,
}

#[derive(Clone, Debug, Default)]
struct VbanCounters {
    blocks_sent: u64,
    packets_sent: u64,
    send_errors: u64,
    blocks_dropped: u64,
    blocks_substituted: u64,
    late_sends: u64,
    frame_counter: u32,
    intervals_us: VecDeque<u64>,
    slew_owed_us: i64,
    /// #210 part 2: every sent packet's lateness (the ring, the window, the
    /// WARN's rate limit), under the same lock as the counters: one lock
    /// per packet on the real-time thread.
    stalls: VbanStallLog,
}

struct VbanQueue {
    blocks: VecDeque<ProgramBlock>,
    stop: bool,
}

/// What [`VbanOut::take_timeout`] returned.
#[derive(Debug, PartialEq)]
pub enum VbanTake {
    Block(ProgramBlock),
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
    /// Set while [`run_vban_loop`] runs.
    running: AtomicBool,
    /// #233: this destination's wire format.
    format: VbanFormat,
    /// #233: the entry's delay, added to the send latency (100 ns).
    delay_100ns: i64,
    /// #233: the queue's bound ([`queue_bound`] of the delay).
    bound: usize,
}

impl Default for VbanOut {
    fn default() -> Self {
        Self::new()
    }
}

impl VbanOut {
    /// Disabled, no targets, nothing queued; #210's format, no delay.
    pub fn new() -> Self {
        Self::for_destination(VbanFormat::PROGRAM, 0)
    }

    /// #233: an output for one destination's format, delayed by `delay_100ns`.
    pub fn for_destination(format: VbanFormat, delay_100ns: i64) -> Self {
        let bound = queue_bound(delay_100ns);
        Self {
            queue: Mutex::new(VbanQueue {
                blocks: VecDeque::with_capacity(bound + 1),
                stop: false,
            }),
            ready: Condvar::new(),
            config: Mutex::new(Arc::new(VbanConfig::default())),
            stats: Mutex::new(VbanCounters::default()),
            running: AtomicBool::new(false),
            format,
            delay_100ns,
            bound,
        }
    }

    /// #233: the output of a VBAN entry at `network_rate`.
    pub fn for_entry(entry: &OutputEntry, network_rate: u32) -> Result<Self, String> {
        let dest = entry
            .vban
            .as_ref()
            .ok_or_else(|| "not a VBAN entry".to_string())?;
        let format = VbanFormat::new(effective_rate(entry.rate, network_rate), dest.format)?;
        Ok(Self::for_destination(
            format,
            i64::from(entry.delay_ms) * 10_000,
        ))
    }

    /// #233: this destination's wire format.
    pub fn format(&self) -> VbanFormat {
        self.format
    }

    /// #233: the entry's delay (100 ns).
    pub fn delay_100ns(&self) -> i64 {
        self.delay_100ns
    }

    /// #233: the queue's bound.
    pub fn bound(&self) -> usize {
        self.bound
    }

    /// The VBAN thread is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Hand one block over. Never blocks; over the bound (#233: [`queue_bound`]
    /// of the delay) the oldest block is dropped and counted. #233: a stopped
    /// output (discarded, or shutting down) takes no more blocks.
    pub fn push(&self, block: ProgramBlock) {
        let dropped = {
            let mut q = lock(&self.queue);
            if q.stop {
                return;
            }
            q.blocks.push_back(block);
            let over = q.blocks.len() > self.bound;
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
                    bound = self.bound,
                    thread_running = self.is_running(),
                    "vban output: queue full — dropped the oldest block (the VBAN thread fell behind, or is not running)"
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

    /// #233: stop the thread now (a runtime replace or removal): the queued
    /// blocks are dropped, so a changed output (a shorter delay, another
    /// format) never sends its old schedule next to its rebuilt successor
    /// (same host, same stream name). A process shutdown drains ([`Self::stop`]).
    pub fn discard(&self) {
        let mut q = lock(&self.queue);
        q.blocks.clear();
        q.stop = true;
        drop(q);
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

    /// #233: count one sent block.
    fn record_block(&self) {
        lock(&self.stats).blocks_sent += 1;
    }

    /// Count one substituted block; returns the total.
    fn record_substituted(&self) -> u64 {
        let mut s = lock(&self.stats);
        s.blocks_substituted += 1;
        s.blocks_substituted
    }

    /// Count one sent packet and fold its lateness into the stall log
    /// (#210 part 2), under ONE lock. Returns the total send errors and the
    /// packet's WARN, if it is to be written.
    fn record_packet(&self, sent: SentPacket) -> (u64, Option<VbanStallWarn>) {
        let mut s = lock(&self.stats);
        s.packets_sent += 1;
        s.send_errors += sent.errors;
        if sent.sent_100ns - sent.planned_100ns > VBAN_LATE_100NS {
            s.late_sends += 1;
        }
        if let Some(us) = sent.interval_us {
            if s.intervals_us.len() == VBAN_INTERVAL_WINDOW {
                s.intervals_us.pop_front();
            }
            s.intervals_us.push_back(us);
        }
        s.frame_counter = sent.counter;
        let stall = s
            .stalls
            .observe(sent.planned_100ns, sent.sent_100ns, sent.sent_label_100ns);
        (s.send_errors, stall)
    }

    /// Record what VBAN's clock still owes of a date step (100 ns, #224
    /// part 2), for `slew_owed_us`.
    fn record_slew(&self, owed_100ns: i64) {
        lock(&self.stats).slew_owed_us = owed_100ns / 10;
    }

    /// The telemetry for the API. #210 part 2 (review round 2): the
    /// counters are COPIED under the lock and everything else (the p99's
    /// sort, the ring, the targets) is computed after it: the real-time VBAN
    /// thread takes that lock on every packet, and a normal-priority API
    /// worker preempted while holding it would stall the thread.
    pub fn status(&self) -> VbanStatus {
        let cfg = self.config();
        let s = lock(&self.stats).clone();
        VbanStatus {
            enabled: cfg.enabled,
            running: self.is_running(),
            stream_name: cfg.stream_name.clone(),
            blocks_sent: s.blocks_sent,
            packets_sent: s.packets_sent,
            send_errors: s.send_errors,
            blocks_dropped: s.blocks_dropped,
            blocks_substituted: s.blocks_substituted,
            late_sends: s.late_sends,
            late_max_us: s.stalls.late_max_us(),
            late_events: s.stalls.late_events(),
            send_interval_p99_us: percentile_ceil(&s.intervals_us, 99),
            frame_counter: s.frame_counter,
            slew_owed_us: s.slew_owed_us,
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

/// One sent packet, for the counters and the stall log.
struct SentPacket {
    /// Its planned instant and when it went out (VBAN's timeline, 100 ns).
    planned_100ns: i64,
    sent_100ns: i64,
    /// The send reading's fleet label (UTC, #210 part 2).
    sent_label_100ns: i64,
    interval_us: Option<u64>,
    errors: u64,
    counter: u32,
}

/// The clock the VBAN thread paces on (the program's wall domain).
pub trait VbanClock {
    /// Now, 100 ns (the internal timeline, #224 part 2).
    fn now_100ns(&mut self) -> i64;
    /// Sleep `d_100ns`.
    fn sleep_100ns(&mut self, d_100ns: i64);
    /// What the clock still owes of a date step's remainder (100 ns, #224
    /// part 2; 0 for a clock that follows its wall).
    fn slew_owed_100ns(&self) -> i64;
    /// The fleet label of this clock's reading `t_100ns` (`t + D(K_F)`,
    /// i.e. UTC), for the instant of a late packet (#210 part 2). A clock
    /// with no fleet shift (the tests') is its own label. VBAN's slewing
    /// clock reads `slew_owed` off its line, so for ~14 min after a date
    /// step the label is off UTC by that much: before it after a forward
    /// follow (≤ one slot), after it after a residue hold (≤ ~4 ms).
    fn label_100ns(&self, t_100ns: i64) -> i64 {
        t_100ns
    }
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

/// How long to wait from `now_100ns` for a packet due at `at_100ns`: never
/// negative, never more than [`VBAN_MAX_WAIT_100NS`].
pub fn plan_wait_100ns(now_100ns: i64, at_100ns: i64) -> i64 {
    plan_wait_up_to(now_100ns, at_100ns, VBAN_MAX_WAIT_100NS)
}

/// #233: the wait for a packet due at `at_100ns`, never negative, never more
/// than `max_100ns` (an output's [`VBAN_MAX_WAIT_100NS`] + its delay: a due
/// time further ahead is a clock mismatch).
pub fn plan_wait_up_to(now_100ns: i64, at_100ns: i64, max_100ns: i64) -> i64 {
    (at_100ns - now_100ns).clamp(0, max_100ns)
}

/// #233: sleep from `now_100ns` until `end_100ns` in sleeps of at most
/// [`VBAN_SLEEP_STEP_100NS`] (under the wall's tick cap per read), reading the clock
/// between two of them (each read ticks the wall) and planning the next from
/// that read, so an oversleep never adds up. The read after the last sleep is
/// the caller's.
pub fn sleep_until(clock: &mut dyn VbanClock, now_100ns: i64, end_100ns: i64) {
    let mut now = now_100ns;
    for _ in 0..VBAN_WAIT_STEPS {
        let left = end_100ns - now;
        if left <= 0 {
            return;
        }
        let step = left.min(VBAN_SLEEP_STEP_100NS);
        clock.sleep_100ns(step);
        if step == left {
            return;
        }
        now = clock.now_100ns();
    }
}

/// Packet-to-packet interval in µs (0 on a backward clock read).
pub fn interval_us(prev_100ns: i64, now_100ns: i64) -> u64 {
    (now_100ns - prev_100ns).max(0) as u64 / 10
}

/// The sending side of one VBAN stream: its format, its send latency (L +
/// the delay) and wait cap, the rate converter (#233), the encoder (frame
/// counter), the reusable packet buffer and the last send instant.
pub struct VbanSender {
    format: VbanFormat,
    latency_100ns: i64,
    max_wait_100ns: i64,
    converter: VbanRateConverter,
    encoder: VbanEncoder,
    packets: Vec<u8>,
    last_send_100ns: Option<i64>,
}

impl Default for VbanSender {
    /// #210's sender: the `PROGRAM` format, no delay.
    fn default() -> Self {
        Self::new(VbanFormat::PROGRAM, 0)
    }
}

impl VbanSender {
    fn new(format: VbanFormat, delay_100ns: i64) -> Self {
        let converter = VbanRateConverter::new(format.rate_hz());
        if let Some(why) = converter.failed() {
            warn!(
                rate = format.rate_hz(),
                why,
                "vban output: the rate converter could not be built — the output sends silence"
            );
        }
        Self {
            format,
            latency_100ns: VBAN_SEND_LATENCY_100NS + delay_100ns,
            max_wait_100ns: VBAN_MAX_WAIT_100NS + delay_100ns,
            converter,
            encoder: VbanEncoder::default(),
            packets: empty_packets(format),
            last_send_100ns: None,
        }
    }

    /// #233: the sender of `out`'s destination.
    pub fn for_out(out: &VbanOut) -> Self {
        Self::new(out.format(), out.delay_100ns())
    }

    /// The `nuFrame` the next packet carries.
    pub fn next_counter(&self) -> u32 {
        self.encoder.next_counter()
    }

    /// Send one block's packets (8 at 48 kHz INT24, #233: its destination's
    /// count) on their schedule to every resolved target, or nothing while
    /// the output is disabled or has no resolved target. Returns the packets
    /// sent.
    pub fn send_block(
        &mut self,
        out: &VbanOut,
        block: &ProgramBlock,
        sink: &mut dyn VbanSink,
        clock: &mut dyn VbanClock,
    ) -> usize {
        if block.substituted {
            let n = out.record_substituted();
            if should_log(n) {
                warn!(
                    blocks_substituted = n,
                    due_100ns = crate::playback::fleet_shift::wire_100ns(block.due_100ns),
                    "vban output: a program pair's audio was not one program block — sent silence"
                );
            }
        }
        let cfg = out.config();
        if !cfg.is_active() {
            self.last_send_100ns = None; // a disabled gap is not a send interval
            return 0;
        }
        let samples = self.converter.convert(block.samples.as_deref());
        let first = self.encoder.next_counter();
        self.encoder
            .encode_into(self.format, &cfg.name_bytes, samples, &mut self.packets);
        let packet_len = self.format.packet_len();
        for (k, packet) in self.packets.chunks_exact(packet_len).enumerate() {
            let at = packet_send_at_in(self.format, block.due_100ns, self.latency_100ns, k);
            let now = clock.now_100ns();
            let wait = plan_wait_up_to(now, at, self.max_wait_100ns);
            sleep_until(clock, now, now + wait);
            let sent_at = clock.now_100ns();
            let mut errors = 0;
            let mut last_error = None;
            for addr in cfg.targets.iter().filter_map(|t| t.addr) {
                if let Err(e) = sink.send_packet(packet, addr) {
                    errors += 1;
                    last_error = Some((addr, e));
                }
            }
            let (total_errors, stall) = out.record_packet(SentPacket {
                planned_100ns: at,
                sent_100ns: sent_at,
                sent_label_100ns: clock.label_100ns(sent_at),
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
            if let Some(stall) = stall {
                warn_late_packet(&stall, k, wait);
            }
        }
        out.record_block();
        self.format.packets_per_block()
    }
}

/// #210 part 2: the WARN of a packet sent more than 10 ms after its planned
/// instant: when (UTC, to line up with a dev1 capture), how late, which
/// packet of its block, the wait before it and how many such packets the
/// rate limit skipped since the WARN before it. `waited_us` > 0: the thread
/// overslept that wait. 0: it came to the packet already late — a stall
/// before it (taking the block, or right after the packet before), a block
/// handed over late, or the packet before sent late. The decision is
/// `VbanStallLog::observe`'s (tested); logging only.
#[cfg_attr(test, mutants::skip)]
fn warn_late_packet(stall: &VbanStallWarn, packet: usize, wait_100ns: i64) {
    warn!(
        utc = %utc_label(stall.event.utc_ms * 10_000),
        late_us = stall.event.late_us,
        packet,
        waited_us = crate::playback::wallclock::to_us(wait_100ns),
        suppressed = stall.suppressed,
        "vban output: a packet went out more than 10 ms after its planned instant"
    );
}

/// The VBAN thread body: send every queued block on its schedule until the
/// output is stopped and drained. Returns the sender (its frame counter).
pub fn run_vban_loop(
    out: &VbanOut,
    sink: &mut dyn VbanSink,
    clock: &mut dyn VbanClock,
) -> VbanSender {
    let mut sender = VbanSender::for_out(out);
    out.running.store(true, Ordering::SeqCst);
    loop {
        let take = out.take_timeout(VBAN_IDLE_WAIT);
        // Read (= tick) the wall on every pass, also while nothing is sent
        // (disabled, no target, idle), so its anchor follows UTC like the
        // program wall and the first packets after enabling are on schedule.
        clock.now_100ns();
        out.record_slew(clock.slew_owed_100ns());
        match take {
            VbanTake::Block(block) => {
                sender.send_block(out, &block, sink, clock);
            }
            VbanTake::Idle => {}
            VbanTake::Stopped => break,
        }
    }
    out.running.store(false, Ordering::SeqCst);
    info!(
        packets_sent = out.status().packets_sent,
        next_counter = sender.next_counter(),
        "vban output: stopped"
    );
    sender
}

/// Windows: bind one UDP socket and run [`run_vban_loop`] on its own thread
/// (`vban-output`, one per VBAN entry #233, `id` = the entry's), paced on a
/// [`WallVbanClock`]. #210 part 2: the thread is an MMCSS "Pro Audio" thread
/// at `AVRT_PRIORITY_HIGH` for its whole life (`mmcss::join_pro_audio`;
/// `THREAD_PRIORITY_TIME_CRITICAL` if refused).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn spawn_vban_thread(out: Arc<VbanOut>, id: String) {
    let spawned = std::thread::Builder::new()
        .name("vban-output".into())
        .spawn(move || {
            crate::playback::pipeline_paced::request_high_res_timer();
            let _mmcss = crate::playback::mmcss::join_pro_audio("vban-output");
            let mut socket = match UdpSocket::bind(("0.0.0.0", 0)) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(%e, "vban output: binding the UDP socket failed — no VBAN");
                    return;
                }
            };
            let format = out.format();
            info!(
                id = %id,
                rate = format.rate_hz(),
                format = format.sample().as_str(),
                delay_100ns = out.delay_100ns(),
                local = ?socket.local_addr().ok(),
                "vban output thread started"
            );
            // #224 part 2: SlewRemainder — a date step never bursts VBAN.
            let wall = crate::playback::wallclock::WallClock::system();
            let mut clock = WallVbanClock::slewing(wall);
            run_vban_loop(&out, &mut socket, &mut clock);
        });
    if let Err(e) = spawned {
        tracing::error!(%e, "vban output: spawning the thread failed");
    }
}

#[cfg(test)]
#[path = "vban_out_tests.rs"]
pub(crate) mod tests;
#[cfg(test)]
#[path = "vban_out_tests_dest.rs"]
mod tests_dest;
