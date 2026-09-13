//! Mutation-killing unit tests for [`Pacer::prep_p99_us`] (#153, PR #153).
//!
//! The p99 index is `((self.prep_len * 99) / 100).min(self.prep_len - 1)` and
//! the function returns the sorted-ring value at that index. These tests fill
//! the prep ring with a KNOWN set of distinct, already-ascending samples
//! (`ring[i] = 7 * i`) via the private `push_prep`, so `prep_p99_us()` is
//! exactly `7 * p99_index`. Ring lengths are chosen (> 100) so the correct
//! index is strictly below `prep_len - 1`, letting each arithmetic mutant map
//! to a DIFFERENT ring value:
//!
//!   * → +   collapses `len * 99` to `len + 99`  (tiny index near the front)
//!   * → /   collapses `len * 99` to `len / 99`  (index 0)
//!   / → %   turns `… / 100` into `… % 100`       (a small remainder index)
//!   / → *   turns `… / 100` into `… * 100`       (huge, clamped to `len - 1`)
//!
//! Wired as a child of `pacer_prepare`, so it reaches the private `push_prep`
//! and the `Pacer`'s private prep-ring fields (all under the `pacer` tree).

use crate::playback::pacer::Pacer;

/// A pacer whose prep ring holds `len` distinct ascending samples
/// (`ring[i] = 7 * i`). Already sorted, so `prep_p99_us()` == `7 * p99_index`.
fn pacer_with_prep_ring(len: u64) -> Pacer {
    let mut pacer = Pacer::new(30, true);
    for i in 0..len {
        pacer.push_prep(i * 7);
    }
    pacer
}

#[test]
fn prep_p99_index_is_len_times_99_div_100_clamped_len200() {
    // len = 200 → index = (200 * 99) / 100 = 198, .min(199) → 198 → v[198] = 1386.
    //   * → + : (200 + 99) / 100 = 2            → v[2]   = 14   (killed)
    //   * → / : (200 / 99) / 100 = 0            → v[0]   = 0    (killed)
    //   / → % : (200 * 99) % 100 = 0            → v[0]   = 0    (killed)
    //   / → * : (200 * 99) * 100 huge, .min(199) → v[199] = 1393 (killed)
    let pacer = pacer_with_prep_ring(200);
    assert_eq!(
        pacer.prep_p99_us(),
        1386,
        "p99 of 200 ascending samples [0,7,…,1393] is the value at index 198"
    );
}

#[test]
fn prep_p99_index_is_len_times_99_div_100_clamped_len150() {
    // len = 150 → index = (150 * 99) / 100 = 148, .min(149) → 148 → v[148] = 1036.
    //   * → + : (150 + 99) / 100 = 2            → v[2]   = 14   (killed)
    //   * → / : (150 / 99) / 100 = 0            → v[0]   = 0    (killed)
    //   / → % : (150 * 99) % 100 = 50           → v[50]  = 350  (killed)
    //   / → * : (150 * 99) * 100 huge, .min(149) → v[149] = 1043 (killed)
    let pacer = pacer_with_prep_ring(150);
    assert_eq!(
        pacer.prep_p99_us(),
        1036,
        "p99 of 150 ascending samples [0,7,…,1043] is the value at index 148"
    );
}

// ---------------------------------------------------------------------------
// prep_p99_us empty-ring guard (line 80): if prep_len == 0 { return 0 }
// ---------------------------------------------------------------------------

// #156: with an EMPTY ring the `if prep_len == 0` guard must return 0. Without
// it, `prep_len - 1` panics either way — in debug (this test profile) on
// subtraction overflow, in release by wrapping so `.min` never clamps and
// `v[huge]` indexes an empty vec (the 0xc0000409 abort). Locks the guard the
// len=150/200 tests never exercise.
#[test]
fn prep_p99_us_on_empty_ring_returns_zero_without_panicking() {
    let pacer = pacer_with_prep_ring(0);
    assert_eq!(pacer.prep_len, 0);
    assert_eq!(
        pacer.prep_p99_us(),
        0,
        "an empty prep ring must return 0, never index-panic"
    );
}
