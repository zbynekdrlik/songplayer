//! #279 P1b: NDI `SP-program`'s pictures from the GPU (design: #279 comment
//! 6218637444; plan `docs/superpowers/plans/2026-10-16-gpu-video-path.md`
//! §5.6).
//!
//! The `program-max` thread composes each boundary at its fixed compose
//! phase and, while `program_ndi_source` = `gpu`, reads the FHD target back
//! as UYVY (`program_max_ndi.rs`). [`NdiFeed`] hands those pictures to the
//! `program-output` thread, keyed by their boundary's stamp, which sends
//! each one ONE boundary late with that boundary's audio
//! (`program_output_ndi.rs`, the one-slot delay line): picture and audio
//! move together, the stamps are unchanged, the delay is constant.
//!
//! - [`FeedCore`] (pure) keeps at most [`NDI_FEED_DEPTH`] pictures; over it
//!   the OLDEST goes (`feed_coalesced`). [`FeedCore::take`] removes and
//!   returns one stamp's picture and drops every older one; a picture
//!   pushed after its stamp was served is dropped (`readback_late`).
//! - [`ndi_source_for`] (pure) picks NDI's source per boundary: the GPU
//!   while the setting says `gpu` AND the feed's newest picture is at most
//!   [`NDI_FEED_STALE_SLOTS`] behind the boundary, else the CPU canvas, so a
//!   compositor that cannot run (off Windows, no adapter, a backoff) never
//!   freezes NDI.
//! - [`ReturnGate`] (pure, #279 ROZHODNUTÉ 6232669201 item 3): after an
//!   automatic fallback to the canvas NDI goes back to the GPU only after
//!   [`NDI_RETURN_ON_TIME`] on-time readbacks in a row, so a fallback never
//!   flaps boundary by boundary. The switch lines are a
//!   `stat_window::StateLog` of the source, rate-limited by
//!   [`NDI_SWITCH_LOG_EVERY_100NS`]: the log never ends on a source NDI
//!   left.
//! - [`NdiFeed`] = the core + the NDI telemetry under ONE short lock per
//!   call: nothing waits on the other thread. It is `MaxOut::ndi`, on
//!   `ProgramBus::max()`; `GET /api/v1/program` serves [`NdiStatus`] as
//!   `ndi`.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use sp_core::config::NdiSource;
use sp_core::genlock::{GENLOCK_GRID_FPS, grid_index_100ns};

use crate::playback::frame_buf::{PerSize, SharedFrame};
use crate::playback::program_max::Window;

/// How many read-back pictures wait for the `program-output` thread; over
/// it the oldest goes.
pub const NDI_FEED_DEPTH: usize = 3;

/// How far (grid slots, 1 s) the feed's newest picture may lag a boundary
/// for NDI to come from the GPU; further, the CPU canvas sends it.
pub const NDI_FEED_STALE_SLOTS: i64 = 30;

/// At most one log line of a change of NDI's source per this much timeline
/// (100 ns; 5 s); every switch is counted all the same (`ndi.switches`).
pub const NDI_SWITCH_LOG_EVERY_100NS: i64 = 50_000_000;

/// #279 ROZHODNUTÉ 6232669201 item 3: after an automatic fallback to the
/// canvas, NDI goes back to the GPU only after this many on-time readbacks
/// in a row (1 s of the grid).
pub const NDI_RETURN_ON_TIME: u32 = 30;

/// The constant UYVY black: one pixel pair `U Y0 V Y1` of BT.709
/// limited-range black, what the GPU's pass makes of the cleared target.
pub const UYVY_BLACK_PAIR: [u8; 4] = [128, 16, 128, 16];

/// The bytes of a `width`×`height` UYVY picture: two a pixel, packed rows.
pub fn uyvy_len(width: u32, height: u32) -> usize {
    width as usize * 2 * height as usize
}

/// A `width`×`height` UYVY black (`width / 2` pairs a row, `height` rows).
pub fn black_uyvy(width: u32, height: u32) -> Vec<u8> {
    let pairs = (width / 2) as usize * height as usize;
    UYVY_BLACK_PAIR.repeat(pairs)
}

