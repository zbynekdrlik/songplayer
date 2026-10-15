//! Tests for a composition's layers (#223 S1a; #239: in a given target;
//! #267: placed by the target's rule, MAX filling its width).

use sp_core::fit::{Fill, Placement, aspect_fit, fill_width};

use super::{
    CANVAS_HEIGHT, CANVAS_WIDTH, Composition, FHD_FIT, FHD_HEIGHT, FHD_TARGET, FHD_WIDTH, Fit,
    Layer, MAX_FIT, MAX_TARGET, Slot, Target,
};
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

/// (slot, picture id, place, uv_rect, weight) of each layer, in draw order.
fn summary(layers: &[Layer<'_>]) -> Vec<(Slot, u64, Placement, [f32; 4], f32)> {
    layers
        .iter()
        .map(|l| (l.slot, l.picture.id, l.place, l.uv_rect, l.weight))
        .collect()
}

const FULL: Placement = Placement {
    w: 3840,
    h: 2160,
    off_x: 0,
    off_y: 0,
};

/// A 4:3 picture aspect-fitted into 3840×2160: columns 480..3360.
const PILLARBOX: Placement = Placement {
    w: 2880,
    h: 2160,
    off_x: 480,
    off_y: 0,
};

/// The whole picture.
const WHOLE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// #267: a 4:3 picture filling a 16:9 target's width: scaled to 4/3 of the
/// target's height, the middle three quarters of its rows shown.
const CROP_4_3: [f32; 4] = [0.0, 0.125, 1.0, 0.875];

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
    assert_eq!(summary(&layers), [(Slot::Outgoing, 7, FULL, WHOLE, 1.0)]);
}

/// #267: MAX is the LED wall's feed: a 4:3 picture fills its whole width,
/// cropped top and bottom, never pillarboxed (Arena's Fill would carry the
/// bars onto the wall).
#[test]
fn a_4_by_3_picture_fills_max_s_whole_width() {
    let layers = Composition::Picture(picture(7, 1440, 1080)).layers();
    assert_eq!(summary(&layers), [(Slot::Outgoing, 7, FULL, CROP_4_3, 1.0)]);
    // The PP song's own size (`MO3ucp2mBnM`, 2880×2160).
    let layers = Composition::Picture(picture(9, 2880, 2160)).layers();
    assert_eq!(summary(&layers), [(Slot::Outgoing, 9, FULL, CROP_4_3, 1.0)]);
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
            (Slot::Outgoing, 7, FULL, WHOLE, 0.75),
            (Slot::Incoming, 8, FULL, CROP_4_3, 0.25),
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
    assert_eq!(
        summary(&fade(0).layers()),
        [(Slot::Outgoing, 7, FULL, WHOLE, 1.0)]
    );
    assert_eq!(
        summary(&fade(256).layers()),
        [(Slot::Incoming, 8, FULL, CROP_4_3, 1.0)]
    );
    // A weight past 256 is all of the incoming side.
    assert_eq!(
        summary(&fade(300).layers()),
        [(Slot::Incoming, 8, FULL, CROP_4_3, 1.0)]
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
        [(Slot::Incoming, 8, FULL, CROP_4_3, 0.5)]
    );
    let down_to_black = Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: None,
        weight_q8: 192,
    };
    assert_eq!(
        summary(&down_to_black.layers()),
        [(Slot::Outgoing, 7, FULL, WHOLE, 0.25)]
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
        summary(&plain.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT)),
        [(Slot::Outgoing, 7, FHD_FULL, WHOLE, 1.0)]
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
        summary(&wide.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT)),
        [(Slot::Outgoing, 9, letterbox, WHOLE, 1.0)]
    );
    // A target taller than wide: the width binds, the rows are centred
    // (253 rows floored to even, 274 = (800 − 252) / 2). Wider than its
    // target, the picture is placed alike by both rules.
    let tall = Placement {
        w: 600,
        h: 252,
        off_x: 0,
        off_y: 274,
    };
    for fit in [Fit::Aspect, Fit::FillWidth] {
        assert_eq!(
            summary(&wide.layers_in(600, 800, fit)),
            [(Slot::Outgoing, 9, tall, WHOLE, 1.0)],
            "{fit:?}"
        );
    }
}

