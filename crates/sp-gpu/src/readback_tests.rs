//! Tests for the readback's row packing (#223 S1a, review round 4).

use super::{mapped_len, unpad_rows};

const P: u8 = 0xEE;

/// Three rows of 3 pixel bytes, 5 bytes apart; the last row ends at its
/// pixels (a mapping holds no padding after its last row's pixels here).
const MAPPED: [u8; 13] = [0, 1, 2, P, P, 10, 11, 12, P, P, 20, 21, 22];

#[test]
fn a_mapping_ends_with_its_last_row_s_pixels() {
    // 16 · 2 + 12, and an unpadded pitch.
    assert_eq!(mapped_len(16, 12, 3), Some(44));
    assert_eq!(mapped_len(12, 12, 3), Some(36));
    assert_eq!(mapped_len(16, 12, 1), Some(12));
    // The render target's rows: 15 360 bytes, 2160 of them.
    assert_eq!(mapped_len(15_360, 15_360, 2160), Some(33_177_600));
}

#[test]
fn a_pitch_shorter_than_a_row_or_an_overflow_has_no_length() {
    assert_eq!(mapped_len(11, 12, 3), None);
    assert_eq!(mapped_len(usize::MAX, 1, 3), None);
}

#[test]
fn padded_rows_are_packed() {
    assert_eq!(
        unpad_rows(&MAPPED, 5, 3, 3),
        Some(vec![0, 1, 2, 10, 11, 12, 20, 21, 22])
    );
    // Rows with no padding come out as they are.
    assert_eq!(
        unpad_rows(&[1, 2, 3, 4, 5, 6], 3, 3, 2),
        Some(vec![1, 2, 3, 4, 5, 6])
    );
}

#[test]
fn a_mapping_too_short_for_its_rows_is_refused() {
    assert_eq!(unpad_rows(&MAPPED[..12], 5, 3, 3), None);
    assert_eq!(unpad_rows(&MAPPED, 2, 3, 3), None);
}
