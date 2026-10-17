//! #279 P2: the picture ring's shared state and its frame handle
//! (cross-platform).
//!
//! A ring's state ([`RingState`], pure) lives behind one lock with a
//! condvar ([`RingCore`]). Its writer (a pipeline's decode thread, from
//! P3a) takes slots and waits, bounded, while every slot is in use; a
//! [`GpuFrame`]'s drop, on whatever thread drops its last clone (the
//! pacer's, the bus's, the `program-max` thread's), frees its slot and
//! wakes that writer. The Windows ring (`PictureRing`, `win/ring.rs`) adds
//! the textures and their NT handles; [`FakeRing`] (doc-hidden) adds none,
//! so the pacer, bus and MAX tests run with GPU frames on Linux, and the
//! waits and frees are tested here.

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tracing::warn;

use crate::error::GpuError;
use crate::picture::MAX_PICTURE_SIDE;
use crate::ring_state::{
    KEY_PRODUCER, RingFormat, RingState, SizeClass, SlotKey, SlotState, Take, copy_box,
    hresult_text,
};

/// The number of the next ring made in this process (ring numbers name a
/// slot's texture in the compositor's open slots, [`SlotKey`]).
static NEXT_RING: AtomicU64 = AtomicU64::new(1);

/// The id of the next frame handed over in this process.
static NEXT_FRAME: AtomicU64 = AtomicU64::new(1);

/// A ring's counters: what its pipeline reports (#279 P3a's
/// `status.video_decode.pipelines[].ring`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RingStats {
    /// Slots that hold a texture.
    pub slots: usize,
    /// Slots a frame refers to.
    pub in_use: usize,
    /// Writes that found every slot in use and waited.
    pub waits: u64,
    /// Writes that waited the whole bound and failed
    /// ([`GpuError::RingFull`]).
    pub full: u64,
    /// The ring is dead ([`GpuError::RingDead`]).
    pub dead: bool,
}

/// The slot a write got ([`RingCore::take`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Taken {
    pub slot: usize,
    pub alloc_gen: u64,
    /// The slot's texture must be made first (the slot held none, or one
    /// of another class). Read by the Windows ring only.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub create: bool,
    /// The key the write acquires the slot's mutex with. Read by the
    /// Windows ring only.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub key: u64,
}

/// One ring's state behind its lock, the condvar a frame's drop notifies,
/// and its counters.
pub(crate) struct RingCore {
    id: u64,
    state: Mutex<RingState>,
    freed: Condvar,
    waits: AtomicU64,
    full: AtomicU64,
}

impl RingCore {
    /// A ring of `capacity` empty slots, under the next ring number.
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_RING.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(RingState::new(capacity)),
            freed: Condvar::new(),
            waits: AtomicU64::new(0),
            full: AtomicU64::new(0),
        })
    }

    /// The ring's process-wide number.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// The state, through a poisoned lock too (a panic elsewhere never
    /// stops a frame's drop from freeing its slot).
    fn lock(&self) -> MutexGuard<'_, RingState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A slot for a write of `class` ([`RingState::take`]): while every
    /// slot is in use it waits for a frame to drop, at most `limit`
    /// (production `ring_state::RING_WAIT`), counted in
    /// [`RingStats::waits`]; still full then is [`GpuError::RingFull`],
    /// counted in [`RingStats::full`]. A dead ring is
    /// [`GpuError::RingDead`]. The slot is in use from here: a write that
    /// fails gives it back with [`release`](Self::release).
    pub(crate) fn take(&self, class: SizeClass, limit: Duration) -> Result<Taken, GpuError> {
        let state = self.lock();
        let mut state = if state.has_room() {
            state
        } else {
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.freed
                .wait_timeout_while(state, limit, |state| !state.has_room())
                .unwrap_or_else(|p| p.into_inner())
                .0
        };
        match state.take(class) {
            Take::Full => {
                self.full.fetch_add(1, Ordering::Relaxed);
                Err(GpuError::RingFull {
                    waited_ms: u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                })
            }
            Take::Dead => Err(GpuError::RingDead),
            Take::Reuse {
                slot,
                alloc_gen,
                key,
            } => Ok(Taken {
                slot,
                alloc_gen,
                create: false,
                key,
            }),
            Take::Create { slot, alloc_gen } => Ok(Taken {
                slot,
                alloc_gen,
                create: true,
                key: KEY_PRODUCER,
            }),
        }
    }

    /// Slot `slot` (allocation `alloc_gen`) is free again: its last frame
    /// dropped, or its write failed. A waiting writer wakes.
    pub(crate) fn release(&self, slot: usize, alloc_gen: u64) {
        self.lock().release(slot, alloc_gen);
        self.freed.notify_all();
    }

    /// Kill the ring: every later write is [`GpuError::RingDead`]; a
    /// waiting writer wakes to see it. Its first kill is logged once, with
    /// `why` and the `hresult` that told it, if any; whether this call
    /// killed it.
    pub(crate) fn kill(&self, why: &'static str, hresult: Option<u32>) -> bool {
        let first = self.lock().kill();
        self.freed.notify_all();
        if first {
            warn!(
                ring = self.id,
                why,
                hresult = %hresult_text(hresult),
                "sp-gpu: a picture ring died: its pipeline decodes in software from here (#279)"
            );
        }
        first
    }

    /// Whether the ring is dead.
    pub(crate) fn is_dead(&self) -> bool {
        self.lock().is_dead()
    }

    /// What slot `slot` holds now (the Windows ring keeps its textures in
    /// step with it).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn slot(&self, slot: usize) -> Option<SlotState> {
        self.lock().slot(slot)
    }

    /// The ring's counters.
    pub(crate) fn stats(&self) -> RingStats {
        let state = self.lock();
        RingStats {
            slots: state.made(),
            in_use: state.in_use(),
            waits: self.waits.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
            dead: state.is_dead(),
        }
    }
}

