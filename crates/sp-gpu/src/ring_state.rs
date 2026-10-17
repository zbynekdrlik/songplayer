//! #279 P2: the picture ring's decisions (pure): which slot a decoded
//! picture is written into, when a writer waits, which key each side of a
//! slot's keyed mutex takes, what an answer of that mutex means, and which
//! slots the compositor keeps open.
//!
//! The GPU-resident path (epic #279, plan
//! `docs/superpowers/plans/2026-10-16-gpu-video-path.md` §5.3; the P2 design
//! comment 6237269720) decodes each playlist on its own producer device and
//! copies every picture into one slot of that pipeline's ring: an NV12
//! texture shared with the compositor's device by an NT handle and a keyed
//! mutex (the Chromium model; the P0 probe proved it on SNV's RTX and on two
//! WARP devices). The picture then travels as a [`GpuFrame`](crate::GpuFrame)
//! through the pacer and the program bus, and the compositor samples the
//! slot in place: no readback, no upload.
//!
//! - **Slots.** A ring holds at most `capacity` slots
//!   ([`RING_CAPACITY`] in production: the decode queue's 12 pictures + 8
//!   for the pacer, the handoff, MAX's queue and the compositor). A slot is
//!   one texture of one [`SizeClass`] (format and even size); it is in use
//!   while a frame refers to it and free once the last one dropped, and only
//!   a free slot is written again.
//! - **Size classes.** A ring keeps the slots of at most
//!   [`MAX_SIZE_CLASSES`] classes, the most recently written ones: a song of
//!   another size makes slots of its own, and a free slot of a class no
//!   longer kept is dropped at once (an in-use one when its last frame
//!   drops). With no empty slot left, a write takes the place of a free slot
//!   of the other class.
//! - **Back-pressure.** Every slot in use: the writer waits at most
//!   [`RING_WAIT`] for a frame to drop ([`RingState::has_room`]), then gives
//!   up: a decode error, which the reader's fallback takes. The pacer and the
//!   bus free slots by dropping frames, so a long wait is a full queue in
//!   another form.
//! - **Keys** ([`producer_key`]). A new texture's mutex holds key
//!   [`KEY_PRODUCER`] (0); every release leaves [`KEY_CONSUMER`] (1): the
//!   writer acquires the slot's last key, writes, flushes and releases 1;
//!   the compositor acquires 1 with a 0 ms timeout ([`CONSUMER_ACQUIRE_MS`]:
//!   it never blocks), draws, waits for the GPU and releases 1. A slot the
//!   compositor never read (an off-program pipeline) is acquired by the
//!   writer with key 1 again. A slot is reused only once no frame refers to
//!   it (the CPU refcount); the mutex adds the cross-device visibility and
//!   order. So a free slot's key is never the compositor's (it lets the key
//!   go before the frame it draws drops): the writer waits at most
//!   [`WRITER_ACQUIRE_MS`], and any answer but held kills the ring
//!   ([`writer_step`]).
//! - **A lost device.** On the compositor's side ([`slot_draw`]): a slot
//!   whose mutex answers `WAIT_ABANDONED` kills its ring (the design's
//!   rule): a device was lost holding its key, the producer's, or the
//!   compositor's own before its rebuild (a compose cut short by a GPU
//!   reset leaves the keys it held abandoned), and the slot's content is
//!   undefined. An acquire or an open that FAILS kills the ring while the
//!   compositor's own device is healthy, and is the compositor's own lost
//!   device otherwise (the compose fails, the compositor is rebuilt, the
//!   ring lives on). A dead ring's pictures are drawn black and counted,
//!   and its next write fails with
//!   [`GpuError::RingDead`](crate::GpuError::RingDead), so the reader goes
//!   on in software.
//! - **The compositor's open slots** ([`OpenedKeys`]). It opens a slot's
//!   texture once ([`SlotKey`]: ring, slot, allocation), keeps at most
//!   [`OPENED_MAX`] open (the least recently drawn closed first), closes an
//!   older texture of a slot the moment its new one is drawn, and closes a
//!   slot no compose drew for [`OPENED_IDLE_COMPOSES`]: the compositor keeps
//!   no texture alive that its ring let go (a dead or a dropped ring, a
//!   slot made again).

use std::time::Duration;

