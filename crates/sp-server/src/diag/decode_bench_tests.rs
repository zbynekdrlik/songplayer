//! Tests of the decode bench's cross-platform part (#223 S0): the request's
//! checks, the one-run gate, the timing loop over a scripted stream and a
//! scripted clock (no wall-time thresholds), the stats and D2's gate.
//! Exact values come from a scratch model of the arithmetic.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sp_decoder::{DecodedVideoFrame, DecoderError, MediaStream, PixelFormat, VideoStream};

use super::{
    BenchEnd, BenchFileError, BenchOutcome, BenchReport, BenchRun, Budget, DecodeBench, DecodeUs,
    MAX_NAME_BYTES, PictureLayout, StreamFacts, bench_file_name, bench_seconds, is_device_name,
    measure, run_on_decode_thread,
};

// ---------------------------------------------------------------------------
// The request's checks and the gate
// ---------------------------------------------------------------------------

#[test]
fn bare_file_names_are_accepted() {
    for name in [
        "av1_4k.mp4",
        "vp9 4k.webm",
        "ťažký.mp4",
        ".hidden.mp4",
        "a.b.c",
        "COM10.mp4",
        "console.mp4",
        "null.mp4",
        "lpt.mp4",
    ] {
        assert_eq!(bench_file_name(name), Ok(name), "{name:?}");
        // What passes is one plain path component on this platform.
        let parts: Vec<_> = Path::new(name).components().collect();
        assert!(
            matches!(parts.as_slice(), [std::path::Component::Normal(_)]),
            "{name:?}: {parts:?}"
        );
    }
    let longest = "a".repeat(MAX_NAME_BYTES);
    assert_eq!(bench_file_name(&longest), Ok(longest.as_str()));
}

#[test]
fn names_that_could_leave_the_sample_dir_are_refused() {
    let too_long = "a".repeat(MAX_NAME_BYTES + 1);
    for name in [
        "",
        ".",
        "..",
        "../x.mp4",
        "x..y.mp4",
        "a/b.mp4",
        "/abs.mp4",
        "a\\b.mp4",
        "\\\\server\\share",
        "C:x.mp4",
        "C:\\x.mp4",
        "x.mp4:stream",
        "a\u{1}b.mp4",
        "a\nb.mp4",
        too_long.as_str(),
        // Windows strips a trailing dot or space: another file's name.
        "x.mp4.",
        "x.mp4 ",
        // Windows devices, not files.
        "nul",
        "NUL.mp4",
        "com1.mp4",
        "Lpt9",
        "conin$",
        "aux .mp4",
    ] {
        assert!(bench_file_name(name).is_err(), "{name:?} must be refused");
    }
}

#[test]
fn windows_device_names_are_recognized() {
    for name in [
        "CON",
        "prn",
        "Aux",
        "NUL",
        "nul.mp4",
        "nul .mp4",
        "CONIN$",
        "conout$.txt",
        "COM0",
        "COM1",
        "com9.webm",
        "LPT1",
        "lpt9.mp4",
        "COM\u{b9}",
        "LPT\u{b3}.mp4",
    ] {
        assert!(is_device_name(name), "{name:?}");
    }
    for name in [
        "COM10.mp4",
        "COM",
        "LPT",
        "COMX",
        "console.mp4",
        "null.mp4",
        "nul_x.mp4",
        "av1_4k.mp4",
        "x.nul",
        ".nul",
    ] {
        assert!(!is_device_name(name), "{name:?}");
    }
}

#[test]
fn seconds_run_from_one_to_fifteen() {
    assert!(bench_seconds(0).is_err());
    assert_eq!(bench_seconds(1), Ok(Duration::from_secs(1)));
    assert_eq!(bench_seconds(15), Ok(Duration::from_secs(15)));
    assert!(bench_seconds(16).is_err());
}

#[test]
fn the_samples_sit_beside_the_db() {
    let data = Path::new("data");
    let bench = DecodeBench::beside_db(&data.join("songplayer.db"));
    assert_eq!(bench.dir(), data.join("bench").as_path());
}

