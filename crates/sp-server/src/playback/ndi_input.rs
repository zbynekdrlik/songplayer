//! #212 (B3 of EPIC #174): the NDI input "OBS manuál" — one received NDI source
//! on SongPlayer's genlock grid, offered to the program bus like a playlist
//! output. Design record: #212 comment 5847592877 (Approach 1).
//!
//! cg OBS stays only for the manual scenes SongPlayer cannot render yet; its
//! manual mix comes in over NDI and is CUT to `SP-program` like any source.
//!
//! - **Receive.** An `sp_ndi::NdiFrameSync` (receiver + the SDK's FrameSync,
//!   `NDIlib_recv_color_format_fastest` = UYVY / UYVA, progressive) is pulled
//!   on OUR clock: FrameSync repeats or drops video and resamples audio so the
//!   caller's cadence is the master (the SDK's time-base corrector, no
//!   hand-rolled jitter buffer).
//! - **One pair per boundary.** [`run_input_loop`] ticks on the process genlock
//!   grid (the pacer's exact-rational boundaries, the `SP-program` wall domain).
//!   At each boundary B, [`NdiInput::service`] captures one video frame and
//!   `capture_audio(48000, 2, 1600)` (one 33.3 ms block), converts UYVY→NV12
//!   ([`uyvy_to_nv12`], into a `frame_pool` buffer — recycled on the last
//!   `SharedFrame` drop, so the steady state allocates nothing), builds a
//!   `SubmitJob` stamped at B and offers it under [`PROGRAM_INPUT_ID`] (`-1`).
//! - **Always on time.** With no source, a disconnected one, no video yet or an
//!   unsupported FourCC the input offers its standby pair (the NV12 black + one
//!   silent block), so it owns its boundary and the bus never fills for it.
//! - **Only a candidate pays.** Every boundary is `touch`ed and captured (the
//!   FrameSync keeps tracking our cadence and the counters stay live), but the
//!   conversion and the job are built only while the input can own a program
//!   boundary. A FrameSync repeat of an already-converted frame is an `Arc` bump.
//! - **Settings** (`ndi_input_enabled`, `ndi_input_source`) are re-read every
//!   [`INPUT_SETTINGS_POLL`] by [`run_input_config_task`]; a change reconnects
//!   on the input thread. While enabled and not connected, the task lists the
//!   visible NDI sources every [`INPUT_FIND_EVERY`] (logged + served).
//! - **Telemetry** is [`NdiInputStatus`], served under `input` on
//!   `GET /api/v1/program`. Connect, disconnect, source change and format change
//!   are logged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use sp_core::config::{
    PROGRAM_INPUT_ID, PROGRAM_INPUT_LABEL, SETTING_NDI_INPUT_ENABLED, SETTING_NDI_INPUT_SOURCE,
};
use sp_core::genlock::{
    GENLOCK_GRID_FPS, GENLOCK_MAX_CATCHUP_INTERVALS, UNITS_PER_SECOND, floor_boundary_100ns,
    lag_slots_100ns, strict_next_boundary_100ns,
};
use sp_ndi::receive::{FOURCC_UYVA, FOURCC_UYVY};
use sp_ndi::{AudioFrame, NdiFrameSync, NdiReceiveBackend};
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::ProgramBus;
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::VbanClock;

/// The receiver's own NDI name (what the source sees connected).
pub const INPUT_RECV_NAME: &str = "SongPlayer program input";

/// The block the FrameSync is asked for on every boundary: 48 kHz stereo, one
/// grid slot (1600 samples) — exactly the program's audio format.
pub const INPUT_AUDIO_RATE_HZ: i32 = 48_000;
pub const INPUT_AUDIO_CHANNELS: i32 = 2;
pub const INPUT_AUDIO_SAMPLES: i32 = 1_600;

/// How often the settings are re-read (a dashboard save applies within this).
pub const INPUT_SETTINGS_POLL: Duration = Duration::from_secs(5);

/// How often the visible NDI sources are listed while enabled + not connected.
pub const INPUT_FIND_EVERY: Duration = Duration::from_secs(30);

/// How long one source listing waits for the network (on the blocking pool).
pub const INPUT_FIND_WAIT_MS: u32 = 1_000;

/// At most this many visible source names are kept for the API.
pub const INPUT_MAX_VISIBLE: usize = 16;

