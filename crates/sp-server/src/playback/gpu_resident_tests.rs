//! #279 P3a: the precondition, the producer lease and the pipelines' table
//! (`gpu_resident.rs`), on Linux over a fake producer. Wired via
//! `#[cfg(test)] #[path = "gpu_resident_tests.rs"] mod tests;`.
//!
//! The table is process-wide: every test uses playlist ids of its own
//! (27_9xx), never another test's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sp_core::config::{NdiSource, VideoDecode};
use sp_decoder::DecodeMode;
use sp_gpu::RingStats;

use super::{
    BLOCKED_NDI_CPU, Lease, Leased, PRODUCER_IDLE_RELEASE, PRODUCER_REBUILD_BACKOFF, PipelineGpu,
    Pipelines, Producer, Resident, ResidentLog, RingStatus, note_song_path, pipelines,
    precondition_changed, resident_log, retires, song_ended, take_for_open,
};
use crate::playback::video_decode::HwDecodeSetting;

/// A producer whose ring dies when the test says, retired like
/// `ProducerGpu`.
#[derive(Debug, Default)]
struct Fake {
    lost: AtomicBool,
    retired: AtomicBool,
    stats: RingStats,
}

impl Producer for Fake {
    fn lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    fn ring(&self) -> RingStats {
        self.stats
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    fn retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }
}

fn made() -> Result<Fake, String> {
    Ok(Fake::default())
}

/// A build the test must never see.
fn never() -> Result<Fake, String> {
    panic!("the lease built a producer it must not")
}

#[test]
fn gpu_takes_effect_only_while_ndi_comes_from_the_gpu() {
    assert_eq!(BLOCKED_NDI_CPU, "ndi_source_cpu");
    let table = [
        (VideoDecode::Software, NdiSource::Cpu, Resident::Off),
        (VideoDecode::Software, NdiSource::Gpu, Resident::Off),
        (VideoDecode::Gpu, NdiSource::Gpu, Resident::On),
        (
            VideoDecode::Gpu,
            NdiSource::Cpu,
            Resident::Blocked("ndi_source_cpu"),
        ),
    ];
    for (decode, ndi, resident) in table {
        assert_eq!(Resident::of(decode, ndi), resident, "{decode:?} / {ndi:?}");
    }
    assert_eq!(Resident::Off.blocked(), None);
    assert_eq!(Resident::On.blocked(), None);
    assert_eq!(
        Resident::Blocked("ndi_source_cpu").blocked(),
        Some("ndi_source_cpu")
    );
}

#[test]
fn each_change_of_the_precondition_logs_once_a_block_as_a_warn() {
    let blocked = Resident::Blocked(BLOCKED_NDI_CPU);
    assert_eq!(
        resident_log(Resident::Off, Resident::Off),
        ResidentLog::Quiet
    );
    assert_eq!(resident_log(blocked, blocked), ResidentLog::Quiet);
    assert_eq!(
        resident_log(Resident::Off, blocked),
        ResidentLog::Warn(BLOCKED_NDI_CPU)
    );
    assert_eq!(
        resident_log(Resident::On, blocked),
        ResidentLog::Warn(BLOCKED_NDI_CPU)
    );
    assert_eq!(resident_log(blocked, Resident::On), ResidentLog::Info);
    assert_eq!(resident_log(Resident::On, Resident::Off), ResidentLog::Info);
    assert_eq!(resident_log(Resident::Off, Resident::On), ResidentLog::Info);
}

#[test]
fn the_lease_keeps_its_producer_across_songs() {
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    assert!(lease.held().is_none());
    let first = lease.take(t0, made);
    let Leased::Made(producer) = &first else {
        panic!("the first open makes the producer: {first:?}");
    };
    let later = lease.take(t0 + Duration::from_secs(5), never);
    let Leased::Kept(kept) = &later else {
        panic!("a later open keeps it: {later:?}");
    };
    assert!(Arc::ptr_eq(producer, kept), "the same producer");
    assert!(lease.held().is_some_and(|held| Arc::ptr_eq(held, producer)));
    assert!(first.producer().is_some() && later.producer().is_some());
}

