//! Tests for the shared NV12 layout arithmetic (#223 S1a).

use super::{nv12_chroma_row, nv12_len};

#[test]
fn a_chroma_row_holds_a_pair_per_two_pixels_rounded_up() {
    assert_eq!(nv12_chroma_row(4), 4);
    assert_eq!(nv12_chroma_row(5), 6);
    assert_eq!(nv12_chroma_row(1), 2);
    assert_eq!(nv12_chroma_row(0), 0);
    assert_eq!(nv12_chroma_row(3840), 3840);
}

#[test]
fn a_picture_is_its_luma_rows_and_half_as_many_chroma_rows() {
    // 6·(2 + 1), 6·(3 + 2), 7·(4 + 2).
    assert_eq!(nv12_len(6, 2), 18);
    assert_eq!(nv12_len(6, 3), 30);
    assert_eq!(nv12_len(7, 4), 42);
    // 2624 · (1440 + 720): a padded 2560×1440 decoder picture.
    assert_eq!(nv12_len(2624, 1440), 5_667_840);
    assert_eq!(nv12_len(0, 1080), 0);
}

#[test]
fn the_sizes_saturate_instead_of_wrapping() {
    // u32::MAX rows of u32::MAX bytes: past a 64-bit usize as well.
    assert_eq!(nv12_len(u32::MAX, u32::MAX), usize::MAX);
    // Exactly representable: no saturation.
    assert_eq!(nv12_chroma_row(u32::MAX), (u32::MAX as usize) + 1);
}
