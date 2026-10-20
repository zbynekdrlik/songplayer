//! #279 P2: a boundary's picture, in CPU memory or in VRAM.
//!
//! Every decoded picture used to be a CPU [`SharedFrame`] from the reader
//! to the end. The GPU-resident path (epic #279, plan §5.4; the P2 design
//! comment 6237269720) keeps a hardware-decoded picture in a ring slot of
//! its pipeline ([`GpuFrame`], from P3a), so the pacer, the handoff, the
//! program bus, MAX's queue and the held tails carry a [`Picture`]: either
//! kind, shared by `Arc` (a clone is a refcount bump, never a pixel copy).
//! They only clone it, compare it ([`Picture::ptr_eq`]) and drop it; MAX
//! draws either kind (`MaxPicture::side`: an upload, or the slot sampled in
//! place).
//!
//! Production is still all [`Picture::Cpu`] in P2. The consumers that read
//! the bytes on the CPU (`SP-program`'s canvas fit and fade, a held step's
//! preview tap) take a CPU picture only: a GPU one reaching one of them is
//! a MISS ([`Picture::cpu_for`]): counted by the consumer
//! ([`CpuMisses`]), WARNed at most once a minute with the count, and the
//! consumer shows black (the canvas) or no picture (the preview), never a
//! panic. The preview gets its GPU path in P3b; the canvas never does (NDI
//! can take the compositor's readback since P1b, `program_ndi_source=gpu`,
//! default `cpu`; P5 deletes the canvas), so a GPU picture there stays
//! black whenever NDI sends the canvas (#279: a P3a precondition).

use std::fmt;

use sp_gpu::GpuFrame;
use tracing::warn;

use crate::playback::fleet_shift;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::stat_window::WarnLimiter;

/// A consumer's misses are WARNed at most once per this (100 ns of the
/// timeline: a minute).
pub const CPU_MISS_WARN_EVERY_100NS: i64 = 600_000_000;

/// One boundary's picture: NV12 bytes in CPU memory, or a decoded picture
/// in a ring slot on the GPU.
#[derive(Clone)]
pub enum Picture {
    /// NV12 in CPU memory (a software decode, the NDI input, a standby
    /// black), shared by `Arc` and recycled to the frame pool on its last
    /// drop.
    Cpu(SharedFrame),
    /// A decoded picture in a pipeline's ring slot (#279 P3a), shared by
    /// `Arc`; its last drop frees the slot.
    Gpu(GpuFrame),
}

impl Picture {
    /// The CPU picture's bytes; `None` for a GPU picture (a consumer that
    /// must read bytes takes [`cpu_for`](Self::cpu_for), which counts it).
    pub fn cpu(&self) -> Option<&SharedFrame> {
        match self {
            Picture::Cpu(frame) => Some(frame),
            Picture::Gpu(_) => None,
        }
    }

    /// The ring frame; `None` for a CPU picture.
    pub fn gpu(&self) -> Option<&GpuFrame> {
        match self {
            Picture::Cpu(_) => None,
            Picture::Gpu(frame) => Some(frame),
        }
    }

    /// The CPU picture's bytes, for a test that built one (a GPU picture
    /// there is the test's own bug).
    #[cfg(test)]
    pub(crate) fn expect_cpu(&self) -> &SharedFrame {
        self.cpu()
            .expect("the test built a CPU picture (Picture::Cpu)")
    }

    /// The bytes it holds in CPU memory: a CPU picture's length, 0 for a
    /// GPU one (it holds none; a byte-size check refuses it).
    pub fn len(&self) -> usize {
        self.cpu().map_or(0, |frame| frame.len())
    }

