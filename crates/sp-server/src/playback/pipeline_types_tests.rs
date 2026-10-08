//! #217: `real_start_ms`, where a Play really starts (the position its
//! `Started` reports), and `real_seek_ms`, where a song plays on from after a
//! seek (the position its `Seeked` reports). The decode path is Windows-only,
//! so the decisions are pinned here on Linux.

use std::cell::Cell;

use super::{real_seek_ms, real_start_ms};

#[test]
fn a_start_seek_that_worked_starts_where_it_asked() {
    let asked = Cell::new(None);
    let start = real_start_ms(
        Some(120_000),
        |ms| {
            asked.set(Some(ms));
            Ok::<(), String>(())
        },
        7,
        "test",
    );
    assert_eq!(start, 120_000);
    assert_eq!(
        asked.get(),
        Some(120_000),
        "the decoder seeks to the position"
    );
}

#[test]
fn a_start_seek_that_failed_starts_from_0() {
    let start = real_start_ms(Some(120_000), |_| Err("no index"), 7, "test");
    assert_eq!(start, 0, "the song plays from its start");
}

#[test]
fn no_start_position_starts_from_0_with_no_seek() {
    let asked = Cell::new(false);
    let start = real_start_ms(
        None,
        |_| {
            asked.set(true);
            Ok::<(), String>(())
        },
        7,
        "test",
    );
    assert_eq!(start, 0);
    assert!(!asked.get(), "no seek is made");
}

#[test]
fn a_seek_that_worked_plays_from_where_it_asked() {
    let asked = Cell::new(None);
    let position = real_seek_ms(
        120_000,
        30_000,
        |ms| {
            asked.set(Some(ms));
            Ok::<(), String>(())
        },
        7,
        "test",
    );
    assert_eq!(position, 120_000);
    assert_eq!(
        asked.get(),
        Some(120_000),
        "the decoder seeks to the position"
    );
}

#[test]
fn a_refused_seek_plays_on_from_where_the_decoder_was() {
    let position = real_seek_ms(120_000, 30_000, |_| Err("no index"), 7, "test");
    assert_eq!(position, 30_000, "the decoder never left 30 s");
}
