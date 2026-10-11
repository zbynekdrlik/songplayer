//! #233: an ASIO output's status as the host's pipe carries it. The host
//! reads its output's [`AsioSnapshot`] (`AsioOut::snapshot`, the worker's
//! own) and sends it as a [`WireSnapshot`]; SongPlayer turns it back into the
//! same snapshot (`AsioOut::apply_remote`), so `GET /api/v1/program` and the
//! dashboard read what they always read.
//!
//! The snapshot's fixed words (`&'static str`) are read back against their
//! known sets: the state (an unknown one reads "waiting"), the driver's
//! sample type (an unknown one reads ""), a hard re-centre's cause (typed,
//! [`WireCause`]); the reason's code is the decoded reason's own
//! (`Reason::code`). Every float is sent finite (a non-finite one as 0):
//! JSON has no NaN, serde_json would write `null` and the typed decode would
//! refuse the whole frame.

use serde::{Deserialize, Serialize};

use crate::playback::asio_format::AsioSample;
use crate::playback::asio_out::{AsioSnapshot, AsioStatus, HardRecentre, LateCallback};
use crate::playback::asio_state::Reason;
use crate::playback::asrc_servo::Recentre;
use crate::playback::audio_out::{STATE_DISABLED, STATE_OPENING, STATE_RUNNING, STATE_WAITING};

/// `AsioSnapshot` on the pipe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireSnapshot {
    pub state: String,
    pub reason: Option<Reason>,
    pub status: WireStatus,
    pub blocks_sent: u64,
    pub blocks_dropped: u64,
}

/// `AsioStatus` on the pipe (its fields, the fixed words as text).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireStatus {
    pub driver: String,
    pub channels: [u32; 2],
    pub driver_rate: u32,
    pub buffer_frames: u32,
    pub out_channels: u32,
    pub sample_type: String,
    pub ppm: f64,
    pub rate_ppm: f64,
    pub locked: bool,
    pub latency_ms: f64,
    pub offset_ms: f64,
    pub slew_eta_s: Option<f64>,
    pub cushion_ms: f64,
    pub underruns: u64,
    pub resets: u64,
    pub hard_recentres: u64,
    pub last_hard_recentre: Option<WireHard>,
    pub overflows: u64,
    pub overloads: u64,
    pub retry_in_s: Option<f64>,
    pub clock_waits: u64,
    pub resyncs: u64,
    pub callback_gap_max_ms: f64,
    pub late_callbacks: u64,
    pub fast_callbacks: u64,
    pub last_late_callback: Option<LateCallback>,
}

/// `HardRecentre` on the pipe.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireHard {
    pub cause: WireCause,
    pub ms: f64,
    pub lateness_ms: f64,
    pub ago_s: f64,
}

/// A hard re-centre's cause (`Recentre::as_str`), typed on the pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireCause {
    Prime,
    Deficit,
    Excess,
}

/// Every sample type a driver can run (`AsioSample`), to read its name back.
const SAMPLES: [AsioSample; 8] = [
    AsioSample::Int16,
    AsioSample::Int24,
    AsioSample::Int32,
    AsioSample::Float32,
    AsioSample::Int32In16,
    AsioSample::Int32In18,
    AsioSample::Int32In20,
    AsioSample::Int32In24,
];

/// Every output state (`audio_out::STATE_*`).
const STATES: [&str; 4] = [STATE_RUNNING, STATE_OPENING, STATE_WAITING, STATE_DISABLED];

/// `x`, or 0 when it is not finite (JSON has no NaN).
pub fn finite(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

/// The known state named `s`; an unknown one reads waiting.
pub fn known_state(s: &str) -> &'static str {
    STATES
        .into_iter()
        .find(|k| *k == s)
        .unwrap_or(STATE_WAITING)
}

/// The known sample type named `s`; an unknown one reads "".
pub fn known_sample(s: &str) -> &'static str {
    SAMPLES
        .into_iter()
        .map(AsioSample::name)
        .find(|n| *n == s)
        .unwrap_or("")
}

