//! #233: the ASIO output's Windows glue over azo 0.4.0 (pure-Rust COM ASIO
//! host, MIT, no Steinberg SDK — iemmixer's choice,
//! `iemmixer/crates/iem-audio-io/src/asio.rs`). It only CALLS the driver;
//! every decision is in the Linux-tested `asio_format`, `asio_state`,
//! `asio_out`, `asrc_servo` and `asrc`, so this file is out of the mutation
//! gate (`.cargo/mutants.toml`, like `sp-gpu/src/win/`).
//!
//! - Every driver call is made on the output's worker thread, which created
//!   the driver and pumps its window messages in `poll`. The device holds
//!   that thread's COM apartment (an STA, [`ComApartment`]) from `new` until
//!   it is dropped: azo's `SafeHandle` initialises COM in `new` but
//!   uninitialises it in its own `Drop`, BEFORE its interface field is
//!   released, so without an outer apartment the driver's `Release` would
//!   run after COM went down (review round 1; azo's own host keeps an outer
//!   apartment the same way, `host.rs:105-111`).
//! - It reads the driver's rate, preferred buffer, output channels and their
//!   sample type, and never sets the rate, the clock source or the buffer,
//!   nor opens the control panel (`ci.yml` scans `crates/` for those calls).
//! - One holder per driver in the process (`asio_hold`): `open` holds the
//!   driver's name before it loads the driver and `close` gives it back
//!   after the release, so a rebuilt entry's successor never loads a second
//!   instance while its predecessor still releases the first (it is refused
//!   as busy and tries again after the 2 s backoff).
//! - ASIO callbacks carry no user pointer: [`ASIO_SLOTS`] static slots, each
//!   with its own four callbacks, hold the running streams. Two per ASIO
//!   entry: a replaced output's old worker may still hold its slot while its
//!   successor starts. An in-flight counter lets `close` free a stream only
//!   after the last callback left it (iemmixer `asio.rs:570-600`), and a
//!   slot is released only after the buffers are disposed and the driver
//!   dropped, so a late driver message never counts into another output. A
//!   callback still inside after 1 s PARKS the device (iemmixer
//!   `asio.rs:487-500`): the stream, the buffers, the driver, the slot and
//!   the driver's hold are never freed, disposed, released or reused until
//!   the process ends, and the device refuses every later open.
//! - The buffer switch copies only: it pops its frames from the ring
//!   (`rtrb`), writes L/R into the two configured channels and zeroes every
//!   other one (`asio_format::fill_channel`), and counts in atomics — no
//!   allocation, lock, log or syscall (iemmixer I7). Driver messages are
//!   answered by `asio_state::reply` and counted for `poll`.

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_long, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use azo::driver::{Driver, Metadata, SafeHandle};
use azo::dto::ChannelId;
use azo::sys::{Bool, Callbacks, MessageSelector, SampleRate, Time};
use tracing::{error, info, warn};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};

use crate::playback::asio_format::{AsioSample, fill_channel, source_of, unsupported_sample_text};
use crate::playback::asio_hold::{DriverHold, DriverHolds};
use crate::playback::asio_out::{AsioDevice, AsioOut, Opened, Started, run_asio_worker};
use crate::playback::asio_state::{DeviceEvents, Reason, reply, selector};

/// Static callback slots: two per ASIO entry (`MAX_ASIO_OUTPUTS`).
pub const ASIO_SLOTS: usize = 8;

/// One running stream, owned by a slot while its driver runs.
struct Stream {
    /// Read only by this slot's callbacks (one driver's callbacks never
    /// overlap).
    ring: UnsafeCell<rtrb::Consumer<f32>>,
    /// The frames one callback pops (allocated at start, never in the
    /// callback).
    scratch: UnsafeCell<Vec<f32>>,
    /// Every output channel's two half-buffers, in channel order.
    buffers: Vec<[*mut c_void; 2]>,
    frames: usize,
    sample: AsioSample,
    left: usize,
    right: usize,
}

