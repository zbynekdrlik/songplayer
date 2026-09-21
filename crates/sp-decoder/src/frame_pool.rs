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

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard};

/// Max recycled buffers kept per exact-capacity size class. Beyond this,
/// [`recycle`] drops the buffer so the pool cannot grow without bound (6 covers
/// the deepest look-ahead + holdover + handoff a single resolution keeps live).
pub const POOL_CAP_PER_CLASS: usize = 6;

/// The free-list: buffers keyed by their EXACT `Vec` capacity, so one frame
/// resolution is one size class and a `take` never hands back a wrong-sized
/// buffer.
type FreeList = BTreeMap<usize, Vec<Vec<u8>>>;

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
pub fn take(len: usize) -> Vec<u8> {
    {
        let mut pool = pool();
        if let Some(class) = pool.get_mut(&len) {
            if let Some(mut buf) = class.pop() {
                buf.clear();
                return buf;
            }
        }
    }
    Vec::with_capacity(len)
}

/// Return a buffer to its exact-capacity size class for reuse. Keeps at most
/// [`POOL_CAP_PER_CLASS`] buffers per class; beyond the cap (or for an empty,
/// unallocated buffer) the buffer is dropped. Called from [`PooledBuf`]'s `Drop`.
pub fn recycle(buf: Vec<u8>) {
    let cap = buf.capacity();
    if cap == 0 {
        return; // no allocation to recycle
    }
    let mut pool = pool();
    let class = pool.entry(cap).or_default();
    if class.len() < POOL_CAP_PER_CLASS {
        class.push(buf);
    }
}

/// An owned pixel buffer whose `Drop` returns its allocation to the pool for
/// reuse instead of freeing it (#203). Derefs to `[u8]` like the `Vec` it wraps;
/// `Clone` makes a FRESH copy (used only when the burn overlay's
/// `Arc::make_mut` forks a shared frame — burn defaults OFF, so the steady state
/// never clones).
pub struct PooledBuf(Vec<u8>);

impl PooledBuf {
    /// Exclusive access to the inner `Vec` — the burn overlay's `make_mut` path
    /// needs `&mut Vec<u8>`, which `DerefMut` (targeting `[u8]`) cannot give.
    pub fn as_vec_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
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

impl Clone for PooledBuf {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
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
    pool().get(&cap).map_or(0, Vec::len)
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
        assert_eq!(take(cap).as_ptr(), p, "the recycled allocation is reused");
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
    fn pooled_buf_clone_is_a_fresh_copy() {
        let _s = guard();
        let a = PooledBuf::from(vec![9u8; 10]);
        let b = a.clone();
        assert_eq!(&a[..], &b[..], "same bytes");
        assert_ne!(a.as_ptr(), b.as_ptr(), "clone is a separate allocation");
    }

    #[test]
    fn pooled_buf_deref_mut_and_as_vec_mut_mutate_in_place() {
        let _s = guard();
        let mut p = PooledBuf::from(vec![0u8; 4]);
        p[0] = 1; // DerefMut -> [u8]
        p.as_vec_mut().push(2);
        assert_eq!(&p[..], &[1u8, 0, 0, 0, 2][..]);
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
}
