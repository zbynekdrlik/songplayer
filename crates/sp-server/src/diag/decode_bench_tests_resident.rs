//! #279 P3a: a decode-bench run GPU-resident (`"resident": true`): the
//! request's mode, the producer device a resident run makes and keeps, and
//! its report (`gpu_resident`, the ring after the run, the end line).

use sp_decoder::DecodeMode;
use sp_gpu::{GpuError, RingStats};

use super::super::{BenchMode, BenchReport, DecodeFacts, decode_mode};
use super::{clean_report, on_the_gpu};
use crate::playback::gpu_resident::RingStatus;

#[test]
fn a_requests_flags_name_its_mode() {
    assert_eq!(BenchMode::of(false, false), BenchMode::Software);
    assert_eq!(BenchMode::of(true, false), BenchMode::Hardware);
    assert_eq!(BenchMode::of(false, true), BenchMode::Resident);
    assert_eq!(BenchMode::default(), BenchMode::Software);
    assert!(!BenchMode::Software.on_gpu());
    assert!(BenchMode::Hardware.on_gpu());
    assert!(BenchMode::Resident.on_gpu());
    let names = [
        BenchMode::Software.as_str(),
        BenchMode::Hardware.as_str(),
        BenchMode::Resident.as_str(),
    ];
    assert_eq!(names, ["software", "hardware", "gpu_resident"]);
    // The reader's own names for the same modes.
    assert_eq!(names[0], DecodeMode::Software.as_str());
    assert_eq!(names[1], DecodeMode::Hardware.as_str());
}

#[test]
fn only_a_resident_run_makes_a_producer_device() {
    let never = || -> Result<sp_gpu::ProducerGpu, GpuError> { panic!("made for no reason") };
    let (mode, producer) = decode_mode(BenchMode::Software, never).expect("software");
    assert_eq!(mode, DecodeMode::Software);
    assert!(producer.is_none());
    let (mode, producer) = decode_mode(BenchMode::Hardware, never).expect("hardware");
    assert_eq!(mode, DecodeMode::Hardware);
    assert!(producer.is_none());
    // A device that cannot be made is the run's error (off Windows always).
    let refused = decode_mode(BenchMode::Resident, || Err(GpuError::Unsupported));
    assert_eq!(refused.err(), Some(GpuError::Unsupported));
    let production = decode_mode(BenchMode::Resident, sp_gpu::ProducerGpu::new);
    if cfg!(not(windows)) {
        assert_eq!(production.err(), Some(GpuError::Unsupported));
    }
}

/// On a WARP producer (CI has no adapter for the production one): the
/// reader's mode is the producer the run keeps.
#[cfg(windows)]
#[test]
fn a_resident_run_decodes_on_the_producer_it_keeps() {
    let made = decode_mode(BenchMode::Resident, sp_gpu::ProducerGpu::new_warp);
    let (mode, producer) = made.expect("a WARP producer");
    let producer = producer.expect("kept for its ring");
    match mode {
        DecodeMode::GpuResident(on) => assert!(std::sync::Arc::ptr_eq(&on, &producer)),
        other => panic!("expected GPU-resident, got {other:?}"),
    }
}

/// A resident run's ring after the run: 20 4K slots made.
fn a_4k_ring() -> RingStats {
    RingStats {
        slots: 20,
        in_use: 0,
        waits: 3,
        full: 0,
        dead: false,
        copy_us_p99: Some(410),
        vram_bytes: 20 * 12_441_600,
    }
}

#[test]
fn a_resident_run_reports_its_ring() {
    let report = clean_report().with_decode(DecodeFacts {
        mode: BenchMode::Resident,
        ring: Some(a_4k_ring()),
        ..on_the_gpu()
    });
    assert!(report.hw_requested, "a resident run decodes on the GPU");
    assert!(report.gpu_resident);
    assert_eq!(
        report.ring,
        Some(RingStatus {
            slots: 20,
            in_use: 0,
            waits: 3,
            full: 0,
            dead: false,
            copy_us_p99: Some(410),
            vram_mb: 249,
        })
    );
    assert_eq!(
        report.adapter.as_deref(),
        Some("NVIDIA GeForce RTX 3070 Ti")
    );
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["gpu_resident"], true);
    assert_eq!(json["ring"]["slots"], 20);
    assert_eq!(json["ring"]["copy_us_p99"], 410);
    assert_eq!(json["ring"]["vram_mb"], 249);
    assert!(
        report.summary().ends_with(
            " hw=requested decode_path=hardware path_changes=0 \
             adapter=NVIDIA GeForce RTX 3070 Ti hw_fallback=- \
             gpu_resident ring_slots=20 ring_waits=3 ring_full=0 ring_dead=false \
             copy_us_p99=410 vram_mb=249"
        ),
        "{}",
        report.summary()
    );
}

#[test]
fn a_resident_run_with_no_producer_says_so() {
    let ring = RingStats {
        copy_us_p99: None,
        full: 1,
        dead: true,
        ..a_4k_ring()
    };
    let no_copy = clean_report().with_decode(DecodeFacts {
        mode: BenchMode::Resident,
        ring: Some(ring),
        ..on_the_gpu()
    });
    assert!(
        no_copy
            .summary()
            .ends_with(" ring_full=1 ring_dead=true copy_us_p99=- vram_mb=249"),
        "{}",
        no_copy.summary()
    );
    let failed =
        BenchReport::open_failed("x.mp4", "producer device: no adapter".to_string(), 0, None)
            .with_decode(DecodeFacts {
                mode: BenchMode::Resident,
                ..DecodeFacts::default()
            });
    assert!(failed.failed(), "the 500 with the report");
    assert!(failed.gpu_resident);
    assert_eq!(failed.ring, None);
    assert!(
        failed
            .summary()
            .ends_with(" hw_fallback=- gpu_resident ring=- error=producer device: no adapter"),
        "{}",
        failed.summary()
    );
    // Any other run has no ring, and says nothing of one.
    for report in [clean_report(), clean_report().with_decode(on_the_gpu())] {
        assert!(!report.gpu_resident);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["gpu_resident"], false);
        assert!(json["ring"].is_null());
        assert!(!report.summary().contains("gpu_resident"));
    }
    // A ring a non-resident run carried is still reported (never dropped),
    // but the end line names it only for a resident run.
    let odd = clean_report().with_decode(DecodeFacts {
        ring: Some(a_4k_ring()),
        ..on_the_gpu()
    });
    assert!(odd.ring.is_some() && !odd.gpu_resident);
}