/// What a written slot hands over ([`hand_over`]).
pub(crate) struct Written {
    pub slot: usize,
    pub alloc_gen: u64,
    pub class: SizeClass,
    /// The picture's own size, inside the slot's (even) texture.
    pub size: (u32, u32),
    /// The slot texture's NT handle (the compositor opens it);
    /// `FAKE_HANDLE_BASE + 4·(slot + 1)` on the fake ring.
    pub handle: isize,
    /// Kept alive while a frame refers to the slot: on Windows the NT
    /// handle, so the compositor can open it whatever the ring does.
    pub keep: Option<Arc<dyn Any + Send + Sync>>,
}

/// The frame of a slot its writer released with `KEY_CONSUMER`: the slot is
/// marked written, and the frame, under the next frame id, refers to it
/// until its last clone drops.
pub(crate) fn hand_over(core: &Arc<RingCore>, written: Written) -> GpuFrame {
    core.lock().written(written.slot, written.alloc_gen);
    GpuFrame(Arc::new(FrameInner {
        core: Arc::clone(core),
        slot: written.slot,
        alloc_gen: written.alloc_gen,
        id: NEXT_FRAME.fetch_add(1, Ordering::Relaxed),
        class: written.class,
        size: written.size,
        handle: written.handle,
        keep: written.keep,
    }))
}

/// One decoded picture in a ring slot (VRAM), shared by `Arc`: a clone is a
/// refcount bump, exactly sp-server's `SharedFrame` contract. While a clone
/// lives the slot is not written again; the last one's drop frees it. The
/// [`id`](Self::id) names the picture: no two frames of a process share one,
/// so it labels the picture for the compositor (sp-server's `PictureIds`).
#[derive(Clone)]
pub struct GpuFrame(Arc<FrameInner>);

struct FrameInner {
    core: Arc<RingCore>,
    slot: usize,
    alloc_gen: u64,
    id: u64,
    class: SizeClass,
    size: (u32, u32),
    handle: isize,
    /// Held only so its drop runs after the frame's (the NT handle).
    #[allow(dead_code)]
    keep: Option<Arc<dyn Any + Send + Sync>>,
}

impl Drop for FrameInner {
    fn drop(&mut self) {
        self.core.release(self.slot, self.alloc_gen);
    }
}

impl GpuFrame {
    /// The picture's process-wide id.
    pub fn id(&self) -> u64 {
        self.0.id
    }

    /// The picture's width (its slot's texture may be one column wider).
    pub fn width(&self) -> u32 {
        self.0.size.0
    }

    /// The picture's height (its slot's texture may be one row taller).
    pub fn height(&self) -> u32 {
        self.0.size.1
    }