/// #267: the `SP-program` sender carries the NDI `SP-program`'s picture, so
/// a 4:3 picture keeps its bars there while MAX fills its width with it.
#[test]
fn the_fhd_target_keeps_the_bars_max_fills_the_width() {
    let narrow = Composition::Picture(picture(8, 1440, 1080));
    assert_eq!(
        summary(&narrow.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT)),
        [(Slot::Outgoing, 8, FHD_PILLARBOX, WHOLE, 1.0)]
    );
    assert_eq!(
        summary(&narrow.layers()),
        [(Slot::Outgoing, 8, FULL, CROP_4_3, 1.0)]
    );
    // The fill rule in the 1920×1080 target: the same crop, FHD's place.
    assert_eq!(
        summary(&narrow.layers_in(FHD_WIDTH, FHD_HEIGHT, Fit::FillWidth)),
        [(Slot::Outgoing, 8, FHD_FULL, CROP_4_3, 1.0)]
    );
    // And the aspect rule in MAX's canvas: the pillarbox MAX no longer has.
    assert_eq!(
        summary(&narrow.layers_in(CANVAS_WIDTH, CANVAS_HEIGHT, Fit::Aspect)),
        [(Slot::Outgoing, 8, PILLARBOX, WHOLE, 1.0)]
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
        summary(&fade.layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT)),
        [
            (Slot::Outgoing, 7, FHD_FULL, WHOLE, 0.75),
            (Slot::Incoming, 8, FHD_PILLARBOX, WHOLE, 0.25),
        ]
    );
    assert!(
        Composition::Black
            .layers_in(FHD_WIDTH, FHD_HEIGHT, FHD_FIT)
            .is_empty(),
        "the black has no layer in any target"
    );
}

/// `layers()` is MAX's canvas by MAX's rule: a fade from a 21:9 picture
/// (wider: its letterbox, rows 270..1890) to a 4:3 one (narrower: the whole
/// width, cropped), each side placed by the rule.
#[test]
fn layers_is_layers_in_the_max_canvas() {
    let fade = Composition::Fade {
        from: Some(picture(7, 2560, 1080)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8: 100,
    };
    assert_eq!(
        summary(&fade.layers()),
        summary(&fade.layers_in(CANVAS_WIDTH, CANVAS_HEIGHT, MAX_FIT))
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
                WHOLE,
                156.0 / 256.0
            ),
            (Slot::Incoming, 8, FULL, CROP_4_3, 100.0 / 256.0),
        ]
    );
}

/// A portrait (9:16) picture in MAX: the whole width, 2332 of its 6826
/// scaled rows cut off the top (`sp_core::fit::fill_width`).
#[test]
fn a_portrait_picture_fills_max_s_width_showing_its_middle() {
    let layers = Composition::Picture(picture(5, 1080, 1920)).layers();
    let crop = [0.0, 0.341_634_93, 1.0, 0.658_072_05];
    assert_eq!(summary(&layers), [(Slot::Outgoing, 5, FULL, crop, 1.0)]);
}

#[test]
fn each_rule_places_by_its_sp_core_function() {
    for (w, h) in [(1440, 1080), (2560, 1080), (1080, 1920), (2560, 1440)] {
        assert_eq!(
            Fit::Aspect.place(w, h, CANVAS_WIDTH, CANVAS_HEIGHT),
            Fill::whole(aspect_fit(w, h, CANVAS_WIDTH, CANVAS_HEIGHT)),
            "{w}x{h}"
        );
        assert_eq!(
            Fit::FillWidth.place(w, h, CANVAS_WIDTH, CANVAS_HEIGHT),
            fill_width(w, h, CANVAS_WIDTH, CANVAS_HEIGHT),
            "{w}x{h}"
        );
    }
}