use crate::picture::MAX_PICTURE_SIDE;
pub use crate::probe::{CONSUMER_ACQUIRE_MS, KEY_CONSUMER, KEY_PRODUCER};
use crate::probe::{S_OK, WAIT_ABANDONED, WAIT_TIMEOUT};

/// How long a write waits for a free slot before it fails
/// ([`GpuError::RingFull`](crate::GpuError::RingFull)).
pub const RING_WAIT: Duration = Duration::from_millis(100);

/// A pipeline ring's slots: the decode queue's 12 pictures (sp-server's
/// `DECODE_QUEUE_BOUND`) + 8 for the pacer (2), the handoff (≤ 3), MAX's
/// queue (2) and the compositor (plan §5.4): ≈ 240 MB of VRAM at 4K.
pub const RING_CAPACITY: usize = 20;

/// The size classes a ring keeps slots of.
pub const MAX_SIZE_CLASSES: usize = 2;

/// How many ring slots the compositor keeps open at most: both sides of a
/// fade between two rings (2 × [`RING_CAPACITY`]).
pub const OPENED_MAX: usize = 40;

/// A slot the compositor opened and no compose drew for this many composes
/// (3 s at one compose a boundary) is closed.
pub const OPENED_IDLE_COMPOSES: u64 = 90;

/// How long a write waits for its free slot's key (ms). The key of a free
/// slot is never held (the compositor lets it go before the frame it draws
/// drops), so the wait only bounds a broken protocol: [`RING_WAIT`].
pub const WRITER_ACQUIRE_MS: u32 = 100;

/// A slot texture's format. P2 takes NV12, the decoders' 8-bit 4:2:0 output
/// (10-bit P010 is #279 P4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RingFormat {
    Nv12,
}

/// What a slot's texture is: its format and its size, both sides even (an
/// NV12 texture's sides are even, and planar copies take even boxes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SizeClass {
    pub format: RingFormat,
    pub width: u32,
    pub height: u32,
}

impl SizeClass {
    /// The class a `width`×`height` picture is written into: its sides
    /// rounded up to even.
    pub fn of(format: RingFormat, width: u32, height: u32) -> Self {
        Self {
            format,
            width: even(width),
            height: even(height),
        }
    }
}

/// `x` rounded up to even (`u32::MAX` stays: saturated).
fn even(x: u32) -> u32 {
    x.saturating_add(x % 2)
}

/// The box a write copies out of the surface a decoder handed over (its
/// texture, often taller, 16-row aligned): the picture's top-left
/// `visible` rectangle rounded up to even sides (planar copies take even
/// boxes), as (width, height). `None` when the picture is empty or over
/// [`MAX_PICTURE_SIDE`] on a side, or when that box is not inside the
/// `surface` (width, height).
pub fn copy_box(visible: (u32, u32), surface: (u32, u32)) -> Option<(u32, u32)> {
    let (width, height) = visible;
    if width == 0 || height == 0 || width > MAX_PICTURE_SIDE || height > MAX_PICTURE_SIDE {
        return None;
    }
    let (box_width, box_height) = (even(width), even(height));
    (box_width <= surface.0 && box_height <= surface.1).then_some((box_width, box_height))
}

/// `Some(count)`, the subresources a source has (`array_size` slices of
/// `mip_levels` mips each, saturated), when `subresource` is not one of
/// them: Direct3D would drop a copy out of it with no error, and the slot
/// would go out with stale content, so the write refuses it first.
pub fn missing_subresource(subresource: u32, array_size: u32, mip_levels: u32) -> Option<u32> {
    let count = array_size.saturating_mul(mip_levels);
    (subresource >= count).then_some(count)
}

/// The key a write acquires its slot's mutex with: a new texture's
/// [`KEY_PRODUCER`], else the [`KEY_CONSUMER`] its last release left (the
/// writer's own, or the compositor's).
pub fn producer_key(written: bool) -> u64 {
    if written { KEY_CONSUMER } else { KEY_PRODUCER }
}

/// A keyed mutex's `AcquireSync` answer, from its RAW HRESULT (the `windows`
/// crate's `Result` reads `WAIT_TIMEOUT` and `WAIT_ABANDONED` as `Ok`:
/// `probe::acquire_step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// `S_OK`: the caller holds the key.
    Held,
    /// `WAIT_TIMEOUT`: another side holds it (the compositor's 0 ms acquire
    /// of a slot the writer still writes: the picture is not ready).
    Busy,
    /// `WAIT_ABANDONED`: a device was lost holding it (the other side's,
    /// or the compositor's own before its rebuild); the texture's content
    /// is undefined.
    Abandoned,
    /// Any other HRESULT: the acquire failed.
    Failed(u32),
}

