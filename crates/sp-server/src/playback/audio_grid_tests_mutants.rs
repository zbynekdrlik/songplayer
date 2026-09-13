//! Mutation-killing unit tests for `AudioGridBuffer` (#148) — `enforce_cap`,
//! `buffer_ms`, `target_level`. Each test pins an EXACT observed value at a
//! precise input so a single operator / whole-body mutation flips it.
//! `super::*` resolves to the `audio_grid` module under test.
//!
//! (The pinned-skip mutants `push -> ()`, the `==` in `push`, and
//! `level_samples -> 0/1` are deliberately NOT targeted here — they time out the
//! suite and are pinned by the supervisor.)

use super::*;

/// A single-channel planar chunk of `n` copies of `value`.
fn one_channel(value: f32, n: usize) -> Vec<Vec<f32>> {
    vec![vec![value; n]]
}

/// `enforce_cap` drops only when `level > cap` (strict). Pushing EXACTLY the cap
/// (rate 1000 → cap = 2000) must not drop and must not count an overflow.
///
/// Kills 145:18 `> -> >=`: `2000 >= 2000` would enter the drop branch and bump
/// `overflows` (draining a zero-sized excess), so `overflows()` becomes 1.
#[test]
fn enforce_cap_level_equal_to_cap_is_not_an_overflow() {
    let mut buf = AudioGridBuffer::new(1000, 3200);
    buf.push(&one_channel(0.25, 2000));
    assert_eq!(buf.cap_samples(), 2000, "cap = rate * 2");
    assert_eq!(
        buf.level_samples(),
        2000,
        "exactly the cap, nothing dropped"
    );
    assert_eq!(
        buf.overflows(),
        0,
        "level == cap must not count as overflow"
    );
}

/// When overflowing, `excess = level - cap` so the drop lands the level back on
/// the cap. Pushing 3000 into a 2000 cap must leave exactly 2000 buffered.
///
/// Kills 146:32 `- -> +`: `excess = 3000 + 2000 = 5000` drains the whole FIFO,
/// leaving the level at 0 instead of 2000.
#[test]
fn enforce_cap_drops_only_the_excess() {
    let mut buf = AudioGridBuffer::new(1000, 3200);
    buf.push(&one_channel(0.25, 3000));
    assert_eq!(
        buf.overflows(),
        1,
        "flooding past the cap counts one overflow"
    );
    assert_eq!(
        buf.level_samples(),
        2000,
        "only the 1000-sample excess is dropped; the level rests on the cap"
    );
}

/// `buffer_ms = level * 1000 / rate`. 48000 samples at 48 kHz is exactly
/// 1000 ms.
///
/// Kills 166:39 `* -> +` (`(48000 + 1000) / 48000 = 1`) and 166:46 `/ -> *`
/// (`48000 * 1000 * 48000` = an astronomically large number) — both diverge
/// from 1000.
#[test]
fn buffer_ms_is_level_times_1000_over_rate() {
    let mut buf = AudioGridBuffer::new(48_000, 3200);
    buf.push(&one_channel(0.1, 48_000));
    assert_eq!(buf.level_samples(), 48_000, "48000 samples buffered");
    assert_eq!(buf.buffer_ms(), 1000, "48000 * 1000 / 48000 == 1000 ms");
}

/// `target_level()` returns the configured value verbatim. Two distinct targets,
/// neither 0 nor 1.
///
/// Kills 175:9 `-> usize with 0` (returns 0 ≠ 3200) and `-> usize with 1`
/// (returns 1 ≠ 3200); the second construction (target 7) is an independent
/// witness that both replacements diverge.
#[test]
fn target_level_returns_the_configured_value() {
    assert_eq!(AudioGridBuffer::new(48_000, 3200).target_level(), 3200);
    assert_eq!(AudioGridBuffer::new(1000, 7).target_level(), 7);
}