#[test]
fn resolve_finds_a_sample_and_names_a_missing_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("bench");
    std::fs::create_dir_all(dir.join("a_dir.mp4")).unwrap();
    std::fs::write(dir.join("av1.mp4"), b"x").unwrap();
    std::fs::write(tmp.path().join("outside.mp4"), b"x").unwrap();
    let bench = DecodeBench::new(dir.clone());

    assert_eq!(bench.resolve("av1.mp4"), Ok(dir.join("av1.mp4")));
    assert_eq!(
        bench.resolve("vp9.mp4"),
        Err(BenchFileError::Missing(dir.join("vp9.mp4")))
    );
    // A directory is not a sample.
    assert_eq!(
        bench.resolve("a_dir.mp4"),
        Err(BenchFileError::Missing(dir.join("a_dir.mp4")))
    );
    // A name that leaves the dir is refused even when its target exists.
    assert!(matches!(
        bench.resolve("../outside.mp4"),
        Err(BenchFileError::BadName(_))
    ));
}

#[test]
fn one_run_at_a_time_and_a_dropped_slot_frees_the_bench() {
    let bench = DecodeBench::new(PathBuf::from("bench"));
    let slot = bench.try_start().expect("a free bench is taken");
    assert!(
        bench.try_start().is_none(),
        "a second run while one holds the bench"
    );
    drop(slot);
    let again = bench
        .try_start()
        .expect("the bench is free once the slot is dropped");
    // The run's thread drops the slot, not the caller's.
    std::thread::spawn(move || drop(again)).join().unwrap();
    assert!(bench.try_start().is_some());
}

// ---------------------------------------------------------------------------
// The timing loop
// ---------------------------------------------------------------------------

enum Step {
    Picture(PictureLayout),
    Fail(&'static str),
}

/// A stream that hands over its steps in order, then ends. It counts the
/// `next_frame` calls it answered.
struct Script {
    steps: VecDeque<Step>,
    data_capacity: usize,
    calls: usize,
}

impl Script {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into(),
            data_capacity: 0,
            calls: 0,
        }
    }
}

impl MediaStream for Script {
    fn duration_ms(&self) -> u64 {
        0
    }

    fn seek(&mut self, _position_ms: u64) -> Result<(), DecoderError> {
        Ok(())
    }
}

impl VideoStream for Script {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.calls += 1;
        match self.steps.pop_front() {
            None => Ok(None),
            Some(Step::Fail(why)) => Err(DecoderError::Decode(why.to_string())),
            Some(Step::Picture(layout)) => Ok(Some(DecodedVideoFrame {
                data: Vec::with_capacity(self.data_capacity),
                width: layout.width,
                height: layout.height,
                stride: layout.stride,
                timestamp_ms: 0,
                pixel_format: PixelFormat::Nv12,
            })),
        }
    }

    fn width(&self) -> u32 {
        0
    }

    fn height(&self) -> u32 {
        0
    }

    fn frame_rate(&self) -> (u32, u32) {
        (30, 1)
    }
}

/// The run's clock: `readings_us` in order, one per call. A loop that reads
/// it more often than scripted panics, which fails the test.
fn clock(readings_us: &[u64]) -> impl FnMut() -> Duration + '_ {
    let mut next = readings_us.iter();
    move || {
        let us = next
            .next()
            .expect("the run read its clock more often than scripted");
        Duration::from_micros(*us)
    }
}

const FHD: PictureLayout = PictureLayout {
    width: 1920,
    height: 1080,
    stride: 1920,
};
const HD: PictureLayout = PictureLayout {
    width: 1280,
    height: 720,
    stride: 1280,
};

#[test]
fn every_picture_is_timed_until_the_stream_ends() {
    let mut stream = Script::new(vec![
        Step::Picture(FHD),
        Step::Picture(HD),
        Step::Picture(HD),
    ]);
    // before / after of each call; the 4th call answers the end.
    let readings = [0, 4_000, 4_000, 9_000, 9_500, 12_500, 13_000, 13_100];
    let run = measure(&mut stream, Duration::from_secs(15), clock(&readings));
    assert_eq!(
        run,
        BenchRun {
            decode_us: vec![4_000, 5_000, 3_000],
            first_picture: Some(FHD),
            wall_us: 13_100,
            end: BenchEnd::EndOfStream,
            error: None,
        }
    );
    assert_eq!(stream.calls, 4);
}

#[test]
fn a_decoder_error_ends_the_run_with_the_pictures_so_far() {
    let mut stream = Script::new(vec![
        Step::Picture(HD),
        Step::Picture(FHD),
        Step::Fail("bad bitstream"),
        Step::Picture(FHD),
    ]);
    let readings = [0, 1_000, 1_000, 3_000, 3_000, 3_500];
    let run = measure(&mut stream, Duration::from_secs(15), clock(&readings));
    assert_eq!(
        run,
        BenchRun {
            decode_us: vec![1_000, 2_000],
            first_picture: Some(HD),
            wall_us: 3_500,
            end: BenchEnd::Error,
            error: Some("Decode failure: bad bitstream".to_string()),
        }
    );
    // The failed call is not a sample, and nothing is decoded after it.
    assert_eq!(stream.calls, 3);
}

