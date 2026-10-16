//! #278: the playlist EQ panel's view maths. The exact pins come from a
//! scratch model of the same formulas (Rust's half-away-from-zero rounding).
//! Wired via `#[cfg(test)] #[path = "audio_fx_view_tests.rs"] mod tests;`.

use super::{
    BOX_H, BOX_W, CURVE_POINTS, DB_GRID, FREQ_GRID, SLIDER_STEPS, WHEEL_Q_FACTOR, WheelTurn,
    band_path, display_order, dragged, dragged_from, fill_path, freq_label, freq_slider,
    gain_label, grab_offset, handle_xy, q_label, q_slider, slider_freq, slider_q, slider_shown,
    snap_freq, snap_gain, snap_q, to_box, wheel_q, x_freq, y_db,
};
use crate::audio_fx::{
    BandKind, CURVE_DB_MAX, CURVE_DB_MIN, DEFAULT_Q, EqBand, PlaylistFx, curve_path, db_y,
    default_band, freq_x,
};

fn close(got: f64, want: f64, tol: f64) {
    assert!((got - want).abs() <= tol, "{got} vs {want} (±{tol})");
}

fn band(kind: BandKind, freq_hz: f64, gain_db: f64, q: f64) -> EqBand {
    EqBand {
        kind,
        freq_hz,
        gain_db,
        q,
        enabled: true,
    }
}

#[test]
fn the_box_the_points_the_steps_and_the_wheel_factor() {
    assert_eq!((BOX_W, BOX_H), (800.0, 300.0));
    assert_eq!(CURVE_POINTS, 200);
    assert_eq!(SLIDER_STEPS, 1000);
    assert_eq!(WHEEL_Q_FACTOR, 1.1);
}

/// The grid marks 20 Hz … 20 kHz and every 6 dB inside the −36 … +18 box,
/// the 0 dB line among them.
#[test]
fn the_grid_marks_the_decades_and_every_six_db() {
    let freqs: Vec<f64> = FREQ_GRID.iter().map(|g| g.0).collect();
    assert_eq!(
        freqs,
        [
            20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 20_000.0
        ]
    );
    let labels: Vec<&str> = FREQ_GRID.iter().map(|g| g.1).collect();
    assert_eq!(
        labels,
        [
            "20", "50", "100", "200", "500", "1k", "2k", "5k", "10k", "20k"
        ]
    );
    let levels: Vec<f64> = DB_GRID.iter().map(|g| g.0).collect();
    assert_eq!(levels, [12.0, 6.0, 0.0, -6.0, -12.0, -18.0, -24.0, -30.0]);
    let labels: Vec<&str> = DB_GRID.iter().map(|g| g.1).collect();
    assert_eq!(labels, ["+12", "+6", "0", "−6", "−12", "−18", "−24", "−30"]);
    for (level, _) in DB_GRID {
        assert!(level > CURVE_DB_MIN && level < CURVE_DB_MAX, "{level}");
    }
}

/// A pointer's offset into the drawn SVG maps into the box's units; an
/// element not drawn (0 px) maps everything to 0.
#[test]
fn a_pointer_maps_into_the_box() {
    close(to_box(200.0, 400.0, 800.0), 400.0, 1e-12);
    close(to_box(-10.0, 400.0, 800.0), -20.0, 1e-12);
    close(to_box(150.0, 150.0, 300.0), 300.0, 1e-12);
    close(to_box(37.0, 0.0, 800.0), 0.0, 0.0);
}

/// x → frequency is `freq_x`'s inverse on the log axis, held to the band
/// limits past either end.
#[test]
fn x_maps_back_to_the_frequency_on_the_log_axis() {
    close(x_freq(0.0, 800.0), 20.0, 1e-12);
    close(x_freq(400.0, 800.0), 632.455_532_033_675_8, 1e-9);
    close(x_freq(200.0, 800.0), 112.468_265_038_069_8, 1e-9);
    close(x_freq(800.0, 800.0), 20_000.0, 1e-9);
    close(x_freq(-10.0, 800.0), 20.0, 0.0);
    close(x_freq(900.0, 800.0), 20_000.0, 0.0);
    for f in [20.0, 80.0, 1234.0, 16_000.0] {
        close(x_freq(freq_x(f, BOX_W), BOX_W), f, 1e-9);
    }
}