impl WireCause {
    fn of(word: &str) -> Self {
        match word {
            "prime" => Self::Prime,
            "excess" => Self::Excess,
            _ => Self::Deficit,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Prime => Recentre::Prime.as_str(),
            Self::Deficit => Recentre::Deficit.as_str(),
            Self::Excess => Recentre::Excess.as_str(),
        }
    }
}

impl From<&AsioSnapshot> for WireSnapshot {
    fn from(s: &AsioSnapshot) -> Self {
        let t = &s.status;
        Self {
            state: s.state.to_string(),
            reason: s.reason.clone(),
            blocks_sent: s.blocks_sent,
            blocks_dropped: s.blocks_dropped,
            status: WireStatus {
                driver: t.driver.clone(),
                channels: t.channels,
                driver_rate: t.driver_rate,
                buffer_frames: t.buffer_frames,
                out_channels: t.out_channels,
                sample_type: t.sample_type.to_string(),
                ppm: finite(t.ppm),
                rate_ppm: finite(t.rate_ppm),
                locked: t.locked,
                latency_ms: finite(t.latency_ms),
                offset_ms: finite(t.offset_ms),
                slew_eta_s: t.slew_eta_s.map(finite),
                cushion_ms: finite(t.cushion_ms),
                underruns: t.underruns,
                resets: t.resets,
                hard_recentres: t.hard_recentres,
                last_hard_recentre: t.last_hard_recentre.map(|h| WireHard {
                    cause: WireCause::of(h.cause),
                    ms: finite(h.ms),
                    lateness_ms: finite(h.lateness_ms),
                    ago_s: finite(h.ago_s),
                }),
                overflows: t.overflows,
                overloads: t.overloads,
                retry_in_s: t.retry_in_s.map(finite),
                clock_waits: t.clock_waits,
                resyncs: t.resyncs,
                callback_gap_max_ms: finite(t.callback_gap_max_ms),
                late_callbacks: t.late_callbacks,
                fast_callbacks: t.fast_callbacks,
                last_late_callback: t.last_late_callback.map(|l| LateCallback {
                    gap_ms: finite(l.gap_ms),
                    ago_s: finite(l.ago_s),
                }),
            },
        }
    }
}

impl WireSnapshot {
    /// The snapshot the host read (the reason's code from the reason).
    pub fn into_snapshot(self) -> AsioSnapshot {
        let t = self.status;
        let reason_code = self.reason.as_ref().map(Reason::code);
        AsioSnapshot {
            state: known_state(&self.state),
            reason: self.reason,
            blocks_sent: self.blocks_sent,
            blocks_dropped: self.blocks_dropped,
            status: AsioStatus {
                driver: t.driver,
                channels: t.channels,
                driver_rate: t.driver_rate,
                buffer_frames: t.buffer_frames,
                out_channels: t.out_channels,
                sample_type: known_sample(&t.sample_type),
                ppm: t.ppm,
                rate_ppm: t.rate_ppm,
                locked: t.locked,
                latency_ms: t.latency_ms,
                offset_ms: t.offset_ms,
                slew_eta_s: t.slew_eta_s,
                cushion_ms: t.cushion_ms,
                underruns: t.underruns,
                resets: t.resets,
                hard_recentres: t.hard_recentres,
                last_hard_recentre: t.last_hard_recentre.map(|h| HardRecentre {
                    cause: h.cause.word(),
                    ms: h.ms,
                    lateness_ms: h.lateness_ms,
                    ago_s: h.ago_s,
                }),
                overflows: t.overflows,
                overloads: t.overloads,
                retry_in_s: t.retry_in_s,
                reason_code,
                clock_waits: t.clock_waits,
                resyncs: t.resyncs,
                callback_gap_max_ms: t.callback_gap_max_ms,
                late_callbacks: t.late_callbacks,
                fast_callbacks: t.fast_callbacks,
                last_late_callback: t.last_late_callback,
            },
        }
    }
}

#[cfg(test)]
#[path = "wire_status_tests.rs"]
mod tests;
