//! Recycling frame-buffer pool (#203 round 2b).
//!
//! A process-global, size-class-keyed free-list that lets the playing steady
//! state reuse large NV12 pixel buffers instead of allocating a fresh `Vec` per
//! decoded frame and freeing it once the NDI SDK releases it. Each birth/burial
//! of a 3–8 MB buffer costs a `VirtualAlloc` (demand-zero page faults) plus a
//! `VirtualFree` (a TLB shootdown to every core) — the memory-manager
//! contention that stalls the LED wall under a resident heavy child (#203, box
//! test 8). Recycling removes the churn at the ONE point every frame already
//! passes: the drop of its last owner.
//!
//! This module lives in `sp-decoder` (which has NO `sp-core` dependency) so BOTH
//! the Windows Media Foundation reader (the only taker on the playing path) and
//! `sp-server`'s `SharedFrame` (the single sharing handle across the pacer,
//! handoff, submit thread and SDK holdover) can use it. It is fully
//! cross-platform and Linux-tested — the Win32 taker calls it but the pool logic
//! itself carries no platform code.
//!
//! Ownership / safety: a recycled buffer may be reused ONLY after every owner is
//! gone. `SharedFrame` wraps a [`PooledBuf`] in an `Arc`; the SDK holdover keeps
//! the previous frame's `Arc` alive until the next async submit returns, so the
//! buffer the SDK still points at is not recycled until that `Arc` drops — which
//! is exactly when [`PooledBuf`]'s `Drop` runs. Recycling is therefore
//! release-point-agnostic: holdover replacement, pacer-repeat replacement, a
//! handoff coalesce and a seek flush all recycle through the same `Drop`.
//!
//! #223 S9b (D8): a size class no frame took for [`IDLE_CLASS_EVICT`] is
//! freed whole at the next recycle — the resolution of a song that no longer
//! plays (a 4K class keeps 6 × 12.4 MB). A class a source decodes into is
//! taken every frame and stays; a source paused over a minute loses its
//! class's spare buffers, and its first frames after the pause allocate
//! afresh (a speed cost, never a wrong picture). Freed buffers are dropped
//! after the lock is released, like a fresh allocation is made without it.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Max recycled buffers kept per exact-capacity size class. Beyond this,
/// [`recycle`] drops the buffer so the pool cannot grow without bound (6 covers
/// the deepest look-ahead + holdover + handoff a single resolution keeps live).
pub const POOL_CAP_PER_CLASS: usize = 6;

/// A size class no frame took for this long is freed whole (#223 S9b).
pub const IDLE_CLASS_EVICT: Duration = Duration::from_secs(60);

/// One size class: its free buffers and when a frame of its size was last
/// taken.
struct Class {
    bufs: Vec<Vec<u8>>,
    last_taken: Instant,
}

impl Class {
    fn new(now: Instant) -> Self {
        Self {
            bufs: Vec::new(),
            last_taken: now,
        }
    }
}

/// The free-list: classes keyed by their buffers' EXACT `Vec` capacity, so
/// one frame resolution is one size class and a `take` never hands back a
/// wrong-sized buffer.
type FreeList = BTreeMap<usize, Class>;

/// The process-global pool. `BTreeMap::new` and `Mutex::new` are both `const`,
/// so no lazy initialisation is needed.
static POOL: Mutex<FreeList> = Mutex::new(BTreeMap::new());

