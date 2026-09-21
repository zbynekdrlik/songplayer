//! Shared frame buffer seam (#203).
//!
//! [`SharedFrame`] is a reference-counted pixel buffer shared between the frame
//! PRODUCER and the NDI submit HOLDOVER without copying the pixels. It is the
//! foundation of the allocation-free steady state: every large NV12/BGRA frame
//! used to be a fresh `Vec<u8>` freed right after the SDK released it, so each
//! frame paid a `VirtualAlloc` (demand-zero faults) + `VirtualFree` (a TLB
//! shootdown to every core) — the memory-manager contention that stalls the LED
//! wall under a resident heavy child (#203, box test 8).
//!
//! `Arc<PooledBuf>` — NEVER `Arc<[u8]>`: converting a `Vec<u8>` into an
//! `Arc<[u8]>` COPIES every pixel. The `Arc` refcount is EXACTLY the lifetime the
//! NDI SDK's `send_send_video_async_v2` holdover needs — "the previous buffer
//! stays alive until the next async call returns": the submitter keeps the last
//! [`SharedFrame`] in `prev_frame`, so the bytes the SDK still points at are
//! freed only after the next submit installs a new one, and the idle standby
//! submits the SAME black allocation every boundary (a refcount bump, no copy).
//!
//! #203 round 2b: the inner buffer is a [`PooledBuf`], so when the LAST owner
//! drops (holdover replacement, pacer-repeat replacement, handoff coalesce, seek
//! flush) the allocation is RECYCLED into `sp_decoder::frame_pool` for the next
//! decoded frame instead of freed — removing the per-frame `VirtualAlloc`/
//! `VirtualFree` churn. The recycle happens exactly in `PooledBuf`'s `Drop`,
//! which the `Arc` fires only once every holder is gone, so a buffer the SDK may
//! still point at is never reused early.

use sp_decoder::frame_pool::PooledBuf;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// A reference-counted pixel buffer shared without copying (see the module doc).
#[derive(Clone)]
pub struct SharedFrame(Arc<PooledBuf>);

impl SharedFrame {
    /// Wrap owned pixels. One small `Arc` header allocation; the pixel buffer is
    /// MOVED in, never copied.
    pub fn new(data: Vec<u8>) -> Self {
        Self(Arc::new(PooledBuf::from(data)))
    }

    /// The pixel buffer length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the buffer holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether two handles point at the SAME underlying allocation (an `Arc`
    /// pointer identity, NOT a byte comparison). Tests use it to prove the idle
    /// standby submits the same black allocation every slot and the holdover
    /// keeps the previous frame's exact allocation.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Exclusive mutable access to the pixels, cloning ONLY if this handle is not
    /// the sole owner ([`Arc::make_mut`]). On the submit path a `SharedFrame` is
    /// freshly wrapped and uniquely owned, so this is in place (no copy); it
    /// exists so the paced burn-id overlay can paint into our own copy without
    /// disturbing any other holder.
    pub fn make_mut(&mut self) -> &mut Vec<u8> {
        // `Arc::make_mut` forks the `PooledBuf` (a fresh copy, via its `Clone`)
        // only when this is not the sole owner; `as_vec_mut` exposes the inner
        // `Vec` the overlay needs (`DerefMut` targets `[u8]`, not `Vec<u8>`).
        Arc::make_mut(&mut self.0).as_vec_mut()
    }
}

impl Deref for SharedFrame {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SharedFrame {
    /// Print only the length — never the pixels (multi-MB) — so a `PacedFrame`
    /// (which derives `Debug` and holds a `SharedFrame`) stays cheap to format.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedFrame")
            .field("len", &self.0.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptr_eq_is_allocation_identity_not_byte_equality() {
        let a = SharedFrame::new(vec![1u8, 2, 3, 4]);
        let b = a.clone(); // Arc clone — the SAME allocation.
        let c = SharedFrame::new(vec![1u8, 2, 3, 4]); // identical bytes, DIFFERENT allocation.
        assert!(a.ptr_eq(&b), "an Arc clone shares the allocation");
        assert!(
            !a.ptr_eq(&c),
            "identical bytes in a separate allocation are NOT ptr_eq"
        );
    }

    #[test]
    fn new_moves_pixels_len_and_deref_expose_them() {
        let f = SharedFrame::new(vec![9u8; 6]);
        assert_eq!(f.len(), 6);
        assert!(!f.is_empty());
        assert_eq!(&f[..], &[9u8; 6][..]); // Deref -> [u8]
        assert_eq!(f[0], 9);

        let e = SharedFrame::new(Vec::new());
        assert_eq!(e.len(), 0);
        assert!(e.is_empty());
    }

    #[test]
    fn make_mut_is_in_place_for_a_sole_owner() {
        let mut f = SharedFrame::new(vec![0u8; 3]);
        let before = f.as_ptr(); // via Deref -> [u8]::as_ptr
        f.make_mut()[1] = 5;
        assert_eq!(&f[..], &[0u8, 5, 0][..]);
        assert_eq!(
            f.as_ptr(),
            before,
            "sole owner: make_mut mutates in place, no realloc"
        );
    }

    #[test]
    fn make_mut_clones_when_shared_leaving_the_reader_intact() {
        let mut f = SharedFrame::new(vec![0u8; 3]);
        let g = f.clone(); // now shared (refcount 2)
        f.make_mut()[0] = 7;
        assert_eq!(&f[..], &[7u8, 0, 0][..], "the writer's copy changes");
        assert_eq!(&g[..], &[0u8, 0, 0][..], "the shared reader is untouched");
        assert!(
            !f.ptr_eq(&g),
            "make_mut on a shared handle forks the allocation"
        );
    }

    #[test]
    fn debug_prints_the_length_not_the_pixels() {
        let f = SharedFrame::new(vec![0u8; 42]);
        let s = format!("{f:?}");
        assert!(s.contains("SharedFrame"), "{s}");
        assert!(s.contains("42"), "the length is shown: {s}");
    }

    #[test]
    fn last_owner_drop_returns_the_allocation_to_the_pool() {
        // #203 2b: `SharedFrame` wraps `Arc<PooledBuf>`, so when the LAST owner
        // drops, the pixel buffer is RECYCLED into the pool (for the next
        // decoded frame) instead of freed. A unique, large capacity isolates
        // this from every other test's pool traffic — no clear_pool needed.
        use sp_decoder::frame_pool::{pool_len, take};
        const CAP: usize = 1_500_007;
        let f = SharedFrame::new(vec![0u8; CAP]);
        let cap = f.len(); // == CAP == the PooledBuf's capacity (keys the class)
        let clone = f.clone();
        assert_eq!(pool_len(cap), 0, "two owners alive — nothing recycled yet");
        drop(clone);
        assert_eq!(
            pool_len(cap),
            0,
            "one owner still alive — still not recycled"
        );
        let ptr = f.as_ptr();
        drop(f);
        assert_eq!(
            pool_len(cap),
            1,
            "the last-owner drop recycled the allocation"
        );
        let again = take(cap);
        assert_eq!(again.as_ptr(), ptr, "recycled the SAME allocation");
    }
}
