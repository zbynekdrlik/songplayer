//! #279 P1b: NDI `SP-program` from the GPU readback, ONE boundary late —
//! the delay line of the `SP-program` sender (an impl split of
//! [`ProgramOutput`], a child module of `program_output.rs`, which is at
//! the 1000-line cap). Design: #279 comment 6218637444; plan
//! `docs/superpowers/plans/2026-10-16-gpu-video-path.md` §5.6.
//!
//! `serve` stays `split` → `limit` → `feed_outputs` (VBAN, boundary b) →
//! `offer_max` (b) → the quiet wait → the video side, which is
//! [`ProgramOutput::submit_ndi`]:
//!
//! - NDI's source for b is decided first ([`ndi_source_for`]): the GPU
//!   readback while `program_ndi_source` = `gpu` and the feed's newest
//!   picture is at most `NDI_FEED_STALE_SLOTS` behind b, else the CPU
//!   canvas, so a compositor that cannot run (off Windows, no adapter, a
//!   device backoff) never freezes NDI. After an automatic fallback the
//!   way back is gated (`ReturnGate`, #279 ROZHODNUTÉ 6232669201 item 3):
//!   each boundary on the canvas takes the boundary before's readback as
//!   the GPU would have, and only `NDI_RETURN_ON_TIME` on time in a row
//!   bring NDI back, so it never flaps boundary by boundary. A change
//!   either way is logged once, rate-limited (a `StateLog`), and counted
//!   (`ndi.switches`).
//! - From the CPU canvas: `submit_video`, as before (#223), stamped b.
//! - From the GPU: the pair HELD at b − 1 goes out now, with its audio
//!   after the limiter (moved in, no copy; the standby silence for a
//!   standby pair), its stamps, its item frame (the #228 burn) and its
//!   picture: the compositor's UYVY readback of b − 1 (`ndi_feed.rs`), the
//!   canvas's size, stride `width · 2`. No canvas fit, no fade picture, no
//!   band-pool work: a fade's audio is still crossfaded in `split`, its
//!   picture is the compositor's fade. Then b's pair is held. A picture that
//!   did not come (the worker late, a coalesce, a failed compose) repeats
//!   the last one sent (`ndi.repeats`), the constant UYVY black before any:
//!   never a wait. A picture that is not whole UYVY for the canvas is never
//!   sent (a repeat too).
//! - A switch to the GPU holds b and sends nothing (one missing frame). A
//!   switch to the CPU never drops an audio block (#279 ROZHODNUTÉ
//!   6232669201 item 1): the held pair goes out FIRST (its readback, else
//!   the last picture, else the black: a repeat), then b on the canvas —
//!   two submits back to back, contiguous stamps.
//! - The last picture: every boundary on the canvas forgets it (no 4 MB
//!   stays pinned while NDI is on the canvas). A switch to the GPU takes
//!   the boundary before's readback as the last picture (the gate's
//!   on-time take, or `take_on_time` at the switch), so a picture missing
//!   right after it repeats that boundary, the black only when the
//!   boundary before was not read back (review round 3: a first entry used
//!   to send the black) — or was let go by a canvas boundary between, a
//!   gpu → cpu → gpu flip within one boundary, which the 5 s settings poll
//!   never makes.
//! - The feed: this output ALONE lets its pictures go, at every canvas
//!   boundary under the setting, after the held pair's flush took its
//!   picture (review rounds 4-5). The `program-max` thread used to let
//!   them go too, and usually saw the setting's switch first: the held
//!   pair then repeated the picture before (round 5 F1). A picture the
//!   thread pushes after the switch goes at the next such boundary.
//!
//! So picture and audio move together, the stamps are b − 1's exactly as
//! the canvas would have stamped them, and the delay is a constant slot
//! (camera-box's receivers latch their FIFO depth from the arrival lag).
//! `health.timing.submit_us` is the NDI submit alone (at a switch to the
//! canvas, the held pair's submit and b's canvas picture), and the trace's
//! `wire_100ns` names the stamp that went out (b itself at a switch to the
//! canvas, where b − 1 went out just before it).
//!
//! The #228 burn lives here too ([`ProgramOutput::burn`]): on the canvas
//! picture (NV12) or on the readback (UYVY), into a copy.

use sp_core::config::NdiSource;
use sp_gpu::FHD_TARGET;
use sp_ndi::{AudioFrame, NdiBackend, PixelFormat};
use tracing::{info, warn};

use super::{PROGRAM_STANDBY_H, PROGRAM_STANDBY_W, Pair, ProgramOutput};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::ndi_feed::{
    BlackUyvy, CpuWhy, NDI_RETURN_ON_TIME, NDI_SWITCH_LOG_EVERY_100NS, NdiPick, ReturnGate,
    ndi_source_for, uyvy_len,
};
use crate::playback::program_burn;
use crate::playback::program_item::ItemFrame;
use crate::playback::stat_window::StateLog;

// The GPU reads back the compositor's primary target, the FHD program: the
// size `SP-program`'s canvas always is (#223).
const _: () =
    assert!(PROGRAM_STANDBY_W == FHD_TARGET.width && PROGRAM_STANDBY_H == FHD_TARGET.height);