/// A failed receiver creation is retried this long after (5 s).
pub const INPUT_RECONNECT_100NS: i64 = 5 * UNITS_PER_SECOND;

/// A normal wait is at most one grid slot; a boundary further ahead than two
/// slots means the clock stepped backward — re-latch onto the grid.
pub const INPUT_RELATCH_100NS: i64 = 2 * (UNITS_PER_SECOND / GENLOCK_GRID_FPS);

// The FrameSync is asked for exactly one program block per boundary.
const _: () = assert!(
    INPUT_AUDIO_RATE_HZ as i64 == crate::playback::vban_packet::VBAN_SAMPLE_RATE_HZ
        && INPUT_AUDIO_CHANNELS as usize == crate::playback::vban_packet::VBAN_CHANNELS
        && (INPUT_AUDIO_SAMPLES as i64) * GENLOCK_GRID_FPS == INPUT_AUDIO_RATE_HZ as i64
);

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The two input settings as stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputSettings {
    pub enabled: bool,
    /// The full NDI name, `"MACHINE (stream)"` (trimmed).
    pub source: String,
}

impl InputSettings {
    /// Enabled with a source to receive.
    pub fn active(&self) -> bool {
        self.enabled && !self.source.is_empty()
    }
}

/// Read the input settings: `ndi_input_enabled == "true"` enables; the source
/// is trimmed, absent = empty.
pub async fn load_input_settings(pool: &SqlitePool) -> Result<InputSettings, sqlx::Error> {
    use crate::db::models::get_setting;
    let enabled = get_setting(pool, SETTING_NDI_INPUT_ENABLED)
        .await?
        .is_some_and(|v| v.trim() == "true");
    let source = get_setting(pool, SETTING_NDI_INPUT_SOURCE)
        .await?
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    Ok(InputSettings { enabled, source })
}

/// List the visible NDI sources now? Only while enabled with a source and not
/// connected (the operator is looking for the right name), and at most every
/// [`INPUT_FIND_EVERY`].
pub fn needs_find(active: bool, connected: bool, since_last: Option<Duration>) -> bool {
    active && !connected && since_last.is_none_or(|d| d >= INPUT_FIND_EVERY)
}

// ---------------------------------------------------------------------------
// Shared state + telemetry
// ---------------------------------------------------------------------------

/// The received picture's format, as logged on a format change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoFormat {
    pub four_cc: u32,
    pub width: i32,
    pub height: i32,
    pub frame_rate_n: i32,
    pub frame_rate_d: i32,
}

impl VideoFormat {
    /// The FourCC as its four ASCII characters (`"UYVY"`), `?` for anything
    /// unprintable.
    pub fn four_cc_str(&self) -> String {
        self.four_cc
            .to_le_bytes()
            .iter()
            .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
            .collect()
    }

    /// UYVY or UYVA (a UYVY plane + an alpha plane) with even, non-zero
    /// dimensions — what [`uyvy_to_nv12`] converts.
    pub fn is_supported(&self) -> bool {
        (self.four_cc == FOURCC_UYVY || self.four_cc == FOURCC_UYVA)
            && self.width > 0
            && self.height > 0
            && self.width % 2 == 0
            && self.height % 2 == 0
    }
}

#[derive(Clone, Debug, Default)]
struct InputCounters {
    boundaries: u64,
    frames_received: u64,
    video_repeats: u64,
    video_drops: u64,
    no_source_boundaries: u64,
    unsupported_boundaries: u64,
    resyncs: u64,
    relatches: u64,
    connected: bool,
    audio_queue_depth: i32,
    format: Option<VideoFormat>,
}