/// Lock the pool, recovering from a poisoned mutex rather than panicking —
/// [`recycle`] runs inside `Drop`, which must never panic.
fn pool() -> MutexGuard<'static, FreeList> {
    POOL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take a buffer with capacity for at least `len` bytes, CLEARED (length 0,
/// capacity retained) so the caller `extend_from_slice`s into recycled capacity
/// with no page fault. Returns a recycled buffer from the exact `len` size class
/// if one is free (removing it from the pool), else a fresh
/// `Vec::with_capacity(len)`.
///
/// The size-class key round-trips because [`take`] keys by `len` while
/// [`recycle`] keys by `Vec::capacity`, and on the Global allocator
/// `Vec::with_capacity(len).capacity() == len` while filling EXACTLY `len` bytes
/// (`extend_from_slice`) never grows the capacity — so a buffer this fn hands out
/// comes back under the same key. If an exotic allocator ever over-allocated,
/// the only effect is a recycle that misses its class (a perf no-op, never a
/// correctness bug — the wall pixels are always the caller's own bytes).
pub fn take(len: usize) -> Vec<u8> {
    // Pop under the lock, then drop the guard at the `let` semicolon so a fresh
    // allocation never holds it. `recycled` owns the popped buffer (or `None`).
    let recycled = take_from(&mut pool(), len, Instant::now());
    match recycled {
        Some(mut buf) => {
            buf.clear();
            buf
        }
        None => Vec::with_capacity(len),
    }
}

/// Raised by [`try_take`] when a pool miss cannot allocate a `len`-byte buffer.
/// The fallible alternative to [`take`]'s aborting `Vec::with_capacity` on the
/// decoder hot path, so a host allocation failure drops one frame instead of
/// aborting the whole process (`handle_alloc_error` — the #156 `0xc0000409`
/// class). Carries the requested byte count for the pipeline's WARN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameAllocFailed {
    /// The byte count the failed allocation asked for.
    pub bytes: usize,
}

impl std::fmt::Display for FrameAllocFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "frame buffer allocation of {} bytes failed", self.bytes)
    }
}

impl std::error::Error for FrameAllocFailed {}

/// Allocate exactly `len` bytes FALLIBLY (never aborts): a fresh `Vec` grown via
/// `try_reserve_exact`, so a host OOM / capacity overflow returns `Err` rather
/// than calling `handle_alloc_error`. Split out so the miss path is unit-testable
/// with a `len` (`usize::MAX / 2`) that `try_reserve_exact` rejects on every
/// platform. Returns an empty buffer with capacity ≥ `len` (length 0), matching
/// [`take`]'s "cleared, capacity retained" contract.
fn alloc_exact(len: usize) -> Result<Vec<u8>, FrameAllocFailed> {
    let mut v: Vec<u8> = Vec::new();
    match v.try_reserve_exact(len) {
        Ok(()) => Ok(v),
        Err(_) => Err(FrameAllocFailed { bytes: len }),
    }
}

/// Like [`take`], but FALLIBLE on a pool miss: a pool hit returns the recycled
/// buffer (never allocates); a miss allocates via [`alloc_exact`] and returns
/// `Err(FrameAllocFailed)` when the host is out of commit, instead of aborting.
/// The Media Foundation reader uses this on the per-frame path so a failed frame
/// buffer drops one frame instead of killing the process (#207 / #156).
pub fn try_take(len: usize) -> Result<Vec<u8>, FrameAllocFailed> {
    // Pop under the lock, then drop the guard at the `let` semicolon so a fresh
    // allocation never holds it.
    let recycled = take_from(&mut pool(), len, Instant::now());
    match recycled {
        Some(mut buf) => {
            buf.clear();
            Ok(buf)
        }
        None => alloc_exact(len),
    }
}

/// Pop a recycled buffer of exactly `len` bytes, marking the class taken at
/// `now` — a miss too: the buffer allocated for it comes back to this class.
fn take_from(pool: &mut FreeList, len: usize, now: Instant) -> Option<Vec<u8>> {
    let class = pool.entry(len).or_insert_with(|| Class::new(now));
    class.last_taken = now;
    class.bufs.pop()
}

/// Keep `buf` in its class (at most [`POOL_CAP_PER_CLASS`]) and free every
/// class no frame took for [`IDLE_CLASS_EVICT`] by `now`. Returns what to
/// drop — a buffer over the cap and the freed classes' buffers — for the
/// caller to drop after the lock.
fn recycle_into(pool: &mut FreeList, buf: Vec<u8>, now: Instant) -> Vec<Vec<u8>> {
    let mut dropped = Vec::new();
    let class = pool
        .entry(buf.capacity())
        .or_insert_with(|| Class::new(now));
    if class.bufs.len() < POOL_CAP_PER_CLASS {
        class.bufs.push(buf);
    } else {
        dropped.push(buf);
    }
    let idle: Vec<usize> = pool
        .iter()
        .filter(|(_, class)| now.saturating_duration_since(class.last_taken) >= IDLE_CLASS_EVICT)
        .map(|(&cap, _)| cap)
        .collect();
    for cap in idle {
        if let Some(class) = pool.remove(&cap) {
            dropped.extend(class.bufs);
        }
    }
    dropped
}

