//! Unit tests for the preview stage probes (#184 round G4).

use std::time::Duration;

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn shared_probe_closes_a_window_after_one_second_with_the_drops() {
    let t0 = Instant::now();
    let p = SharedLevelProbe::new(t0);
    assert_eq!(p.record(&[1.0, -1.0], t0 + ms(400)), None);
    p.note_dropped();
    p.note_dropped();
    assert_eq!(p.pending().1, 2);

    let r = p
        .record(&[1.0, -1.0], t0 + ms(1000))
        .expect("window due at 1 s");
    assert!(r.rms_dbfs.abs() < 1e-4, "full-scale blocks read 0 dBFS");
    assert_eq!(r.samples, 4);
    assert_eq!(r.blocks, 2);
    assert_eq!(r.dropped_blocks, 2);
    assert_eq!(r.window_ms, 1000);

    // The next window starts empty at the close time.
    assert_eq!(p.pending(), (sp_core::audio_level::SILENCE_FLOOR_DBFS, 0));
    assert_eq!(p.record(&[0.0, 0.0], t0 + ms(1999)), None);
    let r = p.record(&[0.0, 0.0], t0 + ms(2000)).unwrap();
    assert_eq!(r.rms_dbfs, sp_core::audio_level::SILENCE_FLOOR_DBFS);
    assert_eq!((r.samples, r.blocks, r.dropped_blocks), (4, 2, 0));
}

#[test]
fn stereo_samples_ms_converts_48k_interleaved_stereo() {
    assert_eq!(stereo_samples_ms(96_000), 1000);
    assert_eq!(stereo_samples_ms(96), 1);
    assert_eq!(stereo_samples_ms(95), 0);
    assert_eq!(stereo_samples_ms(0), 0);
}
