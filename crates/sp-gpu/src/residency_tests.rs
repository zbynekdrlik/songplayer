//! Tests for the upload residency (#223 S1a).

use super::{Resident, Upload, swap_slots, upload_for};
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

/// #251: picture ids (`Some`) in slots `[Outgoing, Incoming]`.
const A: Option<u64> = Some(1);
const B: Option<u64> = Some(2);
const C: Option<u64> = Some(3);
const D: Option<u64> = Some(4);

#[test]
fn the_next_fade_of_a_blended_song_takes_its_outgoing_picture_from_the_other_slot() {
    // Fade(A -> B) left A outgoing and B incoming; Fade(B -> C) wants B
    // outgoing: trading the slots leaves only C to upload (one, not two).
    assert!(swap_slots([A, B], [B, C]));
}

#[test]
fn the_picture_a_fade_ended_on_is_taken_from_the_incoming_slot() {
    // Fade(A -> B) then B alone (outgoing slot, incoming not drawn).
    assert!(swap_slots([A, B], [B, None]));
    // The picture drawn alone sits in the incoming slot.
    assert!(swap_slots([B, A], [A, None]));
    assert!(swap_slots([None, A], [A, None]));
}

#[test]
fn no_trade_when_it_saves_no_upload() {
    assert!(!swap_slots([A, B], [A, B]), "both resident where drawn");
    assert!(!swap_slots([A, B], [A, C]), "the outgoing one is in place");
    assert!(!swap_slots([A, B], [C, D]), "two new pictures either way");
    assert!(
        !swap_slots([A, B], [None, B]),
        "the incoming one is in place"
    );
    assert!(!swap_slots([None, None], [A, B]), "nothing resident");
    assert!(
        !swap_slots([A, None], [A, B]),
        "a fade out of the picture shown"
    );
    assert!(!swap_slots([A, B], [None, None]), "nothing drawn (black)");
}

#[test]
fn both_pictures_in_the_wrong_slots_trade_places() {
    assert!(swap_slots([B, A], [A, B]));
}