/// `GET /api/v1/program` → `input`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NdiInputStatus {
    /// The program-bus source id ([`PROGRAM_INPUT_ID`]).
    pub id: i64,
    pub label: &'static str,
    pub enabled: bool,
    /// The input thread is running (Windows, with the NDI SDK).
    pub running: bool,
    /// The receiver is connected to the source.
    pub connected: bool,
    /// The configured full NDI name.
    pub source: String,
    /// Its stream part (`"MACHINE (stream)"` → `"stream"`).
    pub stream: String,
    /// Distinct frames received (a FrameSync repeat does not count).
    pub frames_received: u64,
    /// Boundaries that got the same frame again (a slower source).
    pub video_repeats: u64,
    /// Source frames skipped between two received ones (a faster source).
    pub video_drops: u64,
    /// Boundaries offered as standby: no receiver, disconnected, no video yet.
    pub no_source_boundaries: u64,
    /// Boundaries offered as standby because the FourCC / size is unsupported.
    pub unsupported_boundaries: u64,
    /// Grid boundaries the input serviced.
    pub boundaries: u64,
    /// Runs of more than 8 missed boundaries skipped (the thread stalled).
    pub resyncs: u64,
    /// Backward clock steps re-latched onto the grid.
    pub relatches: u64,
    /// Audio samples waiting in the FrameSync at the last boundary.
    pub audio_queue_depth: i32,
    /// `"1920x1080"` of the last received frame.
    pub last_frame_size: Option<String>,
    /// Its FourCC (`"UYVY"`).
    pub format: Option<String>,
    /// Its frame rate (`"30000/1001"`).
    pub frame_rate: Option<String>,
    /// NDI source names seen on the network while not connected.
    pub visible_sources: Vec<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The state the input thread, the settings task and the API share (reached
/// through `ProgramBus::input()`). Every method holds a lock for µs only.
#[derive(Default)]
pub struct NdiInputShared {
    settings: Mutex<InputSettings>,
    counters: Mutex<InputCounters>,
    visible: Mutex<Vec<String>>,
    running: AtomicBool,
    stop: AtomicBool,
}

impl NdiInputShared {
    /// The settings the input thread applies.
    pub fn settings(&self) -> InputSettings {
        lock(&self.settings).clone()
    }

    /// Replace the settings (the settings task).
    pub fn set_settings(&self, settings: InputSettings) {
        *lock(&self.settings) = settings;
    }

    /// Replace the visible source list (at most [`INPUT_MAX_VISIBLE`] kept).
    pub fn set_visible(&self, mut names: Vec<String>) {
        names.truncate(INPUT_MAX_VISIBLE);
        *lock(&self.visible) = names;
    }

    /// The receiver is connected (last boundary).
    pub fn is_connected(&self) -> bool {
        lock(&self.counters).connected
    }

    /// Stop the input thread (process shutdown).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// The telemetry for the API, reporting `settings` as configured (the API
    /// passes the stored settings, so a save shows at once).
    pub fn status(&self, settings: &InputSettings) -> NdiInputStatus {
        let c = lock(&self.counters).clone();
        let fmt = c.format;
        NdiInputStatus {
            id: PROGRAM_INPUT_ID,
            label: PROGRAM_INPUT_LABEL,
            enabled: settings.enabled,
            running: self.is_running(),
            connected: c.connected,
            source: settings.source.clone(),
            stream: crate::obs::ndi_discovery::extract_ndi_stream_name(&settings.source)
                .to_string(),
            frames_received: c.frames_received,
            video_repeats: c.video_repeats,
            video_drops: c.video_drops,
            no_source_boundaries: c.no_source_boundaries,
            unsupported_boundaries: c.unsupported_boundaries,
            boundaries: c.boundaries,
            resyncs: c.resyncs,
            relatches: c.relatches,
            audio_queue_depth: c.audio_queue_depth,
            last_frame_size: fmt.map(|f| format!("{}x{}", f.width, f.height)),
            format: fmt.map(|f| f.four_cc_str()),
            frame_rate: fmt.map(|f| format!("{}/{}", f.frame_rate_n, f.frame_rate_d)),
            visible_sources: lock(&self.visible).clone(),
        }
    }

    fn counters(&self) -> MutexGuard<'_, InputCounters> {
        lock(&self.counters)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Convert one UYVY (4:2:2, `U0 Y0 V0 Y1` per two pixels) picture into NV12
/// (4:2:0: the `width × height` luma plane, then `height / 2` rows of
/// interleaved `U V`), written into `out` (cleared first, its capacity kept).
/// Chroma is subsampled vertically by averaging each row pair, rounding half
/// up. Rows are `stride` bytes apart (padding ignored). Returns `false` (with
/// `out` cleared) for a zero or odd size, a stride under `2 × width`, or a
/// `src` shorter than `stride × height`.
pub fn uyvy_to_nv12(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    out: &mut Vec<u8>,
) -> bool {
    out.clear();
    let row = width * 2;
    if width == 0
        || height == 0
        || width % 2 == 1
        || height % 2 == 1
        || stride < row
        || src.len() < stride * height
    {
        return false;
    }
    for y in 0..height {
        let line = &src[y * stride..y * stride + row];
        out.extend(line.iter().skip(1).step_by(2));
    }
    for pair in 0..height / 2 {
        let top = &src[2 * pair * stride..2 * pair * stride + row];
        let bottom = &src[(2 * pair + 1) * stride..(2 * pair + 1) * stride + row];
        for (a, b) in top.chunks_exact(4).zip(bottom.chunks_exact(4)) {
            out.push(avg(a[0], b[0]));
            out.push(avg(a[2], b[2]));
        }
    }
    true
}

/// Added before halving a chroma pair's sum: the mean rounds half UP.
const CHROMA_ROUND: u16 = 1;

/// The rounded mean of two chroma samples.
fn avg(a: u8, b: u8) -> u8 {
    ((u16::from(a) + u16::from(b) + CHROMA_ROUND) / 2) as u8
}

/// Source frames that were skipped between two received frames `prev_tc` and
/// `tc` (100 ns) of a `n / d` fps source: the rounded frame distance minus one,
/// never negative. A non-positive `d` (no rate) counts nothing.
pub fn skipped_frames(prev_tc: i64, tc: i64, n: i32, d: i32) -> u64 {
    if d <= 0 {
        return 0;
    }
    let den = i128::from(UNITS_PER_SECOND) * i128::from(d);
    let frames = (i128::from(tc - prev_tc) * i128::from(n) + den / 2) / den;
    (frames - 1).max(0) as u64
}

/// One step of the input's grid loop at `now_100ns`, `last_100ns` being the
/// last boundary serviced (or the grid floor it started from).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridStep {
    /// Sleep this long (100 ns) — the next boundary is not reached yet.
    Wait(i64),
    /// The next boundary is more than [`INPUT_RELATCH_100NS`] ahead: the clock
    /// stepped backward, start over from the current grid floor.
    Relatch,
    /// Service `boundary`; `resync` = more than 8 boundaries were missed and
    /// skipped (never a catch-up burst of old boundaries).
    Service { boundary: i64, resync: bool },
}

/// The pacer's grid rule for the input thread: the boundary after the last one
/// serviced, caught up one by one (up to 8 behind), resynced beyond that.
pub fn grid_step(now_100ns: i64, last_100ns: i64) -> GridStep {
    let target = strict_next_boundary_100ns(last_100ns, GENLOCK_GRID_FPS);
    let wait = target - now_100ns;
    if wait > INPUT_RELATCH_100NS {
        return GridStep::Relatch;
    }
    if wait > 0 {
        return GridStep::Wait(wait);
    }
    let floor = floor_boundary_100ns(now_100ns, GENLOCK_GRID_FPS);
    if lag_slots_100ns(target, floor, GENLOCK_GRID_FPS) > GENLOCK_MAX_CATCHUP_INTERVALS {
        return GridStep::Service {
            boundary: floor,
            resync: true,
        };
    }
    GridStep::Service {
        boundary: target,
        resync: false,
    }
}

// ---------------------------------------------------------------------------
// The input
// ---------------------------------------------------------------------------

/// The last received frame: its identity (a FrameSync repeat returns the same
/// timecode + buffer) and, once converted, its NV12 picture.
struct LastFrame {
    timecode: i64,
    data: usize,
    nv12: Option<SharedFrame>,
}

/// What one boundary's capture produced.
enum Captured {
    /// Offer the standby pair.
    Standby,
    /// Offer this picture + audio block.
    Picture(SharedFrame, u32, u32, Vec<f32>),
    /// Not a program candidate: captured, nothing to offer.
    Skipped,
}

/// The NDI input's boundary service (see the module doc). Owned by the input
/// thread.
pub struct NdiInput {
    backend: Option<Arc<dyn NdiReceiveBackend>>,
    shared: Arc<NdiInputShared>,
    sync: Option<NdiFrameSync>,
    applied: InputSettings,
    retry_at: i64,
    connected: bool,
    last: Option<LastFrame>,
    standby: SharedFrame,
    standby_w: u32,
    standby_h: u32,
}

impl NdiInput {
    /// An input receiving through `backend` (`None` = no NDI SDK: standby
    /// only). The standby black is `standby_w × standby_h` NV12.
    pub fn new(
        backend: Option<Arc<dyn NdiReceiveBackend>>,
        shared: Arc<NdiInputShared>,
        standby_w: u32,
        standby_h: u32,
    ) -> Self {
        let y = standby_w as usize * standby_h as usize;
        let mut black = vec![16u8; y];
        black.resize(y + y / 2, 128);
        Self {
            backend,
            shared,
            sync: None,
            applied: InputSettings::default(),
            retry_at: i64::MIN,
            connected: false,
            last: None,
            standby: SharedFrame::new(black),
            standby_w,
            standby_h,
        }
    }

    /// The shared state (the loop's stop flag + counters).
    pub fn shared(&self) -> &Arc<NdiInputShared> {
        &self.shared
    }

    /// Service boundary `boundary_100ns`: apply a settings change, capture one
    /// video frame + one audio block, and offer the pair (or the standby pair)
    /// to `bus` under [`PROGRAM_INPUT_ID`]. A disabled input does nothing — it
    /// is not a program source then. The audio is stamped `audio_now_100ns`
    /// (the raw wall clock at the emit, §6).
    pub fn service(&mut self, boundary_100ns: i64, audio_now_100ns: i64, bus: &ProgramBus) {
        self.apply_settings(boundary_100ns);
        if !self.applied.active() {
            return;
        }
        let candidate = bus.touch(PROGRAM_INPUT_ID, boundary_100ns);
        let (w, h, stride, video, samples) = match self.capture(candidate) {
            Captured::Skipped => return,
            Captured::Picture(frame, w, h, samples) => (w, h, w, frame, samples),
            Captured::Standby => {
                if !candidate {
                    return;
                }
                let (w, h) = (self.standby_w, self.standby_h);
                let silence = vec![0.0; (INPUT_AUDIO_CHANNELS * INPUT_AUDIO_SAMPLES) as usize];
                (w, h, w, self.standby.clone(), silence)
            }
        };
        let job = SubmitJob {
            width: w,
            height: h,
            stride,
            video,
            audio: vec![AudioFrame {
                data: samples,
                channels: INPUT_AUDIO_CHANNELS as u32,
                sample_rate: INPUT_AUDIO_RATE_HZ as u32,
                timecode_100ns: None,
            }],
            video_tc_100ns: boundary_100ns,
            audio_tc_100ns: audio_now_100ns,
        };
        bus.offer(PROGRAM_INPUT_ID, job);
    }

    /// Reconnect when the settings changed, or retry a failed receiver after
    /// [`INPUT_RECONNECT_100NS`].
    fn apply_settings(&mut self, boundary_100ns: i64) {
        let want = self.shared.settings();
        if want != self.applied {
            info!(
                enabled = want.enabled,
                source = %want.source,
                previous = %self.applied.source,
                "ndi input: settings changed — (re)connecting"
            );
            self.disconnect();
            self.applied = want;
            self.retry_at = i64::MIN;
        }
        if self.applied.active() && self.sync.is_none() && boundary_100ns >= self.retry_at {
            self.connect(boundary_100ns);
        }
    }

    fn connect(&mut self, boundary_100ns: i64) {
        let Some(backend) = self.backend.clone() else {
            warn!("ndi input: no NDI SDK — the input offers its standby pair");
            self.retry_at = i64::MAX; // never retried without an SDK
            return;
        };
        match NdiFrameSync::connect(backend, &self.applied.source, INPUT_RECV_NAME) {
            Ok(sync) => {
                info!(source = %self.applied.source, "ndi input: receiver + FrameSync created");
                self.sync = Some(sync);
            }
            Err(e) => {
                warn!(%e, source = %self.applied.source, "ndi input: creating the receiver failed — retry in 5 s");
                self.retry_at = boundary_100ns + INPUT_RECONNECT_100NS;
            }
        }
    }

    /// Drop the receiver (FrameSync first) and forget the last frame.
    fn disconnect(&mut self) {
        if self.sync.take().is_some() {
            info!(source = %self.applied.source, "ndi input: receiver closed");
        }
        self.set_connected(false);
        self.last = None;
    }

    fn set_connected(&mut self, connected: bool) {
        if connected != self.connected {
            if connected {
                info!(source = %self.applied.source, "ndi input: source connected");
            } else {
                warn!(source = %self.applied.source, "ndi input: source disconnected — standby pair");
            }
            self.connected = connected;
            self.shared.counters().connected = connected;
        }
    }

    /// Capture this boundary's frame + audio and account for it.
    fn capture(&mut self, candidate: bool) -> Captured {
        self.shared.counters().boundaries += 1;
        let connected = self.sync.as_ref().is_some_and(|s| s.connections() > 0);
        self.set_connected(connected);
        let Some(sync) = self.sync.as_ref().filter(|_| connected) else {
            self.shared.counters().no_source_boundaries += 1;
            return Captured::Standby;
        };
        let video = sync.capture_video();
        let audio = sync.capture_audio(
            INPUT_AUDIO_RATE_HZ,
            INPUT_AUDIO_CHANNELS,
            INPUT_AUDIO_SAMPLES,
        );
        let samples =
            audio.interleaved(INPUT_AUDIO_CHANNELS as usize, INPUT_AUDIO_SAMPLES as usize);
        drop(audio);
        self.shared.counters().audio_queue_depth = sync.audio_queue_depth();
        let f = video.frame();
        let format = VideoFormat {
            four_cc: f.four_cc,
            width: f.xres,
            height: f.yres,
            frame_rate_n: f.frame_rate_n,
            frame_rate_d: f.frame_rate_d,
        };
        let (timecode, data) = (f.timecode, f.p_data as usize);
        let stride = f.line_stride_in_bytes.max(0) as usize;
        let Some(plane) = video.first_plane() else {
            self.shared.counters().no_source_boundaries += 1;
            return Captured::Standby;
        };
        let changed = note_format(&self.shared, &self.applied.source, format);
        if !format.is_supported() {
            self.last = None;
            self.shared.counters().unsupported_boundaries += 1;
            return Captured::Standby;
        }
        let repeat = !changed
            && self
                .last
                .as_ref()
                .is_some_and(|l| l.timecode == timecode && l.data == data);
        {
            let mut c = self.shared.counters();
            if repeat {
                c.video_repeats += 1;
            } else {
                c.frames_received += 1;
                if !changed && let Some(l) = &self.last {
                    c.video_drops += skipped_frames(
                        l.timecode,
                        timecode,
                        format.frame_rate_n,
                        format.frame_rate_d,
                    );
                }
            }
        }
        if !repeat {
            self.last = Some(LastFrame {
                timecode,
                data,
                nv12: None,
            });
        }
        if !candidate {
            return Captured::Skipped;
        }
        let (w, h) = (format.width as u32, format.height as u32);
        if let Some(frame) = self.last.as_ref().and_then(|l| l.nv12.clone()) {
            return Captured::Picture(frame, w, h, samples); // a repeat: an Arc bump
        }
        let (wu, hu) = (w as usize, h as usize);
        let mut buf = sp_decoder::frame_pool::take(wu * hu * 3 / 2);
        if !uyvy_to_nv12(plane, wu, hu, stride, &mut buf) {
            self.last = None;
            self.shared.counters().unsupported_boundaries += 1;
            return Captured::Standby;
        }
        let frame = SharedFrame::new(buf);
        if let Some(l) = self.last.as_mut() {
            l.nv12 = Some(frame.clone());
        }
        Captured::Picture(frame, w, h, samples)
    }
}

/// Record `format` as the input's current one; logs it and returns `true` when
/// it differs from the last one seen.
fn note_format(shared: &NdiInputShared, source: &str, format: VideoFormat) -> bool {
    let mut c = shared.counters();
    if c.format == Some(format) {
        return false;
    }
    info!(
        source,
        four_cc = %format.four_cc_str(),
        width = format.width,
        height = format.height,
        frame_rate = %format!("{}/{}", format.frame_rate_n, format.frame_rate_d),
        supported = format.is_supported(),
        "ndi input: video format"
    );
    c.format = Some(format);
    true
}

// ---------------------------------------------------------------------------
// The grid loop + its thread, the settings task
// ---------------------------------------------------------------------------

/// The input thread body: service every grid boundary on `clock` (the program
/// wall domain, ticked once per boundary like the pacer walls) until the
/// shared stop flag is set.
pub fn run_input_loop(input: &mut NdiInput, bus: &ProgramBus, clock: &mut dyn VbanClock) {
    let shared = input.shared().clone();
    shared.running.store(true, Ordering::SeqCst);
    let mut last: Option<i64> = None;
    while !shared.is_stopped() {
        let now = clock.now_100ns();
        let from = *last.get_or_insert_with(|| floor_boundary_100ns(now, GENLOCK_GRID_FPS));
        match grid_step(now, from) {
            GridStep::Wait(d) => clock.sleep_100ns(d),
            GridStep::Relatch => {
                shared.counters().relatches += 1;
                warn!(
                    now_100ns = now,
                    last_100ns = from,
                    "ndi input: clock stepped back — re-latching the grid"
                );
                last = None;
            }
            GridStep::Service { boundary, resync } => {
                if resync {
                    shared.counters().resyncs += 1;
                    warn!(
                        from_100ns = from,
                        boundary_100ns = boundary,
                        "ndi input: > 8 boundaries missed — resync"
                    );
                }
                input.service(boundary, clock.now_100ns(), bus);
                last = Some(boundary);
            }
        }
    }
    input.disconnect();
    shared.running.store(false, Ordering::SeqCst);
    info!("ndi input: stopped");
}

/// Start the NDI input: the settings task (every platform, so the API's
/// `visible_sources` / settings flow is the same) and, on Windows with the NDI
/// SDK, the grid thread.
#[cfg_attr(test, mutants::skip)]
pub fn start_ndi_input(
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    receive: Option<Arc<dyn NdiReceiveBackend>>,
    shutdown: &broadcast::Sender<()>,
) {
    tokio::spawn(run_input_config_task(
        pool,
        bus.input().clone(),
        receive.clone(),
        shutdown.subscribe(),
    ));
    #[cfg(windows)]
    spawn_input_thread(receive, bus);
    #[cfg(not(windows))]
    let _ = (receive, bus);
}

/// Windows: run [`run_input_loop`] on its own thread (`ndi-input`), paced on a
/// `WallVbanClock` (the program wall domain).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn spawn_input_thread(receive: Option<Arc<dyn NdiReceiveBackend>>, bus: Arc<ProgramBus>) {
    use crate::playback::program_output::{PROGRAM_STANDBY_H, PROGRAM_STANDBY_W};
    use crate::playback::vban_out::WallVbanClock;
    use crate::playback::wallclock::WallClock;
    let spawned = std::thread::Builder::new()
        .name("ndi-input".into())
        .spawn(move || {
            crate::playback::pipeline_paced::request_high_res_timer();
            info!(has_sdk = receive.is_some(), "ndi input thread started");
            let shared = bus.input().clone();
            let mut input = NdiInput::new(receive, shared, PROGRAM_STANDBY_W, PROGRAM_STANDBY_H);
            let mut clock = WallVbanClock::new(WallClock::system());
            run_input_loop(&mut input, &bus, &mut clock);
        });
    if let Err(e) = spawned {
        tracing::error!(%e, "ndi input: spawning the thread failed");
    }
}