/// A slot's stream pointer and counters (static: a message that comes before
/// the stream exists is still counted).
struct Slot {
    stream: AtomicPtr<Stream>,
    in_flight: AtomicUsize,
    claimed: AtomicBool,
    primed: AtomicBool,
    callbacks: AtomicU64,
    consumed: AtomicU64,
    underruns: AtomicU64,
    reset: AtomicBool,
    resync: AtomicBool,
    size_change: AtomicBool,
    latencies: AtomicBool,
    overloads: AtomicU64,
    /// The last rate `sampleRateDidChange` reported (f64 bits), valid while
    /// `rate_changed` is set: 0.0's bits are 0, and 0 Hz is a lost clock.
    rate_bits: AtomicU64,
    rate_changed: AtomicBool,
}

impl Slot {
    const fn new() -> Self {
        Self {
            stream: AtomicPtr::new(ptr::null_mut()),
            in_flight: AtomicUsize::new(0),
            claimed: AtomicBool::new(false),
            primed: AtomicBool::new(false),
            callbacks: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            reset: AtomicBool::new(false),
            resync: AtomicBool::new(false),
            size_change: AtomicBool::new(false),
            latencies: AtomicBool::new(false),
            overloads: AtomicU64::new(0),
            rate_bits: AtomicU64::new(0),
            rate_changed: AtomicBool::new(false),
        }
    }

    fn clear_counters(&self) {
        for a in [
            &self.callbacks,
            &self.consumed,
            &self.underruns,
            &self.overloads,
            &self.rate_bits,
        ] {
            a.store(0, Ordering::SeqCst);
        }
        for f in [
            &self.primed,
            &self.reset,
            &self.resync,
            &self.size_change,
            &self.latencies,
            &self.rate_changed,
        ] {
            f.store(false, Ordering::SeqCst);
        }
    }
}

static SLOTS: [Slot; ASIO_SLOTS] = [const { Slot::new() }; ASIO_SLOTS];

/// The ASIO drivers this process holds (`asio_hold`).
static HELD: DriverHolds = DriverHolds::new();

/// The buffer switch of `slot`: copy, convert, count — nothing else.
fn on_buffer(slot: &Slot, second: bool) {
    slot.in_flight.fetch_add(1, Ordering::SeqCst);
    let raw = slot.stream.load(Ordering::SeqCst);
    // SAFETY: a non-null slot points to a live stream: `close` clears the
    // slot and waits for `in_flight == 0` before it frees the stream, and
    // this callback counted itself before it read the slot.
    if let Some(stream) = unsafe { raw.as_ref() } {
        // SAFETY: one driver's callbacks never overlap; only they touch the
        // ring and the scratch.
        let (ring, scratch) = unsafe { (&mut *stream.ring.get(), &mut *stream.scratch.get()) };
        let want = stream.frames * 2;
        let got = ring.pop_partial_slice(&mut scratch[..want]).0.len();
        let bytes = stream.frames * stream.sample.bytes();
        for (ch, halves) in stream.buffers.iter().enumerate() {
            let p = halves[usize::from(second)];
            if p.is_null() {
                continue;
            }
            // SAFETY: the driver's half-buffer of `frames` samples of its
            // type, which the driver does not touch during the callback.
            let dst = unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), bytes) };
            let source = source_of(ch, stream.left, stream.right);
            fill_channel(stream.sample, &scratch[..got], source, dst);
        }
        if got < want && slot.primed.load(Ordering::Relaxed) {
            slot.underruns.fetch_add(1, Ordering::Relaxed);
        }
        slot.consumed
            .fetch_add(stream.frames as u64, Ordering::Relaxed);
        slot.callbacks.fetch_add(1, Ordering::Relaxed);
    }
    slot.in_flight.fetch_sub(1, Ordering::SeqCst);
}