#[test]
fn a_lost_producer_is_dropped_and_made_again_after_the_backoff() {
    assert_eq!(PRODUCER_REBUILD_BACKOFF, Duration::from_secs(30));
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    let Leased::Made(producer) = lease.take(t0, made) else {
        panic!("made");
    };
    producer.lost.store(true, Ordering::Relaxed);
    let at = t0 + Duration::from_secs(10);
    let lost = lease.take(at, never);
    assert!(matches!(lost, Leased::Lost), "{lost:?}");
    assert!(lost.producer().is_none());
    assert!(lease.held().is_none(), "the dead producer is let go");
    let waiting = lease.take(at + Duration::from_millis(29_999), never);
    match waiting {
        Leased::Waiting { retry_in } => assert_eq!(retry_in, Duration::from_millis(1)),
        other => panic!("the backoff runs: {other:?}"),
    }
    let again = lease.take(at + PRODUCER_REBUILD_BACKOFF, made);
    assert!(
        matches!(again, Leased::Made(_)),
        "made again at 30 s: {again:?}"
    );
}

#[test]
fn a_failed_build_waits_the_backoff_then_tries_again() {
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    let failed = lease.take(t0, || Err("no adapter".to_string()));
    match &failed {
        Leased::Failed(why) => assert_eq!(why, "no adapter"),
        other => panic!("a failed build: {other:?}"),
    }
    assert!(failed.producer().is_none());
    let waiting = lease.take(t0 + Duration::from_secs(29), never);
    assert!(matches!(waiting, Leased::Waiting { .. }), "{waiting:?}");
    assert!(waiting.producer().is_none());
    let made_now = lease.take(t0 + PRODUCER_REBUILD_BACKOFF, made);
    assert!(matches!(made_now, Leased::Made(_)), "{made_now:?}");
    let kept = lease.take(t0 + PRODUCER_REBUILD_BACKOFF, never);
    assert!(matches!(kept, Leased::Kept(_)), "no backoff left: {kept:?}");
}

#[test]
fn an_idle_producer_is_let_go_ten_minutes_after_the_pipeline_went_idle() {
    assert_eq!(PRODUCER_IDLE_RELEASE, Duration::from_secs(600));
    let min = |m: u64| Duration::from_secs(60 * m);
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    assert!(!lease.release_idle(t0), "nothing held");
    assert!(!lease.release_idle(t0 + min(30)), "still nothing held");
    let Leased::Made(_) = lease.take(t0, made) else {
        panic!("made");
    };
    // A 20 min song asks nothing; its end is the first idle heartbeat.
    let idle = t0 + min(20);
    assert!(!lease.release_idle(idle), "a long song is no idle time");
    let almost = idle + PRODUCER_IDLE_RELEASE - Duration::from_millis(1);
    assert!(!lease.release_idle(almost), "idle under 10 min keeps it");
    assert!(lease.held().is_some());
    assert!(lease.release_idle(idle + PRODUCER_IDLE_RELEASE), "let go");
    assert!(lease.held().is_none());
    assert!(
        !lease.release_idle(idle + 2 * PRODUCER_IDLE_RELEASE),
        "once"
    );
    let again = idle + 2 * PRODUCER_IDLE_RELEASE;
    assert!(
        matches!(lease.take(again, made), Leased::Made(_)),
        "no backoff after a release"
    );
    // An open between two idle stretches starts the idle time again.
    assert!(!lease.release_idle(again + min(1)), "idle from here");
    assert!(matches!(lease.take(again + min(8), never), Leased::Kept(_)));
    assert!(
        !lease.release_idle(again + min(12)),
        "idle again from here, 11 min after the first idle heartbeat"
    );
    assert!(!lease.release_idle(again + min(21)), "9 min idle");
    assert!(lease.release_idle(again + min(22)), "10 min idle");
}

#[test]
fn a_released_producer_is_made_again_at_once() {
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    assert!(!lease.release(), "nothing held");
    let Leased::Made(_) = lease.take(t0, made) else {
        panic!("made");
    };
    assert!(!lease.release_idle(t0), "idle from here");
    assert!(lease.release(), "a song opened with the precondition off");
    assert!(lease.held().is_none());
    assert!(!lease.release(), "once");
    let again = lease.take(t0 + Duration::from_secs(1), made);
    assert!(matches!(again, Leased::Made(_)), "no backoff: {again:?}");
    assert!(
        !lease.release_idle(t0 + PRODUCER_IDLE_RELEASE),
        "the idle time starts afresh after a release"
    );
}