/// A boundary's pair, held for the next boundary's NDI submit.
struct Held {
    stamp_100ns: i64,
    /// Its audio after the limiter, moved in (`None`: the standby silence).
    audio: Option<Vec<AudioFrame>>,
    audio_tc_100ns: i64,
    /// The item frame it shows (#228: its burn).
    shown: Option<ItemFrame>,
    /// A window boundary: it joins the run of mixed boundaries when it goes
    /// out.
    mixed: bool,
}

impl Held {
    /// `pair`, served on `stamp_100ns`: a forwarded pair keeps its audio
    /// stamp; what the program makes itself (a standby pair, a mixed block)
    /// is stamped on its boundary (#224).
    fn of(pair: Pair, stamp_100ns: i64, shown: Option<ItemFrame>) -> Self {
        let (audio, audio_tc_100ns, mixed) = match pair {
            Pair::Source(job) => (Some(job.audio), job.audio_tc_100ns, false),
            Pair::Standby => (None, stamp_100ns, false),
            Pair::Mix { audio, .. } => (Some(audio), stamp_100ns, true),
        };
        Self {
            stamp_100ns,
            audio,
            audio_tc_100ns,
            shown,
            mixed,
        }
    }
}

/// The delay line's state.
#[derive(Default)]
pub(super) struct NdiLine {
    /// NDI's source at the boundary before.
    source: NdiSource,
    /// The pair held for the next boundary (from the GPU).
    held: Option<Held>,
    /// The last read-back picture sent, or taken at a switch to the GPU:
    /// what a missing one repeats. Every canvas boundary forgets it.
    last: Option<SharedFrame>,
    /// The constant UYVY black, sent before any picture came.
    black: BlackUyvy,
    /// What the switch lines last said of the source (the canvas at the
    /// start), rate-limited; every switch is counted all the same.
    switch_log: StateLog<NdiSource>,
    /// The way back to the GPU after an automatic fallback.
    rejoin: ReturnGate,
    /// The stamp of the boundary before (what the gate takes from the
    /// feed while it counts).
    prev: Option<i64>,
}

impl<B: NdiBackend> ProgramOutput<B> {
    /// The video side of the boundary on `stamp_100ns` (VBAN and MAX
    /// already have it): NDI's source for it, then its canvas picture
    /// (`submit_video`) or, from the GPU, the pair held at the boundary
    /// before with its read-back picture, `pair` held in its place. Returns
    /// when the video side started (read off `now`) and the stamp NDI got
    /// (`stamp_100ns` itself when nothing went out).
    pub(super) fn submit_ndi(
        &mut self,
        pair: Pair,
        stamp_100ns: i64,
        shown: Option<ItemFrame>,
        now: &impl Fn() -> i64,
    ) -> (i64, i64) {
        let (setting, newest) = match &self.max {
            Some(max) => (max.ndi_source(), max.ndi.newest_stamp()),
            None => (NdiSource::Cpu, None),
        };
        if setting == NdiSource::Gpu && self.ndi.rejoin.counting() {
            let came = self.take_on_time();
            self.ndi.rejoin.observe(came);
        }
        let pick = self
            .ndi
            .rejoin
            .admit(ndi_source_for(setting, newest, stamp_100ns));
        let previous = self.ndi.source;
        let switched = previous != pick.source();
        if switched && pick == NdiPick::Gpu {
            // The boundary before's readback is the last picture (the gate
            // already took it when it counted this boundary).
            self.take_on_time();
        }
        let every = NDI_SWITCH_LOG_EVERY_100NS;
        let line = self
            .ndi
            .switch_log
            .observe(stamp_100ns, every, pick.source());
        log_switch(line.map(|held_back| (pick, held_back)));
        self.ndi.rejoin.after(previous, pick);
        self.ndi.source = pick.source();
        self.ndi.prev = Some(stamp_100ns);
        let (start, sent, repeated) = match pick {
            NdiPick::Cpu(why) => {
                // ROZHODNUTÉ 6232669201 item 1: the held pair goes out
                // first (no audio block is ever lost), then b on the canvas.
                let flushed = self
                    .ndi
                    .held
                    .take()
                    .map(|held| (now(), self.send_held(held)));
                self.ndi.last = None;
                // Nothing takes the feed's pictures under the setting: this
                // output alone lets them go, after the flush took its
                // picture, at every such boundary (review round 5 F1: the
                // thread usually saw the setting first and let the held
                // pair's picture go; one it pushes late goes next time).
                if why == CpuWhy::Setting
                    && let Some(max) = &self.max
                {
                    max.ndi.clear();
                }
                let canvas_start = self.submit_video(pair, stamp_100ns, shown, now);
                match flushed {
                    Some((start, repeated)) => (start, stamp_100ns, repeated),
                    None => (canvas_start, stamp_100ns, false),
                }
            }
            NdiPick::Gpu => {
                let start = now();
                let next = Held::of(pair, stamp_100ns, shown);
                match self.ndi.held.replace(next) {
                    Some(held) => (start, held.stamp_100ns, self.send_held(held)),
                    None => (start, stamp_100ns, false),
                }
            }
        };
        if let Some(max) = &self.max {
            max.ndi.record_boundary(pick.source(), switched, repeated);
        }
        (start, sent)
    }