#[test]
fn the_run_stops_once_its_clock_reaches_the_bound() {
    let pictures = || (0..10).map(|_| Step::Picture(FHD)).collect::<Vec<_>>();
    let bound = Duration::from_millis(10);

    // A call that starts before the bound is timed; at the bound the run stops.
    let mut stream = Script::new(pictures());
    let readings = [0, 4_000, 4_000, 8_000, 8_000, 12_000, 12_000];
    let run = measure(&mut stream, bound, clock(&readings));
    assert_eq!(run.decode_us, vec![4_000, 4_000, 4_000]);
    assert_eq!(run.wall_us, 12_000);
    assert_eq!(run.end, BenchEnd::TimeLimit);
    assert_eq!(run.error, None);
    assert_eq!(stream.calls, 3);

    // Reaching the bound exactly stops the run too.
    let mut stream = Script::new(pictures());
    let readings = [0, 5_000, 5_000, 10_000, 10_000];
    let run = measure(&mut stream, bound, clock(&readings));
    assert_eq!(run.decode_us, vec![5_000, 5_000]);
    assert_eq!(run.wall_us, 10_000);
    assert_eq!(run.end, BenchEnd::TimeLimit);
    assert_eq!(stream.calls, 2);
}

#[test]
fn a_decoded_picture_goes_back_to_the_frame_pool() {
    // A capacity no other test allocates: this size class is this test's.
    const CAP: usize = 1_500_017;
    let before = sp_decoder::frame_pool::pool_len(CAP);
    let mut stream = Script::new(vec![Step::Picture(FHD), Step::Picture(FHD)]);
    stream.data_capacity = CAP;
    let run = measure(
        &mut stream,
        Duration::from_secs(15),
        clock(&[0, 1, 1, 2, 2, 3]),
    );
    assert_eq!(run.decode_us.len(), 2);
    assert_eq!(sp_decoder::frame_pool::pool_len(CAP), before + 2);
}

// ---------------------------------------------------------------------------
// The stats and D2's gate
// ---------------------------------------------------------------------------

#[test]
fn decode_cost_stats() {
    assert_eq!(DecodeUs::of(&[]), DecodeUs::default());
    assert_eq!(
        DecodeUs::of(&[4_000, 5_000, 3_000]),
        DecodeUs {
            mean: 4_000,
            p50: 4_000,
            p99: 5_000,
            max: 5_000,
        }
    );
    // The mean is floored.
    assert_eq!(DecodeUs::of(&[1, 2]).mean, 1);
    // p99 is the nearest rank, not the max: 101 samples, one outlier.
    let mut samples: Vec<u64> = (1..=100).collect();
    samples.push(10_000);
    assert_eq!(
        DecodeUs::of(&samples),
        DecodeUs {
            mean: 149,
            p50: 51,
            p99: 100,
            max: 10_000,
        }
    );
}

fn gate(period_us: u64, over: bool) -> Option<Budget> {
    Some(Budget {
        frame_period_us: period_us,
        mean_over_half_period: over,
    })
}

#[test]
fn the_frame_period_is_one_over_the_source_rate_rounded() {
    assert_eq!(Budget::check((30_000, 1_001), &[1]), gate(33_367, false));
    assert_eq!(Budget::check((30, 1), &[1]), gate(33_333, false));
    assert_eq!(Budget::check((24_000, 1_001), &[1]), gate(41_708, false));
    assert_eq!(Budget::check((50, 1), &[1]), gate(20_000, false));
}

#[test]
fn the_gate_fails_only_a_mean_over_half_the_period() {
    // 50 fps: half of 1/f is exactly 10 000 µs.
    assert_eq!(Budget::check((50, 1), &[10_000]), gate(20_000, false));
    assert_eq!(Budget::check((50, 1), &[10_001]), gate(20_000, true));
    // 29.97 fps: half of 1/f is 16 683.33 µs.
    assert_eq!(
        Budget::check((30_000, 1_001), &[16_683]),
        gate(33_367, false)
    );
    assert_eq!(
        Budget::check((30_000, 1_001), &[16_684]),
        gate(33_367, true)
    );
    // On the samples' sum: two at 16 683 pass, a mean of 16 683.5 fails.
    assert_eq!(
        Budget::check((30_000, 1_001), &[16_683, 16_683]),
        gate(33_367, false)
    );
    assert_eq!(
        Budget::check((30_000, 1_001), &[16_000, 17_367]),
        gate(33_367, true)
    );
}