/// The UYVY black, built once for a size and handed out by `Arc` clone (a
/// different size builds it again): every boundary that needs it sends the
/// same allocation (`frame_buf::PerSize`, the NV12 black's cache).
#[derive(Debug, Default)]
pub struct BlackUyvy(PerSize);

impl BlackUyvy {
    pub fn get(&mut self, width: u32, height: u32) -> SharedFrame {
        self.0.get(width, height, black_uyvy)
    }
}

/// What a push did with its picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// Kept for its take.
    Kept,
    /// Kept; the oldest picture went to make room (`feed_coalesced`).
    Coalesced,
    /// Its stamp was already served: dropped (`readback_late`).
    Late,
}

/// The feed's pictures, keyed by stamp (pure).
#[derive(Debug, Default)]
pub struct FeedCore {
    pictures: BTreeMap<i64, SharedFrame>,
    /// The stamp the last take served.
    served: Option<i64>,
    /// The stamp of the last picture KEPT (pushes come in stamp order; a
    /// late one never entered the feed, so it proves no fresh readback).
    newest: Option<i64>,
    coalesced: u64,
    late: u64,
}

impl FeedCore {
    /// The `program-max` thread read boundary `stamp_100ns` back.
    pub fn push(&mut self, stamp_100ns: i64, picture: SharedFrame) -> Pushed {
        if self.served.is_some_and(|served| stamp_100ns <= served) {
            self.late += 1;
            return Pushed::Late;
        }
        self.newest = Some(stamp_100ns);
        self.pictures.insert(stamp_100ns, picture);
        if self.pictures.len() > NDI_FEED_DEPTH {
            self.pictures.pop_first();
            self.coalesced += 1;
            return Pushed::Coalesced;
        }
        Pushed::Kept
    }

    /// The `program-output` thread sends boundary `stamp_100ns`: its
    /// picture, if it came; every older one goes with it.
    pub fn take(&mut self, stamp_100ns: i64) -> Option<SharedFrame> {
        self.served = Some(stamp_100ns);
        let mut kept = self.pictures.split_off(&stamp_100ns);
        let picture = kept.remove(&stamp_100ns);
        self.pictures = kept;
        picture
    }

    /// The pictures go (NDI is on the canvas by the setting: nothing will
    /// take them); the counters and the freshness stay.
    pub fn clear(&mut self) {
        self.pictures.clear();
    }

    /// The stamp of the newest picture kept (served since or not): a
    /// readback that only ever arrives late leaves it behind, so NDI falls
    /// back to the canvas instead of repeating one picture for good.
    pub fn newest(&self) -> Option<i64> {
        self.newest
    }
}

/// Why NDI comes from the CPU canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuWhy {
    /// `program_ndi_source` is not `gpu`.
    Setting,
    /// The setting says `gpu`, but no picture was read back yet.
    NoReadback,
    /// The newest picture read back is over [`NDI_FEED_STALE_SLOTS`] old (a
    /// readback that keeps arriving late, too: a late picture is never
    /// kept, so it leaves the newest behind).
    Stale,
    /// After an automatic fallback: the readback is fresh, but has not come
    /// on time [`NDI_RETURN_ON_TIME`] boundaries in a row yet
    /// ([`ReturnGate`]).
    Settling,
}

impl CpuWhy {
    /// The log's word.
    pub fn as_str(self) -> &'static str {
        match self {
            CpuWhy::Setting => "setting",
            CpuWhy::NoReadback => "no_readback",
            CpuWhy::Stale => "stale",
            CpuWhy::Settling => "settling",
        }
    }
}

/// NDI's source for one boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdiPick {
    /// The GPU readback, one boundary late.
    Gpu,
    /// The CPU canvas, and why.
    Cpu(CpuWhy),
}

impl NdiPick {
    pub fn source(self) -> NdiSource {
        match self {
            NdiPick::Gpu => NdiSource::Gpu,
            NdiPick::Cpu(_) => NdiSource::Cpu,
        }
    }
}