/// Keep the shared settings in step with the stored ones (every
/// [`INPUT_SETTINGS_POLL`]) and, while enabled + not connected, list the
/// visible NDI sources every [`INPUT_FIND_EVERY`].
#[cfg_attr(test, mutants::skip)]
pub async fn run_input_config_task(
    pool: SqlitePool,
    shared: Arc<NdiInputShared>,
    receive: Option<Arc<dyn NdiReceiveBackend>>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut applied: Option<InputSettings> = None;
    let mut found_at: Option<Instant> = None;
    loop {
        match load_input_settings(&pool).await {
            Ok(settings) => {
                if applied.as_ref() != Some(&settings) {
                    info!(
                        enabled = settings.enabled,
                        source = %settings.source,
                        "ndi input: settings applied"
                    );
                    shared.set_settings(settings.clone());
                    applied = Some(settings.clone());
                }
                let since = found_at.map(|t| t.elapsed());
                if let Some(rb) = receive.clone()
                    && needs_find(settings.active(), shared.is_connected(), since)
                {
                    let names = tokio::task::spawn_blocking(move || {
                        rb.find_source_names(INPUT_FIND_WAIT_MS)
                    })
                    .await
                    .unwrap_or_default();
                    if !names.iter().any(|n| n.trim() == settings.source) {
                        warn!(
                            source = %settings.source,
                            visible = ?names,
                            "ndi input: the configured source is not visible on the network"
                        );
                    }
                    shared.set_visible(names);
                    found_at = Some(Instant::now());
                }
            }
            Err(e) => warn!(%e, "ndi input: reading the settings failed"),
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(INPUT_SETTINGS_POLL) => {}
        }
    }
    info!("ndi input: settings task stopped");
}

#[cfg(test)]
#[path = "ndi_input_tests.rs"]
mod tests;