/// Return a buffer to its exact-capacity size class for reuse. Keeps at most
/// [`POOL_CAP_PER_CLASS`] buffers per class; beyond the cap (or for an empty,
/// unallocated buffer) the buffer is dropped, and every class no frame took
/// for [`IDLE_CLASS_EVICT`] is freed. Called from [`PooledBuf`]'s `Drop`.
pub fn recycle(buf: Vec<u8>) {
    if buf.capacity() == 0 {
        return; // no allocation to recycle
    }
    // The guard drops at the `let` semicolon: the freed buffers are dropped
    // (their memory returned) with the lock released.
    let dropped = recycle_into(&mut pool(), buf, Instant::now());
    drop(dropped);
}

/// An owned pixel buffer whose `Drop` returns its allocation to the pool for
/// reuse instead of freeing it (#203). Derefs to `[u8]` like the `Vec` it wraps,
/// read-only: a frame is never painted after it is decoded (#221 lane 3 deleted
/// the burn overlay, the one writer).
pub struct PooledBuf(Vec<u8>);

impl PooledBuf {
    /// A copy of `src` in a buffer taken from the pool ([`take`]), so a
    /// steady-state copy reuses recycled capacity instead of a fresh
    /// `VirtualAlloc` + demand-zero faults (#147 round 10). The copy is its own
    /// allocation; its `Drop` recycles it like any other `PooledBuf`.
    pub fn copy_from_slice(src: &[u8]) -> Self {
        let mut buf = take(src.len());
        buf.extend_from_slice(src);
        Self(buf)
    }

    /// Extract the inner buffer, taking ownership WITHOUT recycling it.
    pub fn into_inner(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl From<Vec<u8>> for PooledBuf {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        recycle(std::mem::take(&mut self.0));
    }
}

/// Test/diagnostic peek: number of buffers currently pooled in the `cap` size
/// class. NOT `#[cfg(test)]` because `sp-server`'s `frame_buf` tests (a separate
/// crate, hence a separate test binary) must read it too, and a `#[cfg(test)]`
/// item is invisible across crates.
#[doc(hidden)]
pub fn pool_len(cap: usize) -> usize {
    pool().get(&cap).map_or(0, |class| class.bufs.len())
}

/// Test/diagnostic reset: empty the whole pool so a test starts hermetic.
#[doc(hidden)]
pub fn clear_pool() {
    pool().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // The pool is a process-global static, so global-state tests serialise on
    // one lock and clear the pool first (the same pattern the repo's other
    // global-state tests use).
    static SERIAL: Mutex<()> = Mutex::new(());

    fn guard() -> MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        g
    }

    #[test]
    fn take_then_recycle_round_trips_the_same_allocation() {
        let _s = guard();
        let mut b = Vec::with_capacity(128);
        b.extend_from_slice(&[7u8; 128]);
        let cap = b.capacity();
        let p = b.as_ptr();
        recycle(b);
        assert_eq!(pool_len(cap), 1, "recycle pooled it");
        let t = take(cap);
        assert_eq!(t.as_ptr(), p, "take returns the SAME allocation");
        assert!(t.is_empty(), "take clears the recycled buffer");
        assert!(t.capacity() >= cap, "capacity retained for reuse");
        assert_eq!(pool_len(cap), 0, "take removed it from the pool");
    }

    #[test]
    fn take_on_empty_class_allocates_fresh_with_capacity() {
        let _s = guard();
        let t = take(4096);
        assert!(t.is_empty());
        assert!(t.capacity() >= 4096);
        assert_eq!(pool_len(4096), 0, "a fresh take never touches the pool");
    }

    #[test]
    fn recycle_caps_the_class_at_six_and_drops_the_rest() {
        let _s = guard();
        let cap = Vec::<u8>::with_capacity(64).capacity();
        for _ in 0..POOL_CAP_PER_CLASS {
            recycle(Vec::with_capacity(64));
        }
        assert_eq!(pool_len(cap), POOL_CAP_PER_CLASS, "kept exactly the cap");
        recycle(Vec::with_capacity(64));
        assert_eq!(
            pool_len(cap),
            POOL_CAP_PER_CLASS,
            "the 7th recycle of a class is dropped"
        );
    }