/// NDI's source for boundary `b_100ns`: the GPU while `setting` says `gpu`
/// AND the feed's `newest` picture is at most [`NDI_FEED_STALE_SLOTS`] grid
/// slots behind it; else the CPU canvas.
pub fn ndi_source_for(setting: NdiSource, newest: Option<i64>, b_100ns: i64) -> NdiPick {
    if setting == NdiSource::Cpu {
        return NdiPick::Cpu(CpuWhy::Setting);
    }
    let Some(newest) = newest else {
        return NdiPick::Cpu(CpuWhy::NoReadback);
    };
    let behind =
        grid_index_100ns(b_100ns, GENLOCK_GRID_FPS) - grid_index_100ns(newest, GENLOCK_GRID_FPS);
    if behind <= NDI_FEED_STALE_SLOTS {
        NdiPick::Gpu
    } else {
        NdiPick::Cpu(CpuWhy::Stale)
    }
}

/// The way back to the GPU after an automatic fallback (pure; #279
/// ROZHODNUTÉ 6232669201 item 3). A fallback from the GPU for any reason
/// but the setting arms it. While armed, every boundary on the canvas
/// takes the boundary before's read-back picture from the feed, as the
/// GPU would have, and counts the ones that came on time in a row; a late
/// or a missing one starts the count again. A GPU pick before
/// [`NDI_RETURN_ON_TIME`] in a row stays on the canvas (`Settling`). The
/// return to the GPU, or the setting's canvas, disarms it: the setting's
/// flip to `gpu` (a first entry) goes to the GPU as soon as the newest
/// picture read back is fresh, as designed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReturnGate {
    /// `Some(on-time readbacks in a row)` since an automatic fallback.
    on_time: Option<u32>,
}

impl ReturnGate {
    /// Whether it counts: NDI is on the canvas after an automatic fallback.
    pub fn counting(self) -> bool {
        self.on_time.is_some()
    }

    /// The boundary before's readback came on time (`true`), or did not.
    pub fn observe(&mut self, came: bool) {
        if let Some(run) = &mut self.on_time {
            *run = if came { run.saturating_add(1) } else { 0 };
        }
    }

    /// `pick` as the gate lets it through: a GPU pick before
    /// [`NDI_RETURN_ON_TIME`] on-time readbacks in a row since a fallback
    /// stays on the canvas.
    pub fn admit(self, pick: NdiPick) -> NdiPick {
        match (pick, self.on_time) {
            (NdiPick::Gpu, Some(run)) if run < NDI_RETURN_ON_TIME => NdiPick::Cpu(CpuWhy::Settling),
            _ => pick,
        }
    }

    /// After the pick of a boundary whose boundary before came from
    /// `previous`: a fallback from the GPU (not the setting's) arms the
    /// gate, the GPU or the setting's canvas disarms it, the canvas keeps
    /// it as it is.
    pub fn after(&mut self, previous: NdiSource, pick: NdiPick) {
        self.on_time = match pick {
            NdiPick::Gpu | NdiPick::Cpu(CpuWhy::Setting) => None,
            NdiPick::Cpu(_) if previous == NdiSource::Gpu => Some(0),
            NdiPick::Cpu(_) => self.on_time,
        };
    }
}

/// The NDI delay of `source`, in grid slots: one for the GPU readback.
pub fn delay_slots(source: NdiSource) -> u32 {
    match source {
        NdiSource::Gpu => 1,
        NdiSource::Cpu => 0,
    }
}

/// The FourCC `source` sends.
pub fn fourcc(source: NdiSource) -> &'static str {
    match source {
        NdiSource::Gpu => "UYVY",
        NdiSource::Cpu => "NV12",
    }
}

