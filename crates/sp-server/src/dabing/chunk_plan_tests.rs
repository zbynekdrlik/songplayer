//! Unit tests for the pure dub chunking + placement decisions (#183 D4).

use super::*;

fn sil(start_ms: u64, end_ms: u64) -> Silence {
    Silence { start_ms, end_ms }
}

// ── plan_chunks ───────────────────────────────────────────────────────────────

#[test]
fn short_audio_is_a_single_chunk() {
    let cfg = ChunkPlanConfig::default();
    // Well under 8 min, with pauses that must be IGNORED (no split needed).
    let sils = [sil(60_000, 61_000), sil(120_000, 121_000)];
    let chunks = plan_chunks(&sils, 200_000, &cfg);
    assert_eq!(chunks, vec![Chunk { start_ms: 0, end_ms: 200_000 }]);
}

#[test]
fn empty_audio_is_no_chunks() {
    assert!(plan_chunks(&[], 0, &ChunkPlanConfig::default()).is_empty());
}

#[test]
fn splits_at_the_latest_qualifying_pause_within_the_ceiling() {
    let cfg = ChunkPlanConfig::default(); // max 480_000, min pause 700
    // 12 min of audio with pauses at 5 min and 7 min. From start=0 the latest
    // pause <= 480_000 is the one at 420_000 (7 min), so the first cut is its
    // midpoint. (The 5-min pause is eligible but earlier, so not chosen.)
    let sils = [
        sil(300_000, 301_000), // 5:00, 1 s
        sil(420_000, 421_000), // 7:00, 1 s
    ];
    let chunks = plan_chunks(&sils, 720_000, &cfg);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].start_ms, 0);
    assert_eq!(chunks[0].end_ms, 420_500); // midpoint of the 7:00 pause
    assert_eq!(chunks[1].start_ms, 420_500);
    assert_eq!(chunks[1].end_ms, 720_000);
    // Never mid-speech: every internal boundary lands inside a real silence.
    assert!(chunks[0].end_ms >= 420_000 && chunks[0].end_ms <= 421_000);
}

#[test]
fn a_pause_shorter_than_min_is_not_a_cut_point() {
    let cfg = ChunkPlanConfig::default();
    // The only pause within the ceiling is 500 ms (< 700) → NOT eligible, so the
    // cut falls back to the hard ceiling at 480_000.
    let sils = [sil(400_000, 400_500)]; // 500 ms
    let chunks = plan_chunks(&sils, 720_000, &cfg);
    assert_eq!(chunks[0].end_ms, 480_000); // forced ceiling cut, pause ignored
}

#[test]
fn no_qualifying_pause_forces_a_ceiling_cut() {
    let cfg = ChunkPlanConfig::default();
    // 20 min, no pauses at all → hard cuts at the 8-min ceiling, last is remainder.
    let chunks = plan_chunks(&[], 1_200_000, &cfg);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], Chunk { start_ms: 0, end_ms: 480_000 });
    assert_eq!(chunks[1], Chunk { start_ms: 480_000, end_ms: 960_000 });
    assert_eq!(chunks[2], Chunk { start_ms: 960_000, end_ms: 1_200_000 });
}

#[test]
fn every_chunk_stays_within_the_ceiling_and_covers_the_whole_timeline() {
    let cfg = ChunkPlanConfig::default();
    let sils = [
        sil(200_000, 201_000),
        sil(450_000, 451_500),
        sil(700_000, 701_000),
        sil(1_000_000, 1_001_000),
    ];
    let total = 1_300_000;
    let chunks = plan_chunks(&sils, total, &cfg);
    // Contiguous, covering [0, total), each <= ceiling.
    assert_eq!(chunks.first().unwrap().start_ms, 0);
    assert_eq!(chunks.last().unwrap().end_ms, total);
    for w in chunks.windows(2) {
        assert_eq!(w[0].end_ms, w[1].start_ms);
    }
    for c in &chunks {
        assert!(c.len_ms() <= cfg.max_chunk_ms, "chunk {c:?} exceeds ceiling");
        assert!(c.len_ms() > 0);
    }
}