/// y → level is `db_y`'s inverse: +18 dB at the top, −36 dB at the bottom,
/// 0 dB a third down, held to the box.
#[test]
fn y_maps_back_to_the_level() {
    close(y_db(0.0, 300.0), 18.0, 1e-12);
    close(y_db(100.0, 300.0), 0.0, 1e-12);
    close(y_db(150.0, 300.0), -9.0, 1e-12);
    close(y_db(37.5, 300.0), 11.25, 1e-12);
    close(y_db(300.0, 300.0), -36.0, 1e-12);
    close(y_db(-10.0, 300.0), 18.0, 0.0);
    close(y_db(400.0, 300.0), -36.0, 0.0);
    for db in [-30.0, -6.5, 0.0, 12.0] {
        close(y_db(db_y(db, BOX_H), BOX_H), db, 1e-9);
    }
}

/// A frequency is held to the limits and rounded to three significant
/// digits: whole hertz below 1 kHz, 10 Hz to 10 kHz, 100 Hz above.
#[test]
fn a_frequency_snaps_to_three_significant_digits() {
    for (hz, want) in [
        (5.0, 20.0),
        (55.0, 55.0),
        (55.4, 55.0),
        (555.0, 555.0),
        (632.455_532, 632.0),
        (999.4, 999.0),
        (999.6, 1000.0),
        (1000.0, 1000.0),
        (1234.0, 1230.0),
        (1235.0, 1240.0),
        (9994.0, 9990.0),
        (9996.0, 10_000.0),
        (12_345.0, 12_300.0),
        (12_350.0, 12_400.0),
        (19_990.0, 20_000.0),
        (25_000.0, 20_000.0),
    ] {
        close(snap_freq(hz), want, 1e-9);
    }
}

#[test]
fn a_gain_snaps_to_a_tenth_of_a_db_inside_the_limits() {
    for (db, want) in [
        (2.04, 2.0),
        (2.06, 2.1),
        (-3.46, -3.5),
        (0.04, 0.0),
        (20.0, 12.0),
        (-40.0, -30.0),
    ] {
        close(snap_gain(db), want, 1e-12);
    }
}

#[test]
fn a_q_snaps_to_a_hundredth_inside_the_limits() {
    for (q, want) in [
        (DEFAULT_Q, 0.71),
        (1.234, 1.23),
        (0.111, 0.11),
        (0.05, 0.1),
        (12.0, 10.0),
    ] {
        close(snap_q(q), want, 1e-12);
    }
}

/// The values read in Slovak: a decimal comma, kHz from 1 kHz, a sign on
/// a gain (none on 0, a minus sign, never `-0,0`).
#[test]
fn the_values_read_as_the_panel_shows_them() {
    for (hz, want) in [
        (80.0, "80 Hz"),
        (5.0, "20 Hz"),
        (632.455_532, "632 Hz"),
        (999.6, "1 kHz"),
        (1000.0, "1 kHz"),
        (1234.0, "1,23 kHz"),
        (1250.0, "1,25 kHz"),
        (12_345.0, "12,3 kHz"),
        (16_000.0, "16 kHz"),
        (20_000.0, "20 kHz"),
    ] {
        assert_eq!(freq_label(hz), want, "{hz}");
    }
    for (db, want) in [
        (2.5, "+2,5 dB"),
        (2.04, "+2,0 dB"),
        (0.0, "0,0 dB"),
        (-0.04, "0,0 dB"),
        (0.04, "0,0 dB"),
        (-3.46, "−3,5 dB"),
        (-30.0, "−30,0 dB"),
        (20.0, "+12,0 dB"),
    ] {
        assert_eq!(gain_label(db), want, "{db}");
    }
    assert_eq!(q_label(DEFAULT_Q), "Q 0,71");
    assert_eq!(q_label(1.0), "Q 1,00");
    assert_eq!(q_label(12.0), "Q 10,00");
}

