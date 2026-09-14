//! Tests for `genlock::burn` (#151) — ported from camera-box
//! `src/probe/payload.rs` (payload/CRC vectors) and
//! `vendor/distroav/src/burn-geom.hpp` + `tests/burn_payload_parity.rs`
//! (geometry numbers). Included via `#[path] mod genlock_burn_tests;` from
//! `genlock.rs`, so `super::burn` is this crate's `genlock::burn`.

use super::burn::*;

// ---- CRC-32 ----

#[test]
fn crc32_known_answer_123456789() {
    // The canonical CRC-32/ISO-HDLC check value — proves this matches
    // camera-box's `crc` crate exactly.
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn crc32_empty_is_zero() {
    // Init xor xorout with no input.
    assert_eq!(crc32(b""), 0);
}

// ---- Payload (camera-box payload.rs vectors) ----

#[test]
fn payload_roundtrip_preserves_fields() {
    // camera-box `roundtrip_preserves_fields`.
    let s = payload(42, 9001, 1_234_567_890);
    assert_eq!(decode(&s), Some((42, 9001, 1_234_567_890)));
}

#[test]
fn payload_zero_values_roundtrip() {
    // camera-box `zero_values_roundtrip`.
    let s = payload(0, 0, 0);
    assert_eq!(decode(&s), Some((0, 0, 0)));
}

#[test]
fn payload_wire_format_is_exact() {
    // Wire format `P{run}.{frame}.{ts}.{crc}` — the CRC covers the dotted body.
    let s = payload(42, 9001, 1_234_567_890);
    let crc = crc32(b"42.9001.1234567890");
    assert_eq!(s, format!("P42.9001.1234567890.{crc}"));
}

#[test]
fn decode_rejects_corrupted_crc() {
    // camera-box `corrupted_crc_is_rejected`.
    let mut s = payload(1, 2, 3);
    let last = s.pop().unwrap();
    s.push(if last == '0' { '1' } else { '0' });
    assert_eq!(decode(&s), None);
}

#[test]
fn decode_rejects_garbage() {
    // camera-box `garbage_is_rejected`.
    assert_eq!(decode("hello world"), None);
    assert_eq!(decode("P1.2.3"), None);
    assert_eq!(decode(""), None);
}

#[test]
fn songplayer_run_id_is_reserved_911014() {
    assert_eq!(SONGPLAYER_RUN_ID, 911_014);
}

// ---- Geometry (camera-box burn-geom.hpp / burn_payload_parity.rs) ----

#[test]
fn geometry_1080p_matches_fleet() {
    // camera-box `tests/burn_payload_parity.rs`: auto @1080 -> side 302
    // (0.28*h), margin 40. (The design comment's "296" is the stale hpp
    // code-comment; the fleet's parity test asserts 302 — ported here.)
    let g = geometry(1920, 1080);
    assert_eq!(g.side, 302);
    assert_eq!(g.margin, 40);
    // Bottom-right: right edge 1920-40, bottom edge 1080-40.
    assert_eq!(g.x, 1920 - 40 - 302);
    assert_eq!(g.y, 1080 - 40 - 302);
}

#[test]
fn geometry_2160p_matches_fleet() {
    // camera-box `burn_payload_parity.rs`: auto @2160 -> side 604, margin 80.
    let g = geometry(3840, 2160);
    assert_eq!(g.side, 604);
    assert_eq!(g.margin, 80);
    assert_eq!(g.x, 3840 - 80 - 604);
    assert_eq!(g.y, 2160 - 80 - 604);
}

#[test]
fn geometry_side_floored_at_64() {
    // 0.28*100 = 28 < 64 -> floored to a readable 64 px.
    let g = geometry(200, 100);
    assert_eq!(g.side, 64);
}

// ---- Committed decode fixture (#151 item 3, for camera-box#1301) ----

#[test]
fn fixture_payload_matches_committed() {
    // The committed fixture PNG carries this payload; the .txt is its authority.
    let committed = include_str!("../../../eval/fixtures/burn/sp-burn-1080p.txt").trim();
    let expected = payload(SONGPLAYER_RUN_ID, 1, 1_700_000_000_000_000_000);
    assert_eq!(committed, expected);
    assert_eq!(
        decode(committed),
        Some((SONGPLAYER_RUN_ID, 1, 1_700_000_000_000_000_000))
    );
}