/// `GET /api/v1/program` → `ndi` (#279 P1b).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NdiStatus {
    /// `program_ndi_source` as last applied: `gpu` or `cpu`.
    pub setting: &'static str,
    /// The source of the last boundary: `gpu` or `cpu`.
    pub source: &'static str,
    /// 1 from the GPU (one boundary late), else 0.
    pub delay_slots: u32,
    /// `UYVY` from the GPU, `NV12` from the canvas.
    pub fourcc: &'static str,
    /// GPU boundaries sent with the last picture (or the black before any):
    /// the readback of that boundary had not come.
    pub repeats: u64,
    /// Pictures read back after their boundary was sent: dropped.
    pub readback_late: u64,
    /// Pictures dropped because [`NDI_FEED_DEPTH`] newer ones waited.
    pub feed_coalesced: u64,
    /// Changes of the source, either way.
    pub switches: u64,
    /// The p99 over the last `MAX_STAT_WINDOW` (900) readbacks, µs: the map
    /// and the row copy of the UYVY staging texture; and the UYVY pass's
    /// own part of those composes' draw (`ComposeStats::uyvy_us`). A Black
    /// boundary's constant black is no readback: it adds to neither.
    pub readback_us_p99: u64,
    pub uyvy_us_p99: u64,
}

/// The feed and the NDI telemetry, under one lock. The p99 windows are
/// MAX's (`program_max::Window`, the last `MAX_STAT_WINDOW`).
#[derive(Default)]
struct FeedState {
    core: FeedCore,
    /// The source of the last boundary the `program-output` thread served.
    source: Option<NdiSource>,
    repeats: u64,
    switches: u64,
    readback: Window,
    uyvy: Window,
}

/// The hand-off of NDI's read-back pictures and NDI's telemetry
/// (`MaxOut::ndi`).
#[derive(Default)]
pub struct NdiFeed {
    state: Mutex<FeedState>,
}

impl NdiFeed {
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `program-max` thread: boundary `stamp_100ns`'s picture.
    pub fn push(&self, stamp_100ns: i64, picture: SharedFrame) -> Pushed {
        self.lock().core.push(stamp_100ns, picture)
    }

    /// The `program-output` thread: boundary `stamp_100ns`'s picture, if it
    /// came (every older one goes).
    pub fn take(&self, stamp_100ns: i64) -> Option<SharedFrame> {
        self.lock().core.take(stamp_100ns)
    }

    /// The `program-output` thread, on the canvas by the setting, after the
    /// held pair's flush: let the pictures go (`FeedCore::clear`), so no
    /// 4 MB buffer stays out of `frame_pool`. Only that thread clears
    /// (#279 P1b review round 5): only it knows the held pair took its
    /// picture.
    pub fn clear(&self) {
        self.lock().core.clear();
    }

    /// The stamp of the newest picture kept: the readback's freshness.
    pub fn newest_stamp(&self) -> Option<i64> {
        self.lock().core.newest()
    }

    /// The `program-max` thread: one readback's map + copy and the UYVY
    /// pass's part of its compose, µs (`None`: not measured).
    pub fn record_readback(&self, readback_us: u64, uyvy_us: Option<u64>) {
        let mut state = self.lock();
        state.readback.push(readback_us);
        if let Some(us) = uyvy_us {
            state.uyvy.push(us);
        }
    }

    /// The `program-output` thread served a boundary from `source`; it
    /// changed the source (`switched`), it repeated a picture (`repeated`).
    pub fn record_boundary(&self, source: NdiSource, switched: bool, repeated: bool) {
        let mut state = self.lock();
        state.source = Some(source);
        state.switches += u64::from(switched);
        state.repeats += u64::from(repeated);
    }

    /// The `ndi` block, with `setting` as last applied. The windows are
    /// copied under the lock and sorted after it.
    pub fn status(&self, setting: NdiSource) -> NdiStatus {
        let (source, counts, windows) = {
            let state = self.lock();
            (
                state.source.unwrap_or(NdiSource::Cpu),
                [
                    state.repeats,
                    state.core.late,
                    state.core.coalesced,
                    state.switches,
                ],
                (state.readback.clone(), state.uyvy.clone()),
            )
        };
        let [repeats, readback_late, feed_coalesced, switches] = counts;
        NdiStatus {
            setting: setting.as_str(),
            source: source.as_str(),
            delay_slots: delay_slots(source),
            fourcc: fourcc(source),
            repeats,
            readback_late,
            feed_coalesced,
            switches,
            readback_us_p99: windows.0.p99(),
            uyvy_us_p99: windows.1.p99(),
        }
    }
}

#[cfg(test)]
#[path = "ndi_feed_tests.rs"]
mod tests;