    #[test]
    fn size_classes_never_mix() {
        let _s = guard();
        let mut big = Vec::with_capacity(200);
        big.extend_from_slice(&[1u8; 200]);
        let big_cap = big.capacity();
        let big_ptr = big.as_ptr();
        recycle(big);
        // A SMALLER take must never return the bigger class's buffer.
        let t = take(50);
        assert_ne!(t.as_ptr(), big_ptr, "take(50) never returns the 200 class");
        assert!(t.capacity() >= 50);
        assert_eq!(pool_len(big_cap), 1, "the 200 class is untouched");
        assert_eq!(pool_len(50), 0);
    }

    #[test]
    fn pooled_buf_drop_recycles_into_its_capacity_class() {
        let _s = guard();
        let mut v = Vec::with_capacity(96);
        v.extend_from_slice(&[3u8; 96]);
        let cap = v.capacity();
        let p = v.as_ptr();
        let pooled = PooledBuf::from(v);
        assert_eq!(&pooled[..], &[3u8; 96][..], "Deref exposes the pixels");
        assert_eq!(pool_len(cap), 0, "still owned, not yet recycled");
        drop(pooled);
        assert_eq!(pool_len(cap), 1, "Drop recycled it");
        let again = take(cap);
        assert_eq!(again.as_ptr(), p, "the recycled allocation is reused");
    }

    #[test]
    fn copy_from_slice_copies_into_a_recycled_buffer_of_its_class() {
        // #147 round 10: a copy must reuse a recycled buffer of the same size
        // class, never a fresh 5.5 MB allocation. The spare stays alive in the
        // pool until the copy takes it, so a fresh allocation can never land on
        // its address by allocator coincidence.
        let _s = guard();
        let mut spare = Vec::with_capacity(160);
        spare.extend_from_slice(&[0u8; 160]);
        let cap = spare.capacity();
        let spare_ptr = spare.as_ptr();
        recycle(spare);
        let mut v = Vec::with_capacity(cap);
        v.extend((0..cap).map(|i| (i * 7) as u8));
        let src = PooledBuf::from(v);

        let copy = PooledBuf::copy_from_slice(&src);

        assert_eq!(
            copy.as_ptr(),
            spare_ptr,
            "the copy reuses the pooled buffer"
        );
        assert_eq!(&copy[..], &src[..], "with the source's exact bytes");
        assert_ne!(copy.as_ptr(), src.as_ptr(), "still a separate allocation");
        assert_eq!(pool_len(cap), 0, "the recycled buffer left the pool");
    }

    #[test]
    fn pooled_buf_into_inner_extracts_without_recycling() {
        let _s = guard();
        let mut v = Vec::with_capacity(80);
        v.extend_from_slice(&[5u8; 80]);
        let cap = v.capacity();
        let p = v.as_ptr();
        let inner = PooledBuf::from(v).into_inner();
        assert_eq!(inner.as_ptr(), p, "into_inner returns the SAME allocation");
        assert_eq!(&inner[..], &[5u8; 80][..]);
        assert_eq!(pool_len(cap), 0, "into_inner does NOT recycle");
    }

    #[test]
    fn recycle_ignores_an_unallocated_empty_buffer() {
        let _s = guard();
        recycle(Vec::new());
        assert_eq!(pool_len(0), 0, "an empty (capacity 0) buffer is not pooled");
    }

    #[test]
    fn clear_pool_empties_every_class() {
        let _s = guard();
        recycle(Vec::with_capacity(32));
        recycle(Vec::with_capacity(48));
        let c32 = Vec::<u8>::with_capacity(32).capacity();
        assert_eq!(pool_len(c32), 1);
        clear_pool();
        assert_eq!(pool_len(c32), 0, "clear emptied the pool");
    }

    // -----------------------------------------------------------------------
    // #207 — try_take: fallible pool-miss allocation.
    // -----------------------------------------------------------------------

    #[test]
    fn try_take_pool_hit_returns_recycled_without_allocating() {
        let _s = guard();
        let mut b = Vec::with_capacity(256);
        b.extend_from_slice(&[1u8; 256]);
        let cap = b.capacity();
        let p = b.as_ptr();
        recycle(b);
        let t = try_take(cap).expect("a pool hit never fails");
        assert_eq!(
            t.as_ptr(),
            p,
            "try_take returns the SAME recycled allocation"
        );
        assert!(t.is_empty(), "recycled buffer is cleared");
        assert!(t.capacity() >= cap, "capacity retained");
        assert_eq!(pool_len(cap), 0, "try_take removed it from the pool");
    }