#[test]
fn a_rings_status_rounds_its_vram_to_megabytes() {
    let stats = RingStats {
        slots: 20,
        in_use: 14,
        waits: 3,
        full: 1,
        dead: false,
        copy_us_p99: Some(410),
        vram_bytes: 20 * 12_441_600,
    };
    assert_eq!(
        RingStatus::of(stats),
        RingStatus {
            slots: 20,
            in_use: 14,
            waits: 3,
            full: 1,
            dead: false,
            copy_us_p99: Some(410),
            vram_mb: 249,
        }
    );
    let small = RingStats {
        vram_bytes: 3_110_400,
        dead: true,
        ..RingStats::default()
    };
    assert_eq!(RingStatus::of(small).vram_mb, 3);
    assert!(RingStatus::of(small).dead);
    assert_eq!(RingStatus::of(RingStats::default()).vram_mb, 0);
}

#[test]
fn the_table_lists_the_pipelines_that_decode_on_the_gpu_or_hold_a_producer() {
    let table = pipelines();
    let (decoding, holding, neither) = (27_901, 27_902, 27_903);
    table.set_gpu_decoding(decoding, true);
    let producer = Arc::new(Fake {
        stats: RingStats {
            slots: 2,
            vram_bytes: 6_220_800,
            ..RingStats::default()
        },
        ..Fake::default()
    });
    table.set_producer(holding, Some(&producer));
    table.set_gpu_decoding(neither, false);
    let mine: Vec<_> = table
        .status()
        .into_iter()
        .filter(|row| (27_901..=27_903).contains(&row.playlist_id))
        .collect();
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert_eq!(
        (mine[0].playlist_id, mine[0].gpu_decoding),
        (decoding, true)
    );
    assert_eq!(mine[0].ring, None);
    assert_eq!(
        (mine[1].playlist_id, mine[1].gpu_decoding),
        (holding, false)
    );
    assert_eq!(mine[1].ring.map(|r| (r.slots, r.vram_mb)), Some((2, 6)));
    assert!(table.gpu_decoding(decoding));
    assert!(!table.gpu_decoding(holding));
    assert!(!table.gpu_decoding(27_999), "no row");
    drop(producer);
    assert!(
        table.status().iter().all(|row| row.playlist_id != holding),
        "a producer let go leaves no row behind"
    );
    table.forget(decoding);
    assert!(!table.gpu_decoding(decoding), "forgotten");
    table.set_producer::<Fake>(holding, None);
    table.forget(holding);
    table.forget(neither);
}

#[test]
fn a_songs_path_is_written_to_the_table() {
    let id = 27_904;
    note_song_path(id, true, true, Some("AV01"), (3840, 2160), None);
    assert!(pipelines().gpu_decoding(id));
    note_song_path(id, false, false, Some("AV01"), (3840, 2160), None);
    assert!(!pipelines().gpu_decoding(id), "a fall back mid-song");
    pipelines().forget(id);
}

#[test]
fn a_pipeline_opens_its_songs_in_the_base_mode_until_the_precondition_holds() {
    let setting = HwDecodeSetting::default();
    setting.set(true);
    let mut gpu = PipelineGpu::new(27_905);
    let t0 = Instant::now();
    let never_made = || -> Result<sp_gpu::ProducerGpu, String> { panic!("no producer when off") };
    assert_eq!(
        gpu.decode_mode_with(&setting, t0, never_made),
        DecodeMode::Hardware,
        "video_decode=software: video_hw_decode's mode, no producer asked for"
    );
    setting.set_video_decode(VideoDecode::Gpu);
    assert_eq!(
        gpu.decode_mode_with(&setting, t0, never_made),
        DecodeMode::Hardware,
        "blocked while NDI comes from the canvas"
    );
    setting.set_ndi_source(NdiSource::Gpu);
    let mut asked = 0;
    let mode = gpu.decode_mode_with(&setting, t0, || {
        asked += 1;
        Err("no adapter".to_string())
    });
    assert_eq!(asked, 1, "on: the producer asked for");
    assert_eq!(mode, DecodeMode::Hardware, "a failed build: the base mode");
    assert_eq!(
        gpu.decode_mode_with(&setting, t0 + Duration::from_secs(29), never_made),
        DecodeMode::Hardware,
        "the backoff: no producer asked for"
    );
    pipelines().set_gpu_decoding(27_905, true);
    assert!(
        pipelines()
            .status()
            .iter()
            .any(|row| row.playlist_id == 27_905)
    );
    drop(gpu);
    assert!(
        pipelines()
            .status()
            .iter()
            .all(|row| row.playlist_id != 27_905),
        "a pipeline that ended leaves no row"
    );
}

