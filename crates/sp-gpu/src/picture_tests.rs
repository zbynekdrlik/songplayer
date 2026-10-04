//! Tests for the NV12 picture check (#223 S1a).

use super::{MAX_PICTURE_SIDE, Nv12Picture, PictureError, Plane, Planes};

fn picture(width: u32, height: u32, stride: u32, data: &[u8]) -> Nv12Picture<'_> {
    Nv12Picture {
        id: 1,
        width,
        height,
        stride,
        data,
    }
}

#[test]
fn a_whole_picture_gives_its_two_planes() {
    let data = [0u8; 18];
    assert_eq!(
        picture(4, 2, 6, &data).planes(),
        Ok(Planes {
            luma: Plane {
                width: 4,
                height: 2,
                offset: 0
            },
            chroma: Plane {
                width: 2,
                height: 1,
                offset: 12
            },
            pitch: 6,
        })
    );
}

#[test]
fn an_odd_picture_rounds_its_chroma_plane_up() {
    // 5×3: chroma 3×2 pairs (a row of 6 bytes), 6·3 + 6·2 = 30 bytes.
    let data = [0u8; 30];
    let planes = picture(5, 3, 6, &data)
        .planes()
        .expect("a whole 5x3 picture");
    assert_eq!(
        planes.chroma,
        Plane {
            width: 3,
            height: 2,
            offset: 18
        }
    );
    assert_eq!(
        picture(5, 3, 6, &data[..29]).planes(),
        Err(PictureError::Short { len: 29, need: 30 })
    );
}

#[test]
fn the_stride_must_hold_a_chroma_row() {
    let data = [0u8; 30];
    // 5 columns need a 6-byte chroma row (3 U/V pairs).
    assert_eq!(
        picture(5, 3, 5, &data).planes(),
        Err(PictureError::Stride { stride: 5, row: 6 })
    );
    assert!(picture(5, 3, 6, &data).planes().is_ok());
}

#[test]
fn a_picture_with_no_pixels_is_refused() {
    let data = [0u8; 64];
    assert_eq!(
        picture(0, 2, 4, &data).planes(),
        Err(PictureError::Empty {
            width: 0,
            height: 2
        })
    );
    assert_eq!(
        picture(2, 0, 4, &data).planes(),
        Err(PictureError::Empty {
            width: 2,
            height: 0
        })
    );
}

#[test]
fn a_side_over_the_texture_limit_is_refused() {
    let side = MAX_PICTURE_SIDE;
    let data = vec![0u8; (side as usize + 2) * 3];
    assert!(picture(side, 2, side, &data).planes().is_ok());
    assert_eq!(
        picture(side + 1, 2, side + 2, &data).planes(),
        Err(PictureError::TooLarge {
            width: side + 1,
            height: 2
        })
    );
    assert_eq!(
        picture(2, side + 1, 2, &data).planes(),
        Err(PictureError::TooLarge {
            width: 2,
            height: side + 1
        })
    );
}

#[test]
fn the_bytes_must_hold_both_planes() {
    // 4×2, stride 6: 6·2 + 6·1 = 18 bytes.
    let data = [0u8; 18];
    assert!(picture(4, 2, 6, &data).planes().is_ok());
    assert_eq!(
        picture(4, 2, 6, &data[..17]).planes(),
        Err(PictureError::Short { len: 17, need: 18 })
    );
}