    #[test]
    fn try_take_miss_allocates_ok_with_capacity() {
        let _s = guard();
        let t = try_take(4096).expect("a sane allocation succeeds");
        assert!(t.is_empty());
        assert!(t.capacity() >= 4096, "miss allocates capacity >= len");
        assert_eq!(pool_len(4096), 0, "a fresh miss never touches the pool");
    }

    #[test]
    fn alloc_exact_returns_ok_with_capacity_for_a_sane_len() {
        let v = alloc_exact(1024).expect("a sane allocation succeeds");
        assert!(v.is_empty());
        assert!(v.capacity() >= 1024);
    }

    #[test]
    fn alloc_exact_impossible_len_maps_to_frame_alloc_failed_not_abort() {
        // usize::MAX/2 bytes: try_reserve_exact rejects it (CapacityOverflow /
        // alloc error) on every platform, so the miss path returns Err — the
        // whole point of #207 (no `handle_alloc_error` abort on the wall).
        let len = usize::MAX / 2;
        let err = alloc_exact(len).expect_err("an impossible allocation must fail, not abort");
        assert_eq!(
            err.bytes, len,
            "the failure carries the requested byte count"
        );
        assert!(
            err.to_string().contains(&len.to_string()),
            "Display carries the byte count for the pipeline WARN"
        );
    }

    // ---- #223 S9b: idle size classes are freed (pure, injected time) ----

    fn buf(cap: usize) -> Vec<u8> {
        Vec::with_capacity(cap)
    }

    #[test]
    fn a_class_no_frame_took_for_a_minute_is_freed_whole() {
        let t0 = Instant::now();
        let mut pool = FreeList::new();
        let _ = take_from(&mut pool, 64, t0);
        assert!(recycle_into(&mut pool, buf(64), t0).is_empty());
        assert!(recycle_into(&mut pool, buf(64), t0).is_empty());
        // Another resolution plays now; the 64-byte class is never taken again.
        let later = t0 + IDLE_CLASS_EVICT;
        let _ = take_from(&mut pool, 128, later);
        let dropped = recycle_into(&mut pool, buf(128), later);
        assert_eq!(
            dropped.len(),
            2,
            "both idle buffers are handed back to drop"
        );
        assert!(dropped.iter().all(|b| b.capacity() == 64));
        assert!(!pool.contains_key(&64), "the idle class is gone");
        assert_eq!(pool.get(&128).map(|c| c.bufs.len()), Some(1));
    }

    #[test]
    fn a_class_taken_within_the_minute_keeps_its_buffers() {
        let t0 = Instant::now();
        let mut pool = FreeList::new();
        let _ = take_from(&mut pool, 64, t0);
        assert!(recycle_into(&mut pool, buf(64), t0).is_empty());
        let just_before = t0 + IDLE_CLASS_EVICT - Duration::from_millis(1);
        assert!(recycle_into(&mut pool, buf(64), just_before).is_empty());
        assert_eq!(pool.get(&64).map(|c| c.bufs.len()), Some(2));
    }

    #[test]
    fn a_take_keeps_its_class_alive() {
        let t0 = Instant::now();
        let mut pool = FreeList::new();
        assert!(take_from(&mut pool, 64, t0).is_none(), "a miss");
        assert!(recycle_into(&mut pool, buf(64), t0).is_empty());
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(take_from(&mut pool, 64, t1).map(|b| b.capacity()), Some(64));
        // 70 s after the first take, 20 s after the last: still in use.
        let t2 = t0 + Duration::from_secs(70);
        assert!(recycle_into(&mut pool, buf(64), t2).is_empty());
        assert_eq!(pool.get(&64).map(|c| c.bufs.len()), Some(1));
    }

    #[test]
    fn a_buffer_over_the_cap_is_handed_back_to_drop() {
        let t0 = Instant::now();
        let mut pool = FreeList::new();
        for _ in 0..POOL_CAP_PER_CLASS {
            assert!(recycle_into(&mut pool, buf(64), t0).is_empty());
        }
        let dropped = recycle_into(&mut pool, buf(64), t0);
        assert_eq!(dropped.len(), 1);
        assert_eq!(
            pool.get(&64).map(|c| c.bufs.len()),
            Some(POOL_CAP_PER_CLASS)
        );
    }
}
