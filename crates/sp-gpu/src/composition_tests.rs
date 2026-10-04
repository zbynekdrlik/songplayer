//! Tests for a composition's layers (#223 S1a).

use sp_core::fit::Placement;

use super::{CANVAS_HEIGHT, CANVAS_WIDTH, Composition, Layer, Slot};
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

/// (slot, picture id, place, weight) of each layer, in draw order.
fn summary(layers: &[Layer<'_>]) -> Vec<(Slot, u64, Placement, f32)> {
    layers
        .iter()
        .map(|l| (l.slot, l.picture.id, l.place, l.weight))
        .collect()
}

const FULL: Placement = Placement {
    w: 3840,
    h: 2160,
    off_x: 0,
    off_y: 0,
};

const PILLARBOX: Placement = Placement {
    w: 2880,
    h: 2160,
    off_x: 480,
    off_y: 0,
};

#[test]
fn the_canvas_is_fixed_at_3840_by_2160() {
    // The owner's rule (#223 revision 3): SP-program-MAX never changes size.
    assert_eq!((CANVAS_WIDTH, CANVAS_HEIGHT), (3840, 2160));
}

#[test]
fn the_black_has_no_layer() {
    assert!(Composition::Black.layers().is_empty());
}

#[test]
fn a_plain_picture_is_one_outgoing_layer_at_full_weight() {
    let layers = Composition::Picture(picture(7, 2560, 1440)).layers();
    assert_eq!(summary(&layers), [(Slot::Outgoing, 7, FULL, 1.0)]);
}

#[test]
fn a_fade_draws_the_outgoing_side_first_at_one_minus_the_weight() {
    let layers = Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8: 64,
    }
    .layers();
    assert_eq!(
        summary(&layers),
        [
            (Slot::Outgoing, 7, FULL, 0.75),
            (Slot::Incoming, 8, PILLARBOX, 0.25),
        ]
    );
}

#[test]
fn a_side_of_weight_zero_is_not_drawn() {
    let fade = |weight_q8| Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8,
    };
    assert_eq!(summary(&fade(0).layers()), [(Slot::Outgoing, 7, FULL, 1.0)]);
    assert_eq!(
        summary(&fade(256).layers()),
        [(Slot::Incoming, 8, PILLARBOX, 1.0)]
    );
    // A weight past 256 is all of the incoming side.
    assert_eq!(
        summary(&fade(300).layers()),
        [(Slot::Incoming, 8, PILLARBOX, 1.0)]
    );
}

#[test]
fn a_missing_side_is_the_black() {
    let up_from_black = Composition::Fade {
        from: None,
        to: Some(picture(8, 1440, 1080)),
        weight_q8: 128,
    };
    assert_eq!(
        summary(&up_from_black.layers()),
        [(Slot::Incoming, 8, PILLARBOX, 0.5)]
    );
    let down_to_black = Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: None,
        weight_q8: 192,
    };
    assert_eq!(
        summary(&down_to_black.layers()),
        [(Slot::Outgoing, 7, FULL, 0.25)]
    );
}

#[test]
fn each_slot_has_its_own_texture_index() {
    assert_eq!(Slot::Outgoing.index(), 0);
    assert_eq!(Slot::Incoming.index(), 1);
}
