//! #233: the ASIO output's decisions, pure (the worker `asio_out.rs` and the
//! Windows glue `asio_win.rs` only act on them).
//!
//! - Reopen backoff 2 / 10 / 30 / 60 / 60 … s after a failed open or a close;
//!   a run of 60 s resets it, so a reset after a long run waits 2 s (some
//!   virtual drivers stay busy for seconds after a release).
//! - A close: a reset request (or a buffer-size change, which is answered 0
//!   so the driver then asks a reset — iemmixer never resizes live), a rate
//!   change of 1 Hz or more (Dante Controller re-clocked the card; 0 Hz is a
//!   lost clock), or no callback for 2 s (a vanished driver: a DVS crash or
//!   reinstall; iemmixer `reset.rs` STALL).
//! - asioMessage replies: iemmixer `telemetry.rs:79-99`.
//! - The driver's rate, buffer and sample type are read, never set: the rate
//!   is admitted (8–384 kHz) and noted when it is not the network's; a
//!   buffer over a third of a grid slot is noted (lane 2's envelope: the
//!   servo's per-block latency saws by one callback period).

use crate::playback::asrc_servo::{GROSS_STEP_100NS, frames_from_100ns};

pub const BACKOFF_S: [i64; 4] = [2, 10, 30, 60];
/// A run this long resets the backoff (60 s).
pub const STABLE_RUN_100NS: i64 = 600_000_000;
/// No callback for this long while running: the driver is gone (2 s).
pub const STALL_100NS: i64 = 20_000_000;
/// The worker steps at least this often (driver messages, 10 ms).
pub const POLL_100NS: i64 = 100_000;
pub const MIN_RATE: f64 = 8_000.0;
pub const MAX_RATE: f64 = 384_000.0;
/// A driver buffer longer than 1/this s is noted: a third of a grid slot.
pub const BUFFER_NOTE_PER_S: u64 = 90;

/// The wait before the next open after `failures` failed opens or closes in
/// a row (at least the first step).
pub fn backoff_100ns(failures: u32) -> i64 {
    let i = (failures.max(1) as usize - 1).min(BACKOFF_S.len() - 1);
    BACKOFF_S[i] * 10_000_000
}

/// The failure count after a close of an output that ran `ran_100ns`.
pub fn failures_after_close(failures: u32, ran_100ns: i64) -> u32 {
    if ran_100ns >= STABLE_RUN_100NS {
        1
    } else {
        failures.saturating_add(1)
    }
}

/// Why an ASIO output is not running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    NotFound {
        present: Vec<String>,
    },
    Busy(String),
    Refused(String),
    Failed(String),
    Reset,
    RateChanged(u32),
    Stalled,
    WindowsOnly,
    /// Another output of this process still holds the driver (a rebuilt
    /// entry's predecessor releasing it, `asio_hold`).
    Held,
}

impl Reason {
    /// A stable code (the dashboard maps it to Slovak).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Busy(_) => "busy",
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
            Self::Reset => "reset",
            Self::RateChanged(0) => "clock_lost",
            Self::RateChanged(_) => "rate_changed",
            Self::Stalled => "stalled",
            Self::WindowsOnly => "windows_only",
            Self::Held => "held",
        }
    }

    /// The server's text (English, like every API reason).
    pub fn text(&self) -> String {
        match self {
            Self::NotFound { present } if present.is_empty() => {
                "the driver is not registered (present: none)".into()
            }
            Self::NotFound { present } => format!(
                "the driver is not registered (present: {})",
                present.join(", ")
            ),
            Self::Busy(e) => {
                format!("the driver refused to start (in use by another program?): {e}")
            }
            Self::Refused(e) | Self::Failed(e) => e.clone(),
            Self::Reset => "the driver asked for a reset".into(),
            Self::RateChanged(0) => "the driver lost its clock (it reports 0 Hz)".into(),
            Self::RateChanged(r) => format!("the driver's rate changed to {r} Hz"),
            Self::Stalled => "no callback from the driver for 2 s".into(),
            Self::WindowsOnly => "ASIO runs on Windows only".into(),
            Self::Held => "another SongPlayer output still holds the driver (the output this one replaces is releasing it)".into(),
        }
    }
}

/// What the driver said since the last poll (counted by its callbacks).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DeviceEvents {
    pub reset: bool,
    pub resync: bool,
    pub buffer_size_change: bool,
    pub latencies_changed: bool,
    /// The last rate `sampleRateDidChange` reported (0 = a lost clock).
    pub rate_changed: Option<f64>,
    pub overloads: u64,
    /// Buffer switches since the output started (monotonic).
    pub callbacks: u64,
}

