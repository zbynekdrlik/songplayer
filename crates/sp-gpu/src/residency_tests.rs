//! Tests for the upload residency (#223 S1a).

use super::{Resident, Upload, upload_for};
use crate::picture::Nv12Picture;

fn picture(id: u64, width: u32, height: u32) -> Nv12Picture<'static> {
    Nv12Picture {
        id,
        width,
        height,
        stride: width,
        data: &[],
    }
}

fn held(id: u64, width: u32, height: u32) -> Option<Resident> {
    Some(Resident { id, width, height })
}

#[test]
fn an_empty_slot_creates_its_textures() {
    assert_eq!(upload_for(None, &picture(7, 2560, 1440)), Upload::Create);
}

#[test]
fn the_same_picture_again_is_not_uploaded() {
    assert_eq!(
        upload_for(held(7, 2560, 1440), &picture(7, 2560, 1440)),
        Upload::Skip
    );
}

#[test]
fn a_new_picture_of_the_same_size_is_written_into_the_textures() {
    assert_eq!(
        upload_for(held(7, 2560, 1440), &picture(8, 2560, 1440)),
        Upload::Write
    );
}

#[test]
fn another_size_creates_new_textures_whatever_the_id() {
    assert_eq!(
        upload_for(held(7, 2560, 1440), &picture(7, 1920, 1440)),
        Upload::Create
    );
    assert_eq!(
        upload_for(held(7, 2560, 1440), &picture(7, 2560, 1080)),
        Upload::Create
    );
    assert_eq!(
        upload_for(held(7, 2560, 1440), &picture(8, 1280, 720)),
        Upload::Create
    );
}

#[test]
fn a_slot_holds_what_was_uploaded_into_it() {
    assert_eq!(
        Resident::of(&picture(9, 1280, 720)),
        Resident {
            id: 9,
            width: 1280,
            height: 720
        }
    );
}