/// #278 review round 3: while dragged, a slider shows the pointer's
/// position, else its value's.
#[test]
fn a_dragged_slider_shows_the_pointers_position() {
    assert_eq!(slider_shown(true, "523", "520"), "523");
    assert_eq!(slider_shown(false, "523", "520"), "520");
    assert_eq!(slider_shown(true, 7_u32, 9), 7);
}

/// The frequency slider is a log scale over 0 … 1000; a position reads back
/// as the snapped frequency.
#[test]
fn the_frequency_slider_is_a_log_scale() {
    for (hz, want) in [
        (20.0, 0),
        (80.0, 201),
        (200.0, 333),
        (632.455_532, 500),
        (2000.0, 667),
        (16_000.0, 968),
        (20_000.0, 1000),
        (10.0, 0),
        (30_000.0, 1000),
    ] {
        assert_eq!(freq_slider(hz), want, "{hz}");
    }
    for (position, want) in [
        (0, 20.0),
        (250, 112.0),
        (333, 200.0),
        (500, 632.0),
        (667, 2000.0),
        (1000, 20_000.0),
    ] {
        close(slider_freq(position), want, 1e-9);
    }
}

#[test]
fn the_q_slider_is_a_log_scale() {
    for (q, want) in [
        (0.1, 0),
        (DEFAULT_Q, 425),
        (1.0, 500),
        (2.0, 651),
        (10.0, 1000),
        (0.05, 0),
        (12.0, 1000),
    ] {
        assert_eq!(q_slider(q), want, "{q}");
    }
    for (position, want) in [(0, 0.1), (250, 0.32), (425, 0.71), (500, 1.0), (1000, 10.0)] {
        close(slider_q(position), want, 1e-12);
    }
}

/// The wheel over a handle: ×1,1 per notch, up (negative) narrows, snapped,
/// held to the Q limits.
#[test]
fn the_wheel_steps_q_by_a_tenth_per_notch() {
    for (q, delta_y, want) in [
        (1.0, -100.0, 1.1),
        (1.0, 100.0, 0.91),
        (1.0, -300.0, 1.33),
        (2.0, -50.0, 2.1),
        (1.0, 0.0, 1.0),
        (9.5, -100.0, 10.0),
        (0.11, 100.0, 0.1),
    ] {
        close(wheel_q(q, delta_y), want, 1e-12);
    }
}

/// Q after each of `deltas`, one `WheelTurn` over a handle from `q`.
fn turns(q: f64, deltas: &[f64]) -> Vec<f64> {
    let mut turn = WheelTurn::default();
    let mut q = q;
    deltas
        .iter()
        .map(|delta_y| {
            q = turn.turn(q, *delta_y);
            q
        })
        .collect()
}

fn assert_turns(got: Vec<f64>, want: &[f64]) {
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (g, w) in got.iter().zip(want) {
        close(*g, *w, 1e-12);
    }
}

/// #278 review rounds 1-2: small wheel events (a touchpad's few pixels, a
/// percent-based scroll) add up until they reach the next 0,01 of Q — Q
/// never sticks and moves no faster than with a mouse wheel; a zero event
/// keeps what is pending.
#[test]
fn small_wheel_turns_add_up_until_q_moves() {
    assert_turns(
        turns(1.0, &[-4.0, -4.0, -4.0, -4.0]),
        &[1.0, 1.01, 1.01, 1.02],
    );
    assert_turns(turns(1.0, &[-4.0, 0.0, -4.0]), &[1.0, 1.0, 1.01]);
    assert_turns(turns(1.0, &[-100.0, -100.0]), &[1.1, 1.21]);
}

/// A turn the other way starts afresh: neither what the old way left nor a
/// turn past a Q limit is owed.
#[test]
fn a_wheel_turn_the_other_way_starts_afresh() {
    assert_turns(turns(1.0, &[-4.0, 4.0, 4.0]), &[1.0, 1.0, 0.99]);
    assert_turns(turns(10.0, &[-100.0, 100.0]), &[10.0, 9.09]);
}