#[test]
fn no_gate_without_a_rate_or_a_picture() {
    assert_eq!(Budget::check((0, 1), &[1_000]), None);
    assert_eq!(Budget::check((30, 0), &[1_000]), None);
    assert_eq!(Budget::check((30, 1), &[]), None);
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

fn facts_4k() -> StreamFacts {
    StreamFacts {
        width: 3840,
        height: 2160,
        frame_rate: Some((30_000, 1_001)),
        codec: Some("AV01".to_string()),
    }
}

/// A 4K picture as MF may hand it over: its height padded to 16 rows.
const PADDED_4K: PictureLayout = PictureLayout {
    width: 3840,
    height: 2176,
    stride: 3840,
};

/// Two pictures at a mean of 16 683 µs, just inside 29.97 fps's half period.
fn clean_report() -> BenchReport {
    let run = BenchRun {
        decode_us: vec![17_000, 16_366],
        first_picture: Some(PADDED_4K),
        wall_us: 15_000_999,
        end: BenchEnd::TimeLimit,
        error: None,
    };
    BenchReport::from_run("av1_4k.mp4", facts_4k(), run, 120, Some(0))
}

#[test]
fn a_clean_run_reports_its_pictures_and_the_gate() {
    let report = clean_report();
    assert_eq!(report.file, "av1_4k.mp4");
    // The picture's layout, not the stream's.
    assert_eq!(
        (report.width, report.height, report.stride),
        (3840, 2176, Some(3840))
    );
    assert_eq!(report.codec.as_deref(), Some("AV01"));
    let fps = report.source_fps.expect("a known rate");
    assert!((fps - 29.970).abs() < 0.001, "{fps}");
    assert_eq!(report.frames, 2);
    assert_eq!(report.first_us, Some(17_000));
    assert_eq!(report.wall_ms, 15_000);
    assert_eq!(report.open_ms, 120);
    assert_eq!(report.ended, BenchEnd::TimeLimit);
    assert_eq!(report.error, None);
    assert_eq!(report.thread_priority, Some(0));
    assert_eq!(
        report.decode_us,
        DecodeUs {
            mean: 16_683,
            p50: 16_366,
            p99: 17_000,
            max: 17_000,
        }
    );
    assert_eq!(report.budget, gate(33_367, false));
    assert!(!report.failed());
}

#[test]
fn a_run_with_no_picture_reports_the_stream_size_and_no_gate() {
    let run = BenchRun {
        decode_us: vec![],
        first_picture: None,
        wall_us: 2_500,
        end: BenchEnd::EndOfStream,
        error: None,
    };
    let report = BenchReport::from_run("empty.mp4", facts_4k(), run, 7, None);
    assert_eq!(
        (report.width, report.height, report.stride),
        (3840, 2160, None)
    );
    assert_eq!(report.frames, 0);
    assert_eq!(report.first_us, None);
    assert_eq!(report.wall_ms, 2);
    assert_eq!(report.decode_us, DecodeUs::default());
    assert_eq!(report.budget, None);
    assert!(!report.failed());
}

#[test]
fn an_unknown_source_rate_has_no_fps_and_no_gate() {
    // MF reported no rate (the reader plays at a 29.97 guess), or a zero.
    for frame_rate in [None, Some((0, 1)), Some((30, 0))] {
        let facts = StreamFacts {
            frame_rate,
            ..facts_4k()
        };
        let run = BenchRun {
            decode_us: vec![1_000],
            first_picture: Some(PADDED_4K),
            wall_us: 1_000,
            end: BenchEnd::EndOfStream,
            error: None,
        };
        let report = BenchReport::from_run("x.mp4", facts, run, 1, None);
        assert_eq!(report.source_fps, None, "{frame_rate:?}");
        assert_eq!(report.budget, None, "{frame_rate:?}");
    }
}

#[test]
fn a_decoder_error_is_a_500_with_the_pictures_so_far() {
    let run = BenchRun {
        decode_us: vec![1_000, 2_000],
        first_picture: Some(PADDED_4K),
        wall_us: 3_500,
        end: BenchEnd::Error,
        error: Some("Decode failure: bad bitstream".to_string()),
    };
    let report = BenchReport::from_run("vp9_4k.mp4", facts_4k(), run, 5, Some(0));
    assert_eq!(report.frames, 2);
    assert_eq!(report.ended, BenchEnd::Error);
    assert_eq!(
        report.error.as_deref(),
        Some("Decode failure: bad bitstream")
    );
    assert!(report.failed());
}

#[test]
fn a_file_that_does_not_open_is_a_500_with_no_picture() {
    let report = BenchReport::open_failed("x.mp4", "open: no video".to_string(), 3, Some(0));
    assert_eq!(report.file, "x.mp4");
    assert_eq!((report.width, report.height, report.frames), (0, 0, 0));
    assert_eq!(report.open_ms, 3);
    assert_eq!(report.ended, BenchEnd::Error);
    assert_eq!(report.error.as_deref(), Some("open: no video"));
    assert_eq!(report.thread_priority, Some(0));
    assert_eq!(report.first_us, None);
    assert_eq!(report.budget, None);
    assert!(report.failed());
}

#[test]
fn the_report_serializes_to_the_documented_json() {
    let json = serde_json::to_value(clean_report()).unwrap();
    assert_eq!(json["file"], "av1_4k.mp4");
    assert_eq!(json["width"], 3840);
    assert_eq!(json["height"], 2176);
    assert_eq!(json["stride"], 3840);
    assert_eq!(json["codec"], "AV01");
    assert_eq!(json["frames"], 2);
    assert_eq!(json["first_us"], 17_000);
    assert_eq!(json["wall_ms"], 15_000);
    assert_eq!(json["ended"], "time_limit");
    assert!(json["error"].is_null());
    assert_eq!(json["thread_priority"], 0);
    assert_eq!(json["decode_us"]["mean"], 16_683);
    assert_eq!(json["decode_us"]["p99"], 17_000);
    assert_eq!(json["budget"]["frame_period_us"], 33_367);
    assert_eq!(
        json["budget"]["mean_over_half_period"],
        serde_json::Value::Bool(false)
    );
    assert_eq!(
        serde_json::to_value(BenchEnd::EndOfStream).unwrap(),
        "end_of_stream"
    );
    assert_eq!(serde_json::to_value(BenchEnd::Error).unwrap(), "error");
}

#[test]
fn the_end_log_line_names_the_stats_and_the_gate() {
    assert_eq!(
        clean_report().summary(),
        "file=av1_4k.mp4 3840x2176 codec=AV01 frames=2 wall_ms=15000 ended=TimeLimit \
         mean_us=16683 p50_us=16366 p99_us=17000 max_us=17000 \
         frame_period_us=33367 mean_over_half_period=false"
    );
    let failed = BenchReport::open_failed("x.mp4", "open: no video".to_string(), 3, None);
    assert_eq!(
        failed.summary(),
        "file=x.mp4 0x0 codec=? frames=0 wall_ms=0 ended=Error \
         mean_us=0 p50_us=0 p99_us=0 max_us=0 budget=unknown error=open: no video"
    );
}

// ---------------------------------------------------------------------------
// The run's thread
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_answers_from_the_decode_thread_with_the_bench_already_free() {
    let bench = DecodeBench::new(PathBuf::from("bench"));
    let slot = bench.try_start().expect("a free bench");
    let outcome = run_on_decode_thread(slot, || {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        BenchReport::open_failed(&thread, "x".to_string(), 0, None)
    })
    .await;
    match outcome {
        // The body ran on the decode thread, which carries the bench's name.
        BenchOutcome::Report(report) => assert_eq!(report.file, "decode-bench"),
        other => panic!("expected a report, got {other:?}"),
    }
    // The thread dropped the slot before it sent the report.
    assert!(bench.try_start().is_some());
}

#[tokio::test]
async fn a_body_that_panics_is_a_failed_run_with_the_bench_free() {
    let bench = DecodeBench::new(PathBuf::from("bench"));
    let slot = bench.try_start().expect("a free bench");
    let outcome =
        run_on_decode_thread(slot, || -> BenchReport { panic!("the decoder blew up") }).await;
    match outcome {
        BenchOutcome::Failed(why) => assert!(why.contains("panicked"), "{why}"),
        other => panic!("expected a failed run, got {other:?}"),
    }
    assert!(bench.try_start().is_some());
}