/// Review round 1: when `gpu` stops taking effect (switched off, NDI back
/// on the canvas) the producers are retired; any other change retires none.
#[test]
fn the_precondition_retires_only_when_gpu_stops_taking_effect() {
    let blocked = Resident::Blocked(BLOCKED_NDI_CPU);
    assert!(retires(Resident::On, Resident::Off));
    assert!(retires(Resident::On, blocked));
    for (before, after) in [
        (Resident::On, Resident::On),
        (Resident::Off, Resident::On),
        (blocked, Resident::On),
        (Resident::Off, blocked),
        (blocked, Resident::Off),
        (Resident::Off, Resident::Off),
    ] {
        assert!(!retires(before, after), "{before:?} -> {after:?}");
    }
}

/// Review round 1: a change that stops `gpu` retires every LIVE producer
/// of the table (a song playing on one reads its pictures back from its
/// next one); one whose pipeline let it go is not counted.
#[test]
fn a_change_that_stops_gpu_retires_every_live_producer() {
    let table = Pipelines::empty();
    let (a, b) = (Arc::new(Fake::default()), Arc::new(Fake::default()));
    table.set_producer(1, Some(&a));
    table.set_producer(2, Some(&b));
    let gone = Arc::new(Fake::default());
    table.set_producer(3, Some(&gone));
    drop(gone);
    table.set_gpu_decoding(4, true);
    let blocked = Resident::Blocked(BLOCKED_NDI_CPU);
    assert_eq!(precondition_changed(&table, Resident::Off, Resident::On), 0);
    assert!(
        !a.retired() && !b.retired(),
        "a change into gpu retires nothing"
    );
    assert_eq!(precondition_changed(&table, Resident::On, blocked), 2);
    assert!(a.retired() && b.retired());
    let fresh = Arc::new(Fake::default());
    table.set_producer(1, Some(&fresh));
    assert_eq!(precondition_changed(&table, blocked, Resident::Off), 0);
    assert!(!fresh.retired(), "no change out of gpu: nothing retired");
    assert_eq!(
        precondition_changed(&table, Resident::On, Resident::Off),
        1,
        "b was retired already: only the fresh one counts"
    );
    assert!(fresh.retired());
}

/// Review round 5: a producer retired already (still held by its lease
/// until the next open) is not retired, nor counted, again.
#[test]
fn a_producer_retired_already_is_not_counted_again() {
    let table = Pipelines::empty();
    let (a, b) = (Arc::new(Fake::default()), Arc::new(Fake::default()));
    table.set_producer(1, Some(&a));
    table.set_producer(2, Some(&b));
    assert_eq!(table.retire_producers(), 2);
    assert!(a.retired() && b.retired());
    assert_eq!(table.retire_producers(), 0, "both retired already");
    assert_eq!(
        precondition_changed(&table, Resident::On, Resident::Off),
        0,
        "a later switch-off retires none of them"
    );
}

/// Review round 2: a retired producer still held at the next open (`gpu`
/// took effect again before a song opened) is replaced at once: its device
/// is healthy, so no backoff and no "lost" WARN.
#[test]
fn a_retired_producer_is_replaced_at_the_next_open_at_once() {
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    let Leased::Made(old) = lease.take(t0, made) else {
        panic!("made");
    };
    old.retire();
    let next = lease.take(t0 + Duration::from_secs(1), made);
    let Leased::Made(new) = &next else {
        panic!("a new producer at once: {next:?}");
    };
    assert!(!Arc::ptr_eq(&old, new), "another producer");
    let later = lease.take(t0 + Duration::from_secs(2), never);
    assert!(
        matches!(later, Leased::Kept(_)),
        "the new one kept: {later:?}"
    );
}