    /// The size of the slot's texture: the picture's, rounded up to even.
    pub fn texture_size(&self) -> (u32, u32) {
        (self.0.class.width, self.0.class.height)
    }

    /// The slot texture's format.
    pub fn format(&self) -> RingFormat {
        self.0.class.format
    }

    /// The slot's texture: the ring, the slot and its allocation.
    pub fn key(&self) -> SlotKey {
        SlotKey {
            ring: self.0.core.id(),
            slot: self.0.slot,
            alloc_gen: self.0.alloc_gen,
        }
    }

    /// The slot texture's NT handle, as a number (the compositor opens it
    /// with `OpenSharedResource1`; valid while this frame lives).
    #[doc(hidden)]
    pub fn shared_handle(&self) -> isize {
        self.0.handle
    }

    /// Whether `other` is a clone of this frame (an `Arc` identity).
    pub fn ptr_eq(&self, other: &GpuFrame) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The compositor found the slot's key abandoned (a device was lost
    /// holding it: the producer's, or its own before its rebuild), or the
    /// slot failed on a healthy compositor (`ring_state::slot_draw`): the
    /// ring dies, and its writer goes on in software. `why` and the
    /// `hresult` that told it go into the ring's one WARN; whether this
    /// call killed it.
    pub fn kill_ring(&self, why: &'static str, hresult: Option<u32>) -> bool {
        self.0.core.kill(why, hresult)
    }

    /// Whether the frame's ring is dead.
    pub fn ring_dead(&self) -> bool {
        self.0.core.is_dead()
    }
}

/// The frame's id, picture size and slot, never anything else.
impl fmt::Debug for GpuFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuFrame")
            .field("id", &self.0.id)
            .field("width", &self.0.size.0)
            .field("height", &self.0.size.1)
            .field("ring", &self.0.core.id())
            .field("slot", &self.0.slot)
            .finish()
    }
}

/// The made-up handle of a [`FakeRing`] slot `slot`:
/// `FAKE_HANDLE_BASE + 4 × (slot + 1)`, a value no Windows handle has (a
/// real one is a small positive multiple of 4, a pseudo-handle -1 … -6),
/// so a fake frame that ever reached a real compositor opens nothing.
pub const FAKE_HANDLE_BASE: isize = isize::MIN;

/// A ring with no textures, for tests on every platform (sp-server's pacer,
/// bus and MAX tests): its frames take and free slots exactly as the
/// Windows ring's do, and carry a made-up handle ([`FAKE_HANDLE_BASE`]), so
/// a test sees whose slot a frame holds. Not for production.
#[doc(hidden)]
#[derive(Clone)]
pub struct FakeRing {
    core: Arc<RingCore>,
}

impl FakeRing {
    /// A fake ring of `capacity` slots.
    pub fn new(capacity: usize) -> Self {
        Self {
            core: RingCore::new(capacity),
        }
    }

    /// A frame of a `width`×`height` picture, written as the Windows ring
    /// writes one: its slot taken (waiting at most `wait` while every slot
    /// is in use), then marked written. [`GpuError::RingPicture`] for a
    /// picture no slot takes (empty, over 16384 on a side),
    /// [`GpuError::RingFull`] / [`GpuError::RingDead`] as the ring's.
    pub fn frame(&self, width: u32, height: u32, wait: Duration) -> Result<GpuFrame, GpuError> {
        let side = MAX_PICTURE_SIDE;
        if copy_box((width, height), (side, side)).is_none() {
            return Err(GpuError::RingPicture {
                width,
                height,
                surface_width: side,
                surface_height: side,
            });
        }
        let class = SizeClass::of(RingFormat::Nv12, width, height);
        let taken = self.core.take(class, wait)?;
        Ok(hand_over(
            &self.core,
            Written {
                slot: taken.slot,
                alloc_gen: taken.alloc_gen,
                class,
                size: (width, height),
                handle: FAKE_HANDLE_BASE + 4 * (taken.slot as isize + 1),
                keep: None,
            },
        ))
    }

    /// The ring's counters.
    pub fn stats(&self) -> RingStats {
        self.core.stats()
    }

    /// Kill the ring (a test of the writer's or the compositor's answer to
    /// a lost device); whether this call killed it.
    pub fn kill(&self) -> bool {
        self.core.kill("a test killed the fake ring", None)
    }
}

#[cfg(test)]
#[path = "ring_tests.rs"]
mod tests;