impl Acquired {
    /// The answer of an `AcquireSync` that returned `hresult`.
    pub fn of(hresult: u32) -> Self {
        match hresult {
            S_OK => Acquired::Held,
            WAIT_TIMEOUT => Acquired::Busy,
            WAIT_ABANDONED => Acquired::Abandoned,
            other => Acquired::Failed(other),
        }
    }

    /// The raw HRESULT the answer was read from ([`of`](Self::of) back):
    /// what a ring's death WARN names.
    pub fn hresult(self) -> u32 {
        match self {
            Acquired::Held => S_OK,
            Acquired::Busy => WAIT_TIMEOUT,
            Acquired::Abandoned => WAIT_ABANDONED,
            Acquired::Failed(hresult) => hresult,
        }
    }
}

/// An HRESULT as a ring's death WARN names it (`0x887a0005`), empty when
/// none told it.
pub fn hresult_text(hresult: Option<u32>) -> String {
    hresult.map(|h| format!("{h:#010x}")).unwrap_or_default()
}

/// What a writer does with its slot's acquire answer: write on a held key;
/// anything else kills the ring, with why (a free slot's key is never the
/// compositor's: busy is a broken protocol, abandoned a device lost
/// holding it, failed the writer's own call).
pub fn writer_step(acquired: Acquired) -> Result<(), &'static str> {
    match acquired {
        Acquired::Held => Ok(()),
        Acquired::Busy => Err("the writer found its free slot's key held (a broken protocol)"),
        Acquired::Abandoned => Err("a device was lost holding a free slot's key (WAIT_ABANDONED)"),
        Acquired::Failed(_) => Err("the writer's acquire of a free slot failed"),
    }
}

/// What the compositor found of one ring slot of a compose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOpen {
    /// The slot's ring is dead: neither opened nor acquired.
    Dead,
    /// The slot could not be opened on the compositor's device.
    Failed,
    /// The slot is open; its keyed mutex answered.
    Acquired(Acquired),
}

/// What one compose does with a ring slot ([`slot_draw`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotDraw {
    /// Held: drawn, its key released after the GPU finished.
    Draw,
    /// Busy (the writer still writes it): drawn black this boundary.
    Black,
    /// Its ring is dead: drawn black, the slot closed.
    Gone,
    /// Drawn black and its ring killed: a device was lost holding its key
    /// (the producer's, or the compositor's own before its rebuild), or the
    /// slot failed on a healthy compositor.
    KillRing,
    /// The compositor's own device is gone: the compose fails
    /// ([`GpuError::DeviceLost`](crate::GpuError::DeviceLost)), the ring
    /// lives on.
    DeviceLost,
}

/// The compositor's rule for one ring slot: held = drawn; busy = black; a
/// dead ring's = gone; abandoned = the ring dies; a failed acquire or open asks
/// `device_removed` (read only then): the compositor's own lost device, or
/// else the ring dies.
pub fn slot_draw(open: SlotOpen, device_removed: impl FnOnce() -> bool) -> SlotDraw {
    match open {
        SlotOpen::Acquired(Acquired::Held) => SlotDraw::Draw,
        SlotOpen::Acquired(Acquired::Busy) => SlotDraw::Black,
        SlotOpen::Dead => SlotDraw::Gone,
        SlotOpen::Acquired(Acquired::Abandoned) => SlotDraw::KillRing,
        SlotOpen::Acquired(Acquired::Failed(_)) | SlotOpen::Failed => {
            if device_removed() {
                SlotDraw::DeviceLost
            } else {
                SlotDraw::KillRing
            }
        }
    }
}

/// Why the compositor killed a slot's ring ([`SlotDraw::KillRing`]), for
/// its one WARN.
pub fn why_killed(open: SlotOpen) -> &'static str {
    match open {
        SlotOpen::Acquired(Acquired::Abandoned) => {
            "a device was lost holding a slot's key (WAIT_ABANDONED)"
        }
        SlotOpen::Failed => "the compositor could not open a slot",
        _ => "the compositor's acquire of a slot failed",
    }
}