/// A shelf's or a peak's handle sits at its frequency and gain; a
/// high-/low-pass on the 0 dB line whatever gain it keeps.
#[test]
fn a_handle_sits_at_its_frequency_and_gain() {
    let (x, y) = handle_xy(&band(BandKind::Peak, 1000.0, 6.0, 1.0), 800.0, 300.0);
    close(x, 453.058_667_822_938_4, 1e-9);
    close(y, 66.666_666_666_666_67, 1e-9);
    let (x, y) = handle_xy(
        &band(BandKind::HighPass, 80.0, 9.0, DEFAULT_Q),
        800.0,
        300.0,
    );
    close(x, 160.549_331_020_79, 1e-9);
    close(y, 100.0, 1e-12);
    let (_, y) = handle_xy(
        &band(BandKind::LowPass, 8000.0, -12.0, DEFAULT_Q),
        800.0,
        300.0,
    );
    close(y, 100.0, 1e-12);
    let (_, y) = handle_xy(
        &band(BandKind::LowShelf, 120.0, -12.0, DEFAULT_Q),
        800.0,
        300.0,
    );
    close(y, db_y(-12.0, 300.0), 1e-12);
}

/// A dragged handle takes the frequency under the pointer and (a shelf, a
/// peak) the gain, snapped and held to the limits; a high-/low-pass keeps
/// its gain; the type, Q and on / off stay.
#[test]
fn a_dragged_handle_takes_the_frequency_and_gain_under_the_pointer() {
    let mut peak = band(BandKind::Peak, 1000.0, 0.0, 2.5);
    peak.enabled = false;
    let moved = dragged(&peak, 600.0, 60.0, 800.0, 300.0);
    assert_eq!(
        moved,
        EqBand {
            kind: BandKind::Peak,
            freq_hz: 3560.0,
            gain_db: 7.2,
            q: 2.5,
            enabled: false,
        }
    );
    close(
        dragged(&peak, 400.0, 100.0, 800.0, 300.0).freq_hz,
        632.0,
        0.0,
    );
    close(dragged(&peak, 400.0, 100.0, 800.0, 300.0).gain_db, 0.0, 0.0);
    // Past the band limits (the box goes to +18 / −36 dB).
    close(dragged(&peak, 600.0, 10.0, 800.0, 300.0).gain_db, 12.0, 0.0);
    close(
        dragged(&peak, 600.0, 290.0, 800.0, 300.0).gain_db,
        -30.0,
        0.0,
    );
    close(dragged(&peak, -50.0, 0.0, 800.0, 300.0).freq_hz, 20.0, 0.0);
    let hp = band(BandKind::HighPass, 80.0, 3.0, DEFAULT_Q);
    let moved = dragged(&hp, 600.0, 10.0, 800.0, 300.0);
    assert_eq!(moved, band(BandKind::HighPass, 3560.0, 3.0, DEFAULT_Q));
}

/// A drag moves the band by the pointer's travel from where it took hold,
/// never to the pointer: held 12 units right of and 5 above the dot's
/// centre, the band stays put until the pointer moves, then follows as
/// if the centre had been taken.
#[test]
fn a_drag_moves_the_band_by_the_pointers_travel_from_where_it_took_hold() {
    let peak = band(BandKind::Peak, 1000.0, 3.0, 1.0);
    let (hx, hy) = handle_xy(&peak, 800.0, 300.0);
    let grab = grab_offset(&peak, hx + 12.0, hy - 5.0, 800.0, 300.0);
    close(grab.0, 12.0, 1e-9);
    close(grab.1, -5.0, 1e-9);
    // Taken off the centre and not moved: the band as it was.
    assert_eq!(
        dragged_from(&peak, grab, hx + 12.0, hy - 5.0, 800.0, 300.0),
        peak
    );
    // 30 right and 10 up: as the centre dragged there (1300 Hz, +4,8 dB).
    let moved = dragged_from(&peak, grab, hx + 42.0, hy - 15.0, 800.0, 300.0);
    assert_eq!(moved, band(BandKind::Peak, 1300.0, 4.8, 1.0));
    assert_eq!(moved, dragged(&peak, hx + 30.0, hy - 10.0, 800.0, 300.0));
    // A high-pass sits on the 0 dB line whatever its gain: its grab is
    // measured from there, and it moves on its frequency only.
    let hp = band(BandKind::HighPass, 80.0, 6.0, DEFAULT_Q);
    let (hx, hy) = handle_xy(&hp, 800.0, 300.0);
    let grab = grab_offset(&hp, hx - 7.0, hy + 9.0, 800.0, 300.0);
    close(grab.0, -7.0, 1e-9);
    close(grab.1, 9.0, 1e-9);
    assert_eq!(
        dragged_from(&hp, grab, hx - 7.0, hy + 40.0, 800.0, 300.0),
        hp
    );
}