/// Review round 3: a retired producer whose device was lost meanwhile is
/// a dead one: `Lost` and the backoff, never a replacement at once.
#[test]
fn a_retired_producer_that_died_is_lost_not_replaced() {
    let t0 = Instant::now();
    let mut lease = Lease::<Fake>::default();
    let Leased::Made(producer) = lease.take(t0, made) else {
        panic!("made");
    };
    producer.retire();
    producer.lost.store(true, Ordering::Relaxed);
    let next = lease.take(t0 + Duration::from_secs(1), never);
    assert!(matches!(next, Leased::Lost), "{next:?}");
    let waiting = lease.take(t0 + Duration::from_secs(2), never);
    assert!(
        matches!(waiting, Leased::Waiting { .. }),
        "the backoff: {waiting:?}"
    );
}

/// Review round 4: the settings task stops `gpu` while a producer is
/// being made. Its retire scan finds no producer yet (the row is written
/// after the build), so the open asks the precondition again once the row
/// holds it, and retires it: the song opens in the base mode, never
/// GPU-resident with NDI on the canvas.
#[test]
fn a_producer_made_while_gpu_stops_taking_effect_is_retired_at_its_open() {
    let table = Pipelines::empty();
    let setting = HwDecodeSetting::default();
    setting.set_video_decode(VideoDecode::Gpu);
    setting.set_ndi_source(NdiSource::Gpu);
    assert_eq!(setting.resident(), Resident::On);
    let mut lease = Lease::<Fake>::default();
    let make = || {
        // The settings task, while the device is made: NDI back on the
        // canvas, and its scan retires nothing.
        setting.set_ndi_source(NdiSource::Cpu);
        assert_eq!(
            precondition_changed(&table, Resident::On, setting.resident()),
            0,
            "the table holds no producer yet"
        );
        made()
    };
    let resident = || {
        // Review round 5: the second read comes AFTER the row is written
        // (the order the race's fix rests on; no mutant reorders it).
        assert_eq!(table.status().len(), 1, "the row before the second read");
        setting.resident()
    };
    let leased = take_for_open(&mut lease, &table, 1, Instant::now(), make, resident);
    assert!(matches!(leased, Leased::Retired), "{leased:?}");
    assert!(
        leased.producer().is_none(),
        "the song opens in the base mode"
    );
    let held = lease.held().expect("the lease holds it, retired");
    assert!(held.retired(), "retired at its open");
    let rows = table.status();
    assert_eq!(rows.len(), 1, "its row holds it: {rows:?}");
}

/// Review round 4: while `gpu` keeps taking effect the producer taken is
/// the song's, and its row holds it, so a later change's scan retires it.
#[test]
fn a_producer_taken_while_gpu_holds_is_the_songs_and_a_later_scan_retires_it() {
    let table = Pipelines::empty();
    let mut lease = Lease::<Fake>::default();
    let t0 = Instant::now();
    let leased = take_for_open(&mut lease, &table, 1, t0, made, || Resident::On);
    let Leased::Made(producer) = &leased else {
        panic!("made: {leased:?}");
    };
    assert!(!producer.retired(), "the song's producer");
    let kept = take_for_open(&mut lease, &table, 1, t0, never, || Resident::On);
    assert!(matches!(kept, Leased::Kept(_)), "{kept:?}");
    assert_eq!(
        precondition_changed(&table, Resident::On, Resident::Off),
        1,
        "the row held it"
    );
    assert!(producer.retired());
}

/// Review round 1: a song's end leaves its row decoding nothing.
#[test]
fn a_songs_end_clears_its_row() {
    let id = 27_910;
    pipelines().set_gpu_decoding(id, true);
    assert!(pipelines().gpu_decoding(id));
    song_ended(id);
    assert!(!pipelines().gpu_decoding(id), "the song is over");
    pipelines().forget(id);
}