/// Whether the events close an output opened at `opened_rate`.
pub fn close_reason(ev: &DeviceEvents, opened_rate: f64) -> Option<Reason> {
    if ev.reset || ev.buffer_size_change {
        return Some(Reason::Reset);
    }
    match ev.rate_changed {
        Some(r) if (r - opened_rate).abs() >= 1.0 => Some(Reason::RateChanged(r.round() as u32)),
        _ => None,
    }
}

/// No callback for [`STALL_100NS`].
#[derive(Debug, Default)]
pub struct StallWatch {
    last: Option<(u64, i64)>,
}

impl StallWatch {
    /// `callbacks` (monotonic) read at `now_100ns`: whether none came for
    /// [`STALL_100NS`].
    pub fn stalled(&mut self, callbacks: u64, now_100ns: i64) -> bool {
        match self.last {
            Some((count, since)) if count == callbacks => now_100ns - since >= STALL_100NS,
            _ => {
                self.last = Some((callbacks, now_100ns));
                false
            }
        }
    }
}

/// The ASIO driver-to-host message selectors (asio.h `kAsio…`, azo-sys
/// `MessageSelector`).
pub mod selector {
    pub const SELECTOR_SUPPORTED: i32 = 1;
    pub const ENGINE_VERSION: i32 = 2;
    pub const RESET_REQUEST: i32 = 3;
    pub const BUFFER_SIZE_CHANGE: i32 = 4;
    pub const RESYNC_REQUEST: i32 = 5;
    pub const LATENCIES_CHANGED: i32 = 6;
    pub const SUPPORTS_TIME_INFO: i32 = 7;
    pub const SUPPORTS_TIME_CODE: i32 = 8;
    pub const OVERLOAD: i32 = 15;
}

/// The host's answer to `asioMessage` (iemmixer `telemetry.rs:79-99`): it
/// supports resets (by reopening), resyncs and time info; it never resizes
/// buffers live, so a size change is answered 0.
pub fn reply(sel: i32, value: i32) -> i32 {
    use selector::*;
    match sel {
        SELECTOR_SUPPORTED => i32::from(matches!(
            value,
            ENGINE_VERSION
                | RESET_REQUEST
                | BUFFER_SIZE_CHANGE
                | RESYNC_REQUEST
                | LATENCIES_CHANGED
                | SUPPORTS_TIME_INFO
                | OVERLOAD
        )),
        ENGINE_VERSION => 2,
        RESET_REQUEST | RESYNC_REQUEST | LATENCIES_CHANGED | SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}

/// The driver's rate, as the output follows it (never set).
pub fn admit_rate(rate: f64) -> Result<u32, Reason> {
    if rate.is_finite() && (MIN_RATE..=MAX_RATE).contains(&rate) {
        Ok(rate.round() as u32)
    } else {
        Err(Reason::Refused(format!("the driver reports {rate} Hz")))
    }
}

/// The status note of a driver off the network's rate (a WARN once).
pub fn rate_note(driver_rate: u32, network: u32) -> Option<String> {
    (driver_rate != network)
        .then(|| format!("the driver runs at {driver_rate} Hz, the network at {network} Hz"))
}

/// The status note of a driver buffer over a third of a grid slot: the
/// servo's per-block latency saws by one callback period (lane 2's
/// envelope: ≤ 512 frames at 48 kHz, ≤ 1024 at 96 kHz).
pub fn buffer_note(buffer_frames: u32, rate: u32) -> Option<String> {
    (u64::from(buffer_frames) * BUFFER_NOTE_PER_S > u64::from(rate)).then(|| {
        format!(
            "the driver's buffer of {buffer_frames} frames ({:.1} ms) is over a third of a grid \
             slot (11.1 ms): the drift servo may re-centre often",
            f64::from(buffer_frames) * 1_000.0 / f64::from(rate)
        )
    })
}

/// The ring's capacity, frames: the target latency + four slots, + one
/// block's output.
pub fn ring_capacity_frames(rate: f64, target_100ns: i64, max_block_frames: usize) -> usize {
    let span_100ns = target_100ns + 4 * GROSS_STEP_100NS;
    frames_from_100ns(span_100ns, rate).max(0) as usize + max_block_frames
}

/// An ASIO output's latency, ms: the servo's (ring + splice + hand-off), the
/// resampler's delay, the driver's output latency.
pub fn asio_latency_ms(
    servo_latency_ms: f64,
    asrc_delay_frames: usize,
    driver_latency_frames: u32,
    rate: f64,
) -> f64 {
    servo_latency_ms
        + (asrc_delay_frames as f64 + f64::from(driver_latency_frames)) * 1_000.0 / rate
}

#[cfg(test)]
#[path = "asio_state_tests.rs"]
mod tests;
