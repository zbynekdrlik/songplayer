//! Tests for a composition's layers (#223 S1a; #239: in a given target).

use sp_core::fit::Placement;

use super::{CANVAS_HEIGHT, CANVAS_WIDTH, Composition, FHD_HEIGHT, FHD_WIDTH, Layer, Slot};
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

/// #239: `SP-program`'s 1920×1080 target, filled by a 16:9 picture.
const FHD_FULL: Placement = Placement {
    w: 1920,
    h: 1080,
    off_x: 0,
    off_y: 0,
};

/// A 4:3 picture in the 1920×1080 target: columns 240..1680.
const FHD_PILLARBOX: Placement = Placement {
    w: 1440,
    h: 1080,
    off_x: 240,
    off_y: 0,
};

#[test]
fn the_fhd_target_is_fixed_at_1920_by_1080() {
    // #239: the SP-program Spout sender carries the NDI SP-program's canvas.
    assert_eq!((FHD_WIDTH, FHD_HEIGHT), (1920, 1080));
}

#[test]
fn layers_in_place_each_picture_in_the_given_target() {
    let plain = Composition::Picture(picture(7, 2560, 1440));
    assert_eq!(
        summary(&plain.layers_in(FHD_WIDTH, FHD_HEIGHT)),
        [(Slot::Outgoing, 7, FHD_FULL, 1.0)]
    );
    // 21:9 in 1920×1080: 810 rows, centred on even rows 134..944.
    let wide = Composition::Picture(picture(9, 2560, 1080));
    let letterbox = Placement {
        w: 1920,
        h: 810,
        off_x: 0,
        off_y: 134,
    };
    assert_eq!(
        summary(&wide.layers_in(FHD_WIDTH, FHD_HEIGHT)),
        [(Slot::Outgoing, 9, letterbox, 1.0)]
    );
    // A target taller than wide: the width binds, the rows are centred
    // (253 rows floored to even, 274 = (800 − 252) / 2).
    let tall = Placement {
        w: 600,
        h: 252,
        off_x: 0,
        off_y: 274,
    };
    assert_eq!(
        summary(&wide.layers_in(600, 800)),
        [(Slot::Outgoing, 9, tall, 1.0)]
    );
}

#[test]
fn a_fade_in_the_fhd_target_has_maxs_weights_and_fhds_places() {
    let fade = Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8: 64,
    };
    assert_eq!(
        summary(&fade.layers_in(FHD_WIDTH, FHD_HEIGHT)),
        [
            (Slot::Outgoing, 7, FHD_FULL, 0.75),
            (Slot::Incoming, 8, FHD_PILLARBOX, 0.25),
        ]
    );
    assert!(
        Composition::Black
            .layers_in(FHD_WIDTH, FHD_HEIGHT)
            .is_empty(),
        "the black has no layer in any target"
    );
}

#[test]
fn layers_is_layers_in_the_max_canvas() {
    let fade = Composition::Fade {
        from: Some(picture(7, 2560, 1080)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8: 100,
    };
    assert_eq!(
        summary(&fade.layers()),
        summary(&fade.layers_in(CANVAS_WIDTH, CANVAS_HEIGHT))
    );
    assert_eq!(
        summary(&fade.layers()),
        [
            (
                Slot::Outgoing,
                7,
                Placement {
                    w: 3840,
                    h: 1620,
                    off_x: 0,
                    off_y: 270,
                },
                156.0 / 256.0
            ),
            (Slot::Incoming, 8, PILLARBOX, 100.0 / 256.0),
        ]
    );
}

#[test]
fn each_slot_has_its_own_texture_index() {
    assert_eq!(Slot::Outgoing.index(), 0);
    assert_eq!(Slot::Incoming.index(), 1);
}