/// One slot's state: the texture it holds and who uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotState {
    pub class: SizeClass,
    /// The ring's number for this texture: a slot made again (another
    /// class) is another texture, under a new number.
    pub alloc_gen: u64,
    /// A frame refers to it: it is not written again.
    pub in_use: bool,
    /// It was written once: its mutex holds [`KEY_CONSUMER`].
    pub written: bool,
}

/// What a write gets ([`RingState::take`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// The free slot `slot` holds a texture of the class (allocation
    /// `alloc_gen`): acquire it with `key` and write.
    Reuse {
        slot: usize,
        alloc_gen: u64,
        key: u64,
    },
    /// Make the texture of slot `slot` first (allocation `alloc_gen`): the
    /// slot held none, or a free one of another class, which goes. Its key
    /// is [`KEY_PRODUCER`].
    Create { slot: usize, alloc_gen: u64 },
    /// Every slot is in use: wait for a frame to drop.
    Full,
    /// The ring is dead ([`RingState::kill`]).
    Dead,
}

/// What a frame's drop did to its slot ([`RingState::release`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// The slot is free, its texture kept for the next write.
    Freed,
    /// The slot's class is no longer kept: the slot is empty, its texture
    /// goes.
    Dropped,
    /// Not a slot in use under that allocation: nothing changed.
    Stale,
}

/// One ring's slots, its kept classes and whether it is dead.
#[derive(Debug, Clone)]
pub struct RingState {
    slots: Vec<Option<SlotState>>,
    /// The kept size classes, the most recently written first.
    kept: Vec<SizeClass>,
    /// The allocation number of the next texture made.
    next_gen: u64,
    dead: bool,
}

impl RingState {
    /// A ring of `capacity` empty slots.
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: vec![None; capacity],
            kept: Vec::new(),
            next_gen: 1,
            dead: false,
        }
    }

    /// Slot `slot`'s state: `None` while it holds no texture (or past the
    /// ring).
    pub fn slot(&self, slot: usize) -> Option<SlotState> {
        self.slots.get(slot).copied().flatten()
    }

    /// How many slots hold a texture.
    pub fn made(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// How many slots a frame refers to.
    pub fn in_use(&self) -> usize {
        self.slots.iter().flatten().filter(|s| s.in_use).count()
    }

    /// Whether the ring is dead.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Kill the ring: every later [`take`](Self::take) is [`Take::Dead`].
    /// Whether it was alive (its first kill).
    pub fn kill(&mut self) -> bool {
        let alive = !self.dead;
        self.dead = true;
        alive
    }

    /// Whether a [`take`](Self::take) would end its wait now: a slot is
    /// empty or free, or the ring is dead.
    pub fn has_room(&self) -> bool {
        self.dead
            || self
                .slots
                .iter()
                .any(|slot| slot.is_none_or(|state| !state.in_use))
    }

    /// A slot for a write of `class`: a free slot of the class, else an
    /// empty slot, else a free slot of the other kept class (its texture
    /// goes); [`Take::Full`] when every slot is in use. `class` becomes the
    /// most recently written class first, and a free slot of a class no
    /// longer kept is emptied. The slot is in use from here on: a write that
    /// then fails gives it back with [`release`](Self::release).
    pub fn take(&mut self, class: SizeClass) -> Take {
        if self.dead {
            return Take::Dead;
        }
        self.keep(class);
        let free = self
            .slots
            .iter()
            .position(|s| s.is_some_and(|state| !state.in_use && state.class == class));
        if let Some(slot) = free
            && let Some(state) = self.slots[slot].as_mut()
        {
            state.in_use = true;
            return Take::Reuse {
                slot,
                alloc_gen: state.alloc_gen,
                key: producer_key(state.written),
            };
        }
        let room = self
            .slots
            .iter()
            .position(Option::is_none)
            .or_else(|| self.slots.iter().position(|s| s.is_some_and(|s| !s.in_use)));
        let Some(slot) = room else {
            return Take::Full;
        };
        let alloc_gen = self.next_gen;
        self.next_gen += 1;
        self.slots[slot] = Some(SlotState {
            class,
            alloc_gen,
            in_use: true,
            written: false,
        });
        Take::Create { slot, alloc_gen }
    }

    /// `class` is the most recently written class; a free slot of a class
    /// no longer kept is emptied.
    fn keep(&mut self, class: SizeClass) {
        self.kept.retain(|&kept| kept != class);
        self.kept.insert(0, class);
        self.kept.truncate(MAX_SIZE_CLASSES);
        let kept = &self.kept;
        for slot in &mut self.slots {
            if slot.is_some_and(|state| !state.in_use && !kept.contains(&state.class)) {
                *slot = None;
            }
        }
    }

    /// The write into slot `slot` (allocation `alloc_gen`) was released
    /// with [`KEY_CONSUMER`]: the next write acquires that key.
    pub fn written(&mut self, slot: usize, alloc_gen: u64) {
        if let Some(Some(state)) = self.slots.get_mut(slot)
            && state.alloc_gen == alloc_gen
        {
            state.written = true;
        }
    }

    /// The last frame of slot `slot` (allocation `alloc_gen`) dropped, or
    /// its write failed: the slot is free, or emptied when its class is no
    /// longer kept.
    pub fn release(&mut self, slot: usize, alloc_gen: u64) -> Release {
        let Some(state) = self.slot(slot) else {
            return Release::Stale;
        };
        if state.alloc_gen != alloc_gen || !state.in_use {
            return Release::Stale;
        }
        if self.kept.contains(&state.class) {
            self.slots[slot] = Some(SlotState {
                in_use: false,
                ..state
            });
            Release::Freed
        } else {
            self.slots[slot] = None;
            Release::Dropped
        }
    }
}