/// A band's own curve is its filter alone (no volume), on; a band switched
/// off draws none.
#[test]
fn a_bands_own_curve_is_its_filter_alone() {
    let shelf = band(BandKind::LowShelf, 200.0, -9.0, DEFAULT_Q);
    let alone = PlaylistFx {
        gain_db: 0.0,
        eq: vec![shelf],
    };
    assert_eq!(
        band_path(&shelf, 300.0, 108.0, 5),
        curve_path(&alone, 300.0, 108.0, 5)
    );
    assert!(band_path(&shelf, 300.0, 108.0, 5).starts_with("M0.0 "));
    let mut off = shelf;
    off.enabled = false;
    assert_eq!(band_path(&off, 300.0, 108.0, 5), "");
    // A peak's own curve ignores the playlist's volume: flat at 0 dB.
    let flat = band(BandKind::Peak, 1000.0, 0.0, 1.0);
    assert_eq!(
        band_path(&flat, 300.0, 108.0, 3),
        "M0.0 36.0 L150.0 36.0 L300.0 36.0"
    );
}

/// The fill closes the whole curve down to the 0 dB line.
#[test]
fn the_fill_closes_the_curve_to_the_zero_line() {
    let quieter = PlaylistFx {
        gain_db: -9.0,
        eq: vec![],
    };
    assert_eq!(
        fill_path(&curve_path(&quieter, 300.0, 108.0, 2), 300.0, 108.0),
        "M0.0 54.0 L300.0 54.0 L300.0 36.0 L0.0 36.0 Z"
    );
    let boosted = PlaylistFx {
        gain_db: 0.0,
        eq: vec![band(BandKind::Peak, 1000.0, 6.0, 1.0)],
    };
    let curve = curve_path(&boosted, 800.0, 300.0, 7);
    assert_eq!(
        fill_path(&curve, 800.0, 300.0),
        format!("{curve} L800.0 100.0 L0.0 100.0 Z")
    );
}

/// The cards go low to high by frequency; a tie by type (high-pass first,
/// low-pass last); then as stored.
#[test]
fn the_cards_go_by_frequency_then_by_type() {
    assert_eq!(display_order(&[]), Vec::<usize>::new());
    let eq = [
        default_band(BandKind::LowPass),
        band(BandKind::Peak, 50.0, 3.0, 1.0),
        default_band(BandKind::HighPass),
        band(BandKind::LowShelf, 120.0, 2.0, DEFAULT_Q),
    ];
    assert_eq!(display_order(&eq), [1, 2, 3, 0]);
    // One frequency: by type, then as stored.
    let tie = [
        band(BandKind::LowPass, 1000.0, 0.0, DEFAULT_Q),
        band(BandKind::Peak, 1000.0, 2.0, 1.0),
        band(BandKind::HighPass, 1000.0, 0.0, DEFAULT_Q),
        band(BandKind::Peak, 1000.0, -2.0, 1.0),
        band(BandKind::HighShelf, 1000.0, 0.0, DEFAULT_Q),
        band(BandKind::LowShelf, 1000.0, 0.0, DEFAULT_Q),
    ];
    assert_eq!(display_order(&tie), [2, 5, 1, 3, 4, 0]);
}