/// A driver message to `slot`: counted, answered (`asio_state::reply`).
fn on_message(slot: &Slot, sel: c_long, value: c_long) -> c_long {
    match sel {
        selector::RESET_REQUEST => slot.reset.store(true, Ordering::SeqCst),
        selector::BUFFER_SIZE_CHANGE => slot.size_change.store(true, Ordering::SeqCst),
        selector::RESYNC_REQUEST => slot.resync.store(true, Ordering::SeqCst),
        selector::LATENCIES_CHANGED => slot.latencies.store(true, Ordering::SeqCst),
        selector::OVERLOAD => {
            slot.overloads.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    reply(sel, value)
}

macro_rules! slot_callbacks {
    ($i:literal, $bs:ident, $bsti:ident, $msg:ident, $rate:ident) => {
        unsafe extern "system" fn $bs(index: c_long, _direct: Bool) {
            on_buffer(&SLOTS[$i], index != 0);
        }
        unsafe extern "system" fn $bsti(
            params: *mut Time,
            index: c_long,
            _direct: Bool,
        ) -> *mut Time {
            on_buffer(&SLOTS[$i], index != 0);
            params
        }
        unsafe extern "system" fn $msg(
            sel: MessageSelector,
            value: c_long,
            _message: *const c_void,
            _opt: *const f64,
        ) -> c_long {
            on_message(&SLOTS[$i], sel.0, value)
        }
        unsafe extern "system" fn $rate(rate: SampleRate) {
            SLOTS[$i].rate_bits.store(rate.to_bits(), Ordering::SeqCst);
            SLOTS[$i].rate_changed.store(true, Ordering::SeqCst);
        }
    };
}

slot_callbacks!(0, bs0, bsti0, msg0, rate0);
slot_callbacks!(1, bs1, bsti1, msg1, rate1);
slot_callbacks!(2, bs2, bsti2, msg2, rate2);
slot_callbacks!(3, bs3, bsti3, msg3, rate3);
slot_callbacks!(4, bs4, bsti4, msg4, rate4);
slot_callbacks!(5, bs5, bsti5, msg5, rate5);
slot_callbacks!(6, bs6, bsti6, msg6, rate6);
slot_callbacks!(7, bs7, bsti7, msg7, rate7);

macro_rules! callbacks {
    ($bs:ident, $bsti:ident, $msg:ident, $rate:ident) => {
        Callbacks {
            buffer_switch: $bs,
            sample_rate_did_change: $rate,
            asio_message: $msg,
            buffer_switch_time_info: $bsti,
        }
    };
}

/// Slot `i`'s callbacks (static: they outlive every buffer created on them).
static CALLBACKS: [Callbacks; ASIO_SLOTS] = [
    callbacks!(bs0, bsti0, msg0, rate0),
    callbacks!(bs1, bsti1, msg1, rate1),
    callbacks!(bs2, bsti2, msg2, rate2),
    callbacks!(bs3, bsti3, msg3, rate3),
    callbacks!(bs4, bsti4, msg4, rate4),
    callbacks!(bs5, bsti5, msg5, rate5),
    callbacks!(bs6, bsti6, msg6, rate6),
    callbacks!(bs7, bsti7, msg7, rate7),
];

/// The registered ASIO drivers' descriptions (HKLM\SOFTWARE\ASIO, read
/// only; no driver is loaded). None registered (or no such key) → empty.
pub fn list_drivers() -> Vec<String> {
    Metadata::enumerate()
        .map(|d| d.iter().map(|m| m.description.to_string_lossy()).collect())
        .unwrap_or_default()
}

fn text(s: &CStr) -> String {
    s.to_string_lossy().into_owned()
}

/// A driver call that failed, with the driver's own last error.
fn failed(driver: &SafeHandle, what: &str, e: azo::Error) -> Reason {
    Reason::Failed(format!("{what}: {e} ({})", text(&driver.last_error())))
}

/// Run every window message queued for this (the driver's STA) thread.
fn pump_messages() {
    // SAFETY: MSG is plain data; a zeroed one is valid.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: a plain message pump on the calling thread, `msg` outlives it.
    while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        // SAFETY: `msg` is the message PeekMessageW just filled.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The calling thread's COM apartment (an STA), held until dropped.
struct ComApartment {
    /// The init succeeded (S_OK, or S_FALSE: already an STA), so the drop
    /// owes one `CoUninitialize`.
    held: bool,
}

impl ComApartment {
    fn enter() -> Self {
        // SAFETY: a plain COM init of the calling thread, balanced in Drop.
        let hr = unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        if hr < 0 {
            error!(
                hr,
                "asio output: COM refused this thread as an STA (a driver load will fail)"
            );
        }
        Self { held: hr >= 0 }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.held {
            // SAFETY: balances this guard's successful init, on its thread
            // (the device holding it is !Send: its stream pointer).
            unsafe { CoUninitialize() };
        }
    }
}

/// One ASIO output's driver, on its worker thread.
pub struct WinAsioDevice {
    /// This device's hold on its driver's name, from before the load until
    /// after the release (kept for good when the device is parked).
    hold: Option<DriverHold<'static>>,
    driver: Option<SafeHandle>,
    channels: [usize; 2],
    opened: Option<Opened>,
    slot: Option<usize>,
    stream: *mut Stream,
    buffers_created: bool,
    /// `start` succeeded: `close` stops only a started driver.
    started: bool,
    /// A callback never left the stream: everything it may touch is kept
    /// (see the module doc) and the device opens nothing again.
    parked: bool,
    /// The thread's STA, for the device's whole life; declared last, so it
    /// drops last (after `Drop::drop` ran `close`, which releases the driver).
    com: ComApartment,
}

impl WinAsioDevice {
    /// No driver loaded; the calling (worker) thread is held as an STA.
    pub fn new() -> Self {
        Self {
            hold: None,
            driver: None,
            channels: [0, 1],
            opened: None,
            slot: None,
            stream: ptr::null_mut(),
            buffers_created: false,
            started: false,
            parked: false,
            com: ComApartment::enter(),
        }
    }

    /// The driver's rate, preferred buffer, output channels and their one
    /// sample type (read, never set).
    fn read(driver: &SafeHandle, channels: [u32; 2]) -> Result<Opened, Reason> {
        let rate = driver
            .get_sample_rate()
            .map_err(|e| failed(driver, "getSampleRate", e))?;
        let size = driver
            .buffer_size()
            .map_err(|e| failed(driver, "getBufferSize", e))?;
        let counts = driver
            .channel_counts()
            .map_err(|e| failed(driver, "getChannels", e))?;
        let out_channels = u32::try_from(counts.out).unwrap_or(0);
        if channels.iter().any(|&c| c >= out_channels) {
            return Err(Reason::Refused(format!(
                "the driver has {out_channels} output channels (asked for {} and {})",
                channels[0] + 1,
                channels[1] + 1
            )));
        }
        let mut sample: Option<AsioSample> = None;
        for index in 0..counts.out {
            let info = driver
                .channel_info(ChannelId {
                    input: false,
                    index,
                })
                .map_err(|e| failed(driver, "getChannelInfo", e))?;
            let this = AsioSample::from_code(info.sample_type.0)
                .map_err(|code| Reason::Refused(unsupported_sample_text(code)))?;
            if sample.is_some_and(|s| s != this) {
                return Err(Reason::Refused(
                    "the driver's output channels mix sample types".into(),
                ));
            }
            sample = Some(this);
        }
        let sample =
            sample.ok_or_else(|| Reason::Refused("the driver has no output channel".into()))?;
        let buffer_frames = u32::try_from(size.preferred)
            .ok()
            .filter(|f| *f > 0)
            .ok_or_else(|| {
                Reason::Refused(format!(
                    "the driver prefers a buffer of {} frames",
                    size.preferred
                ))
            })?;
        Ok(Opened {
            rate,
            buffer_frames,
            out_channels,
            sample,
        })
    }
}

impl Default for WinAsioDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl AsioDevice for WinAsioDevice {
    fn open(&mut self, name: &str, channels: [u32; 2]) -> Result<Opened, Reason> {
        self.close();
        if self.parked {
            return Err(Reason::Failed(
                "a driver callback did not return for 1 s: the driver is parked until SongPlayer restarts".into(),
            ));
        }
        // Before anything loads: a held driver (the output this one
        // replaces still releasing it) is busy for now. On a failed open the
        // hold drops after the driver (locals drop in reverse order).
        let Some(hold) = HELD.claim(name) else {
            return Err(Reason::Held);
        };
        let drivers = Metadata::enumerate().unwrap_or_default();
        let present: Vec<String> = drivers
            .iter()
            .map(|d| d.description.to_string_lossy())
            .collect();
        let Some(meta) = drivers
            .iter()
            .find(|d| d.description.to_string_lossy() == name)
        else {
            return Err(Reason::NotFound { present });
        };
        let driver = match SafeHandle::new(&meta.clsid) {
            Ok(driver) => driver,
            Err(e) => {
                // azo counted a COM init before its CoCreateInstance failed
                // and never undoes it; inside the device's STA that init
                // cannot have failed, so it is balanced here.
                if self.com.held {
                    // SAFETY: balances azo's init on this thread.
                    unsafe { CoUninitialize() };
                }
                return Err(Reason::Failed(format!("loading the driver failed: {e}")));
            }
        };
        if !driver.init(None) {
            return Err(Reason::Busy(text(&driver.last_error())));
        }
        let opened = Self::read(&driver, channels)?;
        self.hold = Some(hold);
        self.driver = Some(driver);
        self.channels = [channels[0] as usize, channels[1] as usize];
        self.opened = Some(opened);
        Ok(opened)
    }

    fn start(&mut self, ring: rtrb::Consumer<f32>) -> Result<Started, Reason> {
        let (Some(driver), Some(opened)) = (self.driver.as_ref(), self.opened) else {
            return Err(Reason::Failed("start before open".into()));
        };
        let slot = (0..ASIO_SLOTS)
            .find(|&i| {
                SLOTS[i]
                    .claimed
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
            .ok_or_else(|| Reason::Refused("every ASIO callback slot is in use".into()))?;
        SLOTS[slot].clear_counters();
        self.slot = Some(slot);
        let frames = opened.buffer_frames as usize;
        let ids = (0..opened.out_channels as c_long).map(|index| ChannelId {
            input: false,
            index,
        });
        // SAFETY: CALLBACKS is a static, so it outlives the buffers; the
        // buffers are written only by this slot's callback (between start and
        // stop) and are zeroed below before the start.
        let created = unsafe { driver.create_buffers(ids, frames as c_long, &CALLBACKS[slot]) }
            .map_err(|e| failed(driver, "createBuffers", e))?;
        let buffers: Vec<[*mut c_void; 2]> = created.collect();
        self.buffers_created = true;
        let bytes = frames * opened.sample.bytes();
        for half in buffers.iter().flatten().filter(|p| !p.is_null()) {
            // SAFETY: a fresh half-buffer of `bytes`; no callback runs before
            // the start.
            unsafe { ptr::write_bytes(half.cast::<u8>(), 0, bytes) };
        }
        let stream = Box::into_raw(Box::new(Stream {
            ring: UnsafeCell::new(ring),
            scratch: UnsafeCell::new(vec![0.0; frames * 2]),
            buffers,
            frames,
            sample: opened.sample,
            left: self.channels[0],
            right: self.channels[1],
        }));
        self.stream = stream;
        SLOTS[slot].stream.store(stream, Ordering::SeqCst);
        driver.start().map_err(|e| failed(driver, "start", e))?;
        self.started = true;
        Ok(Started {
            output_latency_frames: self.output_latency_frames(),
        })
    }

    fn poll(&mut self) -> DeviceEvents {
        pump_messages();
        let Some(i) = self.slot else {
            return DeviceEvents::default();
        };
        let s = &SLOTS[i];
        // The flag first: a later report between the two reads is the one
        // read (the newest rate wins), never a lost one.
        let rate_changed = s
            .rate_changed
            .swap(false, Ordering::SeqCst)
            .then(|| f64::from_bits(s.rate_bits.load(Ordering::SeqCst)));
        DeviceEvents {
            reset: s.reset.swap(false, Ordering::SeqCst),
            resync: s.resync.swap(false, Ordering::SeqCst),
            buffer_size_change: s.size_change.swap(false, Ordering::SeqCst),
            latencies_changed: s.latencies.swap(false, Ordering::SeqCst),
            rate_changed,
            overloads: s.overloads.load(Ordering::Relaxed),
            callbacks: s.callbacks.load(Ordering::Relaxed),
        }
    }

    fn consumed_frames(&self) -> u64 {
        self.slot
            .map_or(0, |i| SLOTS[i].consumed.load(Ordering::Relaxed))
    }

    fn underruns(&self) -> u64 {
        self.slot
            .map_or(0, |i| SLOTS[i].underruns.load(Ordering::Relaxed))
    }

    fn mark_primed(&mut self) {
        if let Some(i) = self.slot {
            SLOTS[i].primed.store(true, Ordering::SeqCst);
        }
    }

    fn output_latency_frames(&self) -> u32 {
        self.driver
            .as_ref()
            .and_then(|d| d.latencies().ok())
            .map_or(0, |l| u32::try_from(l.out).unwrap_or(0))
    }

    fn close(&mut self) {
        if self.parked {
            return;
        }
        if self.started
            && let Some(d) = self.driver.as_ref()
            && let Err(e) = d.stop()
        {
            warn!(%e, "asio output: stopping the driver failed (it is released anyway)");
        }
        self.started = false;
        if let Some(i) = self.slot {
            let s = &SLOTS[i];
            s.stream.store(ptr::null_mut(), Ordering::SeqCst);
            // Bounded: a driver stuck inside a callback for a second leaks
            // its stream instead of freeing it under the callback.
            for _ in 0..1_000 {
                if s.in_flight.load(Ordering::SeqCst) == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if s.in_flight.load(Ordering::SeqCst) != 0 {
                // Everything the stuck callback may touch stays: the stream
                // (leaked), the buffers it writes (not disposed), the driver
                // (never released), this slot (its in-flight count is the
                // callback's) and the driver's hold.
                error!(
                    slot = i,
                    "asio output: a callback is still running after 1 s — its stream, buffers, driver, slot and hold are parked until SongPlayer restarts"
                );
                self.parked = true;
                self.stream = ptr::null_mut();
                std::mem::forget(self.driver.take());
                self.started = false;
                self.buffers_created = false;
                self.opened = None;
                return;
            }
            if !self.stream.is_null() {
                // SAFETY: the slot no longer points to it and no callback is
                // inside it.
                drop(unsafe { Box::from_raw(self.stream) });
            }
        }
        self.stream = ptr::null_mut();
        if self.buffers_created
            && let Some(d) = self.driver.as_ref()
            && let Err(e) = d.dispose_buffers()
        {
            warn!(%e, "asio output: disposing the driver's buffers failed (it is released anyway)");
        }
        self.buffers_created = false;
        // azo's handle uninitialises its own COM init, then releases the
        // driver: still inside the device's apartment (`com`).
        self.driver = None;
        // Given back only now, after the release.
        self.hold = None;
        self.opened = None;
        if let Some(i) = self.slot.take() {
            SLOTS[i].claimed.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for WinAsioDevice {
    fn drop(&mut self) {
        self.close();
        if self.parked {
            // The parked driver stays loaded: no other output of the
            // process may load a second instance of it.
            std::mem::forget(self.hold.take());
        }
    }
}

/// The ASIO output's worker thread (`asio-<id>`): its own driver, its own
/// program-wall clock (`WallVbanClock::new`, following the wall: a date step
/// shows to the servo as a step it re-centres). Not an MMCSS thread
/// (`asio_out.rs`). A failed spawn is the output's start error (the outputs
/// task rebuilds it on its next pass).
#[cfg_attr(test, mutants::skip)]
pub fn spawn_asio_thread(out: Arc<AsioOut>, id: String) {
    let watched = out.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("asio-{id}"))
        .spawn(move || {
            crate::playback::pipeline_paced::request_high_res_timer();
            info!(id = %id, "asio output thread started");
            let mut device = WinAsioDevice::new();
            let wall = crate::playback::wallclock::WallClock::system();
            let mut clock = crate::playback::vban_out::WallVbanClock::new(wall);
            run_asio_worker(&out, &mut device, &mut clock);
        });
    if let Err(e) = spawned {
        error!(%e, "asio output: spawning the worker thread failed");
        watched.set_start_error(format!(
            "the ASIO worker did not start: spawning it failed: {e}"
        ));
    }
}

#[cfg(test)]
#[path = "asio_win_tests.rs"]
mod tests;