    /// Whether it holds no bytes in CPU memory.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `other` is the SAME picture: a clone of the same CPU
    /// allocation, or of the same ring frame (an `Arc` identity, never a
    /// byte comparison).
    pub fn ptr_eq(&self, other: &Picture) -> bool {
        match (self, other) {
            (Picture::Cpu(a), Picture::Cpu(b)) => a.ptr_eq(b),
            (Picture::Gpu(a), Picture::Gpu(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    /// The bytes for the CPU consumer `consumer`: a CPU picture's; a GPU
    /// picture is a miss, counted (and WARNed, rate-limited) in `misses`,
    /// and `None`: the consumer shows black or nothing, never a panic.
    pub fn cpu_for(&self, consumer: CpuUse, misses: &mut CpuMisses) -> Option<&SharedFrame> {
        match self {
            Picture::Cpu(frame) => Some(frame),
            Picture::Gpu(frame) => {
                misses.note_now(consumer, frame);
                None
            }
        }
    }
}

impl From<SharedFrame> for Picture {
    fn from(frame: SharedFrame) -> Self {
        Picture::Cpu(frame)
    }
}

impl From<GpuFrame> for Picture {
    fn from(frame: GpuFrame) -> Self {
        Picture::Gpu(frame)
    }
}

/// The kind and size, never the pixels (`PacedFrame` and `SubmitJob`
/// print through it).
impl fmt::Debug for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Picture::Cpu(frame) => f.debug_tuple("Cpu").field(frame).finish(),
            Picture::Gpu(frame) => f.debug_tuple("Gpu").field(frame).finish(),
        }
    }
}

/// A consumer that reads a picture's bytes on the CPU (P2: the ones a GPU
/// picture can reach once P3a makes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuUse {
    /// `SP-program`'s canvas fit and fade (`program_output.rs`): black.
    Canvas,
    /// The preview taps, from a held step past the video's end
    /// (`paced_producer.rs`): no picture, the audio still offered.
    Preview,
}

impl CpuUse {
    /// What the consumer shows of a GPU picture, for its WARN.
    pub fn shows(self) -> &'static str {
        match self {
            CpuUse::Canvas => {
                "shown as black: the canvas never reads ring frames (#279 P3a precondition)"
            }
            CpuUse::Preview => {
                "no picture offered, its audio kept (#279 P3b gives the preview its GPU path)"
            }
        }
    }
}

/// A CPU consumer's misses: GPU pictures it could not read.
#[derive(Debug)]
pub struct CpuMisses {
    count: u64,
    warn: WarnLimiter,
    /// The timeline the WARN's rate limit reads
    /// (`fleet_shift::timeline_now_100ns`).
    clock: fn() -> i64,
}

impl Default for CpuMisses {
    fn default() -> Self {
        Self {
            count: 0,
            warn: WarnLimiter::default(),
            clock: fleet_shift::timeline_now_100ns,
        }
    }
}

impl CpuMisses {
    /// [`note`](Self::note) at the timeline's now (the misses' clock).
    pub fn note_now(&mut self, consumer: CpuUse, frame: &GpuFrame) -> bool {
        let now_100ns = (self.clock)();
        self.note(consumer, frame, now_100ns)
    }

    /// `frame` reached the CPU consumer `consumer` at `at_100ns` (the
    /// timeline): counted; a WARN at most once per
    /// [`CPU_MISS_WARN_EVERY_100NS`], with the misses held back since and
    /// the total (a GPU picture there is a P3a / P3b gap: never a
    /// per-boundary flood, never silent for long). Whether it was WARNed.
    pub fn note(&mut self, consumer: CpuUse, frame: &GpuFrame, at_100ns: i64) -> bool {
        self.count += 1;
        let Some(held_back) = self.warn.admit(at_100ns, CPU_MISS_WARN_EVERY_100NS) else {
            return false;
        };
        warn!(
            ?consumer,
            shows = consumer.shows(),
            frame = ?frame,
            held_back,
            total = self.count,
            "a GPU picture reached a CPU-only consumer"
        );
        true
    }

    /// How many GPU pictures the consumer missed.
    pub fn count(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
#[path = "picture_tests.rs"]
mod tests;