    /// The boundary before's read-back picture, taken from the feed as the
    /// GPU would have taken it (after an automatic fallback, on the canvas;
    /// and at a switch to the GPU); whether it came on time. It is kept as
    /// the last picture: the first one missing after a switch repeats it.
    fn take_on_time(&mut self) -> bool {
        match self.ndi.prev.and_then(|prev| self.take_whole(prev)) {
            Some(picture) => {
                self.ndi.last = Some(picture);
                true
            }
            None => false,
        }
    }

    /// Boundary `stamp_100ns`'s read-back picture, taken from the feed
    /// (every older one goes with it), when it came whole UYVY for the
    /// canvas.
    fn take_whole(&self, stamp_100ns: i64) -> Option<SharedFrame> {
        let canvas = self.canvas.layout();
        let whole = uyvy_len(canvas.width, canvas.height);
        self.max
            .as_ref()
            .and_then(|max| max.ndi.take(stamp_100ns))
            .filter(|picture| picture.len() >= whole)
    }

    /// Send `held` (the boundary before's pair) with its read-back picture
    /// when it came whole, else the last one sent (the black before any);
    /// whether it repeated.
    fn send_held(&mut self, held: Held) -> bool {
        let canvas = self.canvas.layout();
        let (width, height) = (canvas.width, canvas.height);
        let read = self.take_whole(held.stamp_100ns);
        let repeated = read.is_none();
        let video = match read {
            Some(picture) => {
                self.ndi.last = Some(picture.clone());
                picture
            }
            None => match &self.ndi.last {
                Some(last) => last.clone(),
                None => self.ndi.black.get(width, height),
            },
        };
        let video = self.burn(video, held.shown, PixelFormat::Uyvy);
        if held.mixed {
            self.mix_run.boundaries += 1;
        }
        let audio = held.audio.as_deref().unwrap_or(&self.silence[..]);
        self.submitter.submit_frame_at_boundary_owned(
            width,
            height,
            width * 2,
            video,
            audio,
            held.stamp_100ns,
            held.audio_tc_100ns,
            PixelFormat::Uyvy,
        );
        repeated
    }

    /// The picture NDI got last (the async holdover the SDK points at): the
    /// tests' view of the wire, from any module.
    #[cfg(test)]
    pub(crate) fn wire_frame(&self) -> Option<&SharedFrame> {
        self.submitter.held_frame()
    }

    /// #228: `video` with the 911014 burn of the item frame it `shown`s,
    /// painted into a copy while the switch is on — on the canvas picture
    /// (NV12) or (#279 P1b) on the read-back UYVY one —; else `video`
    /// itself. Counts every boundary that went out burned.
    pub(super) fn burn(
        &self,
        video: SharedFrame,
        shown: Option<ItemFrame>,
        format: PixelFormat,
    ) -> SharedFrame {
        let Some(item) = self.item.as_ref().filter(|item| item.burn_on()) else {
            return video;
        };
        let Some(shown) = shown else {
            return video;
        };
        let gen_ts_ns = shown.wire_100ns * 100;
        let canvas = self.canvas.layout();
        let (width, height) = (canvas.width, canvas.height);
        let burned = match format {
            PixelFormat::Uyvy => {
                program_burn::burned_uyvy(&video, width, height, shown.frame, gen_ts_ns)
            }
            _ => program_burn::burned(&video, canvas, shown.frame, gen_ts_ns),
        };
        match burned {
            Some(burned) => {
                item.count_burned();
                burned
            }
            None => video,
        }
    }
}

/// The one line of a change of NDI's source (the switch log's line: the
/// pick NDI is on now, and the boundaries its rate limit held back before
/// it). An automatic fallback is a WARN (NDI is degraded until the
/// readback comes back on time); the setting's switch and the way back to
/// the GPU are an INFO. The decisions are `StateLog`'s and the gate's,
/// tested; logging only.
#[cfg_attr(test, mutants::skip)]
fn log_switch(line: Option<(NdiPick, u64)>) {
    match line {
        Some((NdiPick::Gpu, held_back)) => info!(
            held_back,
            "program output: NDI SP-program now from the GPU readback (UYVY, one boundary late)"
        ),
        Some((NdiPick::Cpu(CpuWhy::Setting), held_back)) => info!(
            reason = CpuWhy::Setting.as_str(),
            held_back, "program output: NDI SP-program falls back to the CPU canvas"
        ),
        Some((NdiPick::Cpu(why), held_back)) => warn!(
            reason = why.as_str(),
            held_back,
            on_time_needed = NDI_RETURN_ON_TIME,
            "program output: NDI SP-program falls back to the CPU canvas - back on the GPU after on_time_needed on-time readbacks in a row"
        ),
        None => {}
    }
}