#[test]
fn silence_helpers() {
    let s = sil(1_000, 1_800);
    assert_eq!(s.len_ms(), 800);
    assert_eq!(s.mid_ms(), 1_400);
}

// ── placement_for ─────────────────────────────────────────────────────────────

#[test]
fn output_is_placed_at_the_chunk_start() {
    let p = placement_for(120_000, 60_000, 55_000, Some(180_000));
    assert_eq!(p.at_ms, 120_000);
}

#[test]
fn no_tempo_change_when_output_fits_before_next_chunk() {
    // out_len 55 s, available 60 s → fits, tempo stays 1.0.
    let p = placement_for(0, 60_000, 55_000, Some(60_000));
    assert!((p.tempo - 1.0).abs() < 1e-6);
}

#[test]
fn speeds_up_only_enough_to_fit_capped_at_max_tempo() {
    // available 60 s, out 63 s → needs 1.05×, under the 1.08 cap.
    let p = placement_for(0, 60_000, 63_000, Some(60_000));
    assert!((p.tempo - 1.05).abs() < 1e-3, "tempo was {}", p.tempo);
    assert!(p.tempo <= MAX_TEMPO);
}

#[test]
fn overrun_beyond_max_tempo_is_clamped_to_max_tempo() {
    // out 90 s into 60 s would need 1.5× → clamp to 1.08 (some overrun accepted).
    let p = placement_for(0, 60_000, 90_000, Some(60_000));
    assert!((p.tempo - MAX_TEMPO).abs() < 1e-6);
}

#[test]
fn last_chunk_is_never_sped_up() {
    // No following chunk → tempo 1.0 even if long.
    let p = placement_for(600_000, 120_000, 130_000, None);
    assert!((p.tempo - 1.0).abs() < 1e-6);
    assert_eq!(p.at_ms, 600_000);
}

#[test]
fn drift_is_signed_output_minus_source() {
    assert_eq!(drift_ms(60_000, 63_000), 3_000);
    assert_eq!(drift_ms(60_000, 58_000), -2_000);
    assert_eq!(drift_ms(60_000, 60_000), 0);
}

// ── parse_silencedetect ─────────────────────────────────────────────────────────

#[test]
fn parse_silencedetect_reads_duration_and_intervals() {
    let stderr = "\
Input #0, wav, from 'a.flac':
  Duration: 00:02:05.50, start: 0.000000, bitrate: 256 kb/s
[silencedetect @ 0x1] silence_start: 10.5
[silencedetect @ 0x1] silence_end: 11.4 | silence_duration: 0.9
[silencedetect @ 0x1] silence_start: 60.0
[silencedetect @ 0x1] silence_end: 61.25 | silence_duration: 1.25
";
    let (total, sils) = parse_silencedetect(stderr);
    assert_eq!(total, Some(125_500)); // 2:05.50
    assert_eq!(sils.len(), 2);
    assert_eq!(sils[0], Silence { start_ms: 10_500, end_ms: 11_400 });
    assert_eq!(sils[1], Silence { start_ms: 60_000, end_ms: 61_250 });
}

#[test]
fn parse_silencedetect_closes_trailing_silence_at_eof() {
    let stderr = "\
  Duration: 00:00:30.00, start: 0.000000
[silencedetect @ 0x1] silence_start: 28.0
";
    let (total, sils) = parse_silencedetect(stderr);
    assert_eq!(total, Some(30_000));
    assert_eq!(sils, vec![Silence { start_ms: 28_000, end_ms: 30_000 }]);
}

#[test]
fn parse_silencedetect_handles_no_silences() {
    let (total, sils) = parse_silencedetect("  Duration: 00:00:10.00, start: 0.0\n");
    assert_eq!(total, Some(10_000));
    assert!(sils.is_empty());
}