/// Whether a writer's texture of a slot (made under allocation
/// `held_gen`) is stale: the ring emptied the slot (a class no longer
/// kept) or made it again (another allocation). Its last frame dropped,
/// so nothing reads it any more.
pub fn stale_texture(state: Option<SlotState>, held_gen: u64) -> bool {
    state.is_none_or(|state| state.alloc_gen != held_gen)
}

/// Whether a write must make its slot's texture first: the take made the
/// slot (`create`), or the writer holds no texture of the slot's
/// allocation `alloc_gen` (`held_gen`: the one it holds, if any).
pub fn make_texture(create: bool, held_gen: Option<u64>, alloc_gen: u64) -> bool {
    create || held_gen != Some(alloc_gen)
}

/// A ring slot's texture as the compositor opens it: the ring (a
/// process-wide number), the slot and its allocation (a slot made again is
/// another texture).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotKey {
    pub ring: u64,
    pub slot: usize,
    pub alloc_gen: u64,
}

/// What drawing a slot asks of the compositor's open slots
/// ([`OpenedKeys::touch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    /// The slot is not open yet: open it.
    pub open: bool,
    /// The slots to close: an older texture of the same slot, and past
    /// [`OPENED_MAX`] the least recently drawn.
    pub close: Vec<SlotKey>,
}

/// The slots the compositor keeps open, each with the compose that last
/// drew it, the least recently drawn first.
#[derive(Debug, Clone, Default)]
pub struct OpenedKeys {
    keys: Vec<(SlotKey, u64)>,
    /// The composes begun.
    composes: u64,
}

impl OpenedKeys {
    /// A compose begins: the open slots no compose drew in the last
    /// [`OPENED_IDLE_COMPOSES`] are to close.
    pub fn begin_compose(&mut self) -> Vec<SlotKey> {
        self.composes += 1;
        let now = self.composes;
        let idle = self
            .keys
            .iter()
            .take_while(|&&(_, drawn)| now - drawn > OPENED_IDLE_COMPOSES)
            .count();
        self.keys.drain(..idle).map(|(key, _)| key).collect()
    }

    /// `key` is drawn by this compose: whether it must be opened, and which
    /// open slots to close.
    pub fn touch(&mut self, key: SlotKey) -> Touch {
        let open = match self.keys.iter().position(|&(held, _)| held == key) {
            Some(at) => {
                self.keys.remove(at);
                false
            }
            None => true,
        };
        let mut close = Vec::new();
        self.keys.retain(|&(held, _)| {
            let older = held.ring == key.ring && held.slot == key.slot;
            if older {
                close.push(held);
            }
            !older
        });
        self.keys.push((key, self.composes));
        let over = self.keys.len().saturating_sub(OPENED_MAX);
        close.extend(self.keys.drain(..over).map(|(key, _)| key));
        Touch { open, close }
    }

    /// `key` is not open (its open failed, or it was closed).
    pub fn forget(&mut self, key: SlotKey) {
        self.keys.retain(|&(held, _)| held != key);
    }

    /// How many slots are open.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no slot is open.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[cfg(test)]
#[path = "ring_state_tests.rs"]
mod tests;