#[test]
fn each_slot_has_its_own_texture_index() {
    assert_eq!(Slot::Outgoing.index(), 0);
    assert_eq!(Slot::Incoming.index(), 1);
}

/// #251 step 2: each target is its size and its rule, named once.
#[test]
fn the_two_targets_are_max_s_canvas_and_sp_program_s() {
    assert_eq!(
        (MAX_TARGET.width, MAX_TARGET.height, MAX_TARGET.fit),
        (3840, 2160, MAX_FIT)
    );
    assert_eq!(
        (FHD_TARGET.width, FHD_TARGET.height, FHD_TARGET.fit),
        (1920, 1080, FHD_FIT)
    );
}

/// (slot, picture id) of each picture one compose uploads.
fn uploads(composition: &Composition<'_>) -> Vec<(Slot, u64)> {
    composition
        .pictures()
        .iter()
        .map(|(slot, picture)| (*slot, picture.id))
        .collect()
}

/// (slot, picture id) of each layer a target draws.
fn sampled(layers: &[Layer<'_>]) -> Vec<(Slot, u64)> {
    layers.iter().map(|l| (l.slot, l.picture.id)).collect()
}

/// #251 step 2: ONE compose uploads `pictures()`, and EVERY target's
/// layers sample exactly those (the same slots, the same pictures): MAX and
/// the FHD program are drawn from one upload.
#[test]
fn every_target_draws_the_pictures_one_compose_uploads() {
    let fade = |weight_q8| Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8,
    };
    let cases = [
        (Composition::Black, vec![]),
        (
            Composition::Picture(picture(5, 1280, 720)),
            vec![(Slot::Outgoing, 5)],
        ),
        (fade(64), vec![(Slot::Outgoing, 7), (Slot::Incoming, 8)]),
        (fade(0), vec![(Slot::Outgoing, 7)]),
        (fade(256), vec![(Slot::Incoming, 8)]),
        (
            Composition::Fade {
                from: None,
                to: Some(picture(8, 1440, 1080)),
                weight_q8: 128,
            },
            vec![(Slot::Incoming, 8)],
        ),
    ];
    for (composition, want) in cases {
        assert_eq!(uploads(&composition), want, "{composition:?}");
        for target in [MAX_TARGET, FHD_TARGET] {
            assert_eq!(
                sampled(&composition.layers_on(target)),
                want,
                "{composition:?} on {target:?}"
            );
        }
    }
}

/// #251 step 2: one composition, both targets: the same weights, each
/// side placed by its target's rule (MAX fills the width, the FHD program
/// keeps its bars).
#[test]
fn one_composition_is_placed_on_each_target_by_its_rule() {
    let fade = Composition::Fade {
        from: Some(picture(7, 2560, 1440)),
        to: Some(picture(8, 1440, 1080)),
        weight_q8: 64,
    };
    assert_eq!(
        summary(&fade.layers_on(MAX_TARGET)),
        [
            (Slot::Outgoing, 7, FULL, WHOLE, 0.75),
            (Slot::Incoming, 8, FULL, CROP_4_3, 0.25),
        ]
    );
    assert_eq!(
        summary(&fade.layers_on(FHD_TARGET)),
        [
            (Slot::Outgoing, 7, FHD_FULL, WHOLE, 0.75),
            (Slot::Incoming, 8, FHD_PILLARBOX, WHOLE, 0.25),
        ]
    );
    assert_eq!(
        summary(&fade.layers()),
        summary(&fade.layers_on(MAX_TARGET)),
        "layers() is MAX's target"
    );
    let small = Target {
        width: 640,
        height: 480,
        fit: Fit::FillWidth,
    };
    assert_eq!(
        summary(&fade.layers_on(small)),
        summary(&fade.layers_in(640, 480, Fit::FillWidth)),
        "any target: its own size and rule"
    );
}
