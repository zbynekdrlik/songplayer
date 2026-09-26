//! #212: the NDI input on the genlock grid, driven over `MockNdiReceiveBackend`
//! into a real `ProgramBus` that has the input (`-1`) on program, so every
//! assertion reads the jobs the `SP-program` sender would get. Received frames
//! are 4×2 UYVY; the input's standby black is 2×4 NV12, so a job's width names
//! whether it carried the source or the standby pair.
//! Wired via `#[cfg(test)] #[path = "ndi_input_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::playback::program_bus::{ProgramBus, ProgramJob, Take};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::VbanClock;
use sp_core::config::PROGRAM_INPUT_ID;
use sp_core::genlock::{
    GENLOCK_GRID_FPS, floor_boundary_100ns, interval_100ns, strict_next_boundary_100ns,
};
use sp_ndi::NdiReceiveBackend;
use sp_ndi::receive::{FOURCC_UYVA, FOURCC_UYVY, FRAME_FORMAT_TYPE_PROGRESSIVE};
use sp_ndi::test_util::{MockNdiReceiveBackend, MockVideoFrame};

/// 2026-09 in 100 ns since the epoch.
const T0: i64 = 17_900_000_000_000_000;
const MS: i64 = 10_000;
const SOURCE: &str = "CG-OBS (manual)";

/// The k-th grid boundary after `floor(T0)` (`b(0)` = `floor(T0)`).
fn b(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

/// A 4×2 UYVY frame (stride 8) stamped `tc`, every byte `fill`.
fn uyvy(tc: i64, n: i32, d: i32, fill: u8) -> MockVideoFrame {
    MockVideoFrame {
        xres: 4,
        yres: 2,
        four_cc: FOURCC_UYVY,
        line_stride: 8,
        frame_rate_n: n,
        frame_rate_d: d,
        frame_format_type: FRAME_FORMAT_TYPE_PROGRESSIVE,
        timecode: tc,
        data: vec![fill; 16],
    }
}

/// `count` frames of an `fps` source, frame `i` stamped `i × 1e7 / fps`.
fn source_frames(count: usize, fps: i32) -> Vec<MockVideoFrame> {
    (0..count)
        .map(|i| uyvy(i as i64 * 10_000_000 / fps as i64, fps, 1, i as u8))
        .collect()
}

/// Planar stereo audio: left = `0.001 × s`, right = `−0.001 × s`.
fn ramp_audio() -> Vec<f32> {
    let n = INPUT_AUDIO_SAMPLES as usize;
    let mut planar: Vec<f32> = (0..n).map(|s| s as f32 * 0.001).collect();
    planar.extend((0..n).map(|s| s as f32 * -0.001));
    planar
}

fn ramp_interleaved() -> Vec<f32> {
    (0..INPUT_AUDIO_SAMPLES as usize)
        .flat_map(|s| [s as f32 * 0.001, s as f32 * -0.001])
        .collect()
}

fn enabled() -> InputSettings {
    InputSettings {
        enabled: true,
        source: SOURCE.to_string(),
    }
}

struct Rig {
    mock: Arc<MockNdiReceiveBackend>,
    shared: Arc<NdiInputShared>,
    bus: Arc<ProgramBus>,
    input: NdiInput,
}

/// An enabled, connected input on program, receiving `frames` on `schedule`.
fn rig(frames: Vec<MockVideoFrame>, schedule: Vec<Option<usize>>) -> Rig {
    let mock = Arc::new(MockNdiReceiveBackend::default());
    mock.set_connections(1);
    mock.set_video_frames(frames);
    mock.set_video_schedule(schedule);
    mock.set_audio(
        ramp_audio(),
        2,
        INPUT_AUDIO_SAMPLES,
        INPUT_AUDIO_SAMPLES * 4,
    );
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(PROGRAM_INPUT_ID);
    let shared = bus.input().clone();
    shared.set_settings(enabled());
    let input = NdiInput::new(
        Some(mock.clone() as Arc<dyn NdiReceiveBackend>),
        shared.clone(),
        2,
        4,
    );
    Rig {
        mock,
        shared,
        bus,
        input,
    }
}

impl Rig {
    /// Service boundaries `b(1)..=b(n)` (audio stamped 2 ms after each) and
    /// return the program jobs, drained after every boundary.
    fn run(&mut self, n: usize) -> Vec<SubmitJob> {
        let mut jobs = Vec::new();
        for k in 1..=n {
            self.input.service(b(k), b(k) + 2 * MS, &self.bus);
            jobs.extend(drain(&self.bus));
        }
        jobs
    }

    fn status(&self) -> NdiInputStatus {
        self.shared.status(&enabled())
    }

    fn calls_matching(&self, prefix: &str) -> usize {
        self.mock
            .calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

/// Every source job the bus has queued for the program sender.
fn drain(bus: &ProgramBus) -> Vec<SubmitJob> {
    let mut out = Vec::new();
    for _ in 0..64 {
        match bus.take_timeout(Duration::ZERO) {
            Take::Job(ProgramJob::Source(job)) => out.push(job),
            Take::Job(ProgramJob::Standby { stamp_100ns }) => {
                panic!("the program filled boundary {stamp_100ns} — the input must own it")
            }
            Take::Idle | Take::Stopped => break,
        }
    }
    out
}

fn assert_one_pair_per_boundary(jobs: &[SubmitJob], n: usize) {
    let stamps: Vec<i64> = jobs.iter().map(|j| j.video_tc_100ns).collect();
    assert_eq!(
        stamps,
        (1..=n).map(b).collect::<Vec<_>>(),
        "one pair per boundary"
    );
    for (k, job) in jobs.iter().enumerate() {
        assert_eq!(
            job.audio_tc_100ns,
            b(k + 1) + 2 * MS,
            "audio stamped at the emit"
        );
        assert_eq!(job.audio.len(), 1, "one audio block per boundary");
        assert_eq!(job.audio[0].data.len(), 3200, "1600 stereo samples");
        assert_eq!(job.audio[0].channels, 2);
        assert_eq!(job.audio[0].sample_rate, 48_000);
    }
}

// --- one pair per boundary for 30 / 25 / 60 fps sources ----------------------

#[test]
fn a_30_fps_source_lands_one_frame_per_boundary() {
    let mut rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    let jobs = rig.run(30);
    assert_one_pair_per_boundary(&jobs, 30);
    for (k, job) in jobs.iter().enumerate() {
        assert_eq!((job.width, job.height, job.stride), (4, 2, 4));
        // Frame k is all-`k` UYVY: NV12 luma `k` and chroma `k`.
        assert_eq!(
            &job.video[..],
            &[k as u8; 12][..],
            "boundary {k} carries frame {k}"
        );
        assert_eq!(job.audio[0].data, ramp_interleaved());
    }
    let st = rig.status();
    assert_eq!(st.frames_received, 30);
    assert_eq!(st.video_repeats, 0);
    assert_eq!(st.video_drops, 0);
    assert_eq!(st.boundaries, 30);
    assert_eq!(st.no_source_boundaries, 0);
}

#[test]
fn a_25_fps_source_is_repeated_onto_the_grid_without_reconverting() {
    // FrameSync returns frame floor(k × 25 / 30) at boundary k.
    let schedule: Vec<Option<usize>> = (0..30).map(|k| Some(k * 25 / 30)).collect();
    let mut rig = rig(source_frames(25, 25), schedule.clone());
    let jobs = rig.run(30);
    assert_one_pair_per_boundary(&jobs, 30);
    for k in 1..30 {
        let same = schedule[k] == schedule[k - 1];
        assert_eq!(
            jobs[k].video.ptr_eq(&jobs[k - 1].video),
            same,
            "boundary {k}: a repeat re-offers the same converted picture"
        );
        assert_eq!(jobs[k].video[0], schedule[k].unwrap() as u8);
    }
    let st = rig.status();
    assert_eq!(st.frames_received, 25);
    assert_eq!(st.video_repeats, 5);
    assert_eq!(st.video_drops, 0);
    assert_eq!(st.boundaries, 30);
}

#[test]
fn a_60_fps_source_is_decimated_onto_the_grid_and_the_drops_are_counted() {
    let mut rig = rig(
        source_frames(60, 60),
        (0..30).map(|k| Some(2 * k)).collect(),
    );
    let jobs = rig.run(30);
    assert_one_pair_per_boundary(&jobs, 30);
    for (k, job) in jobs.iter().enumerate() {
        assert_eq!(job.video[0], (2 * k) as u8);
    }
    let st = rig.status();
    assert_eq!(st.frames_received, 30);
    assert_eq!(st.video_repeats, 0);
    assert_eq!(
        st.video_drops, 29,
        "one source frame skipped between every two"
    );
    assert_eq!(st.frame_rate.as_deref(), Some("60/1"));
}

// --- standby -----------------------------------------------------------------

fn assert_standby(job: &SubmitJob) {
    assert_eq!((job.width, job.height, job.stride), (2, 4, 2));
    assert_eq!(
        &job.video[..],
        &[16, 16, 16, 16, 16, 16, 16, 16, 128, 128, 128, 128][..],
        "NV12 black: 2×4 luma 16, then 2×2 chroma 128"
    );
    assert_eq!(job.audio[0].data, vec![0.0; 3200], "one silent block");
}

#[test]
fn a_disconnected_source_gives_standby_black_and_silence_on_every_boundary() {
    let mut rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    rig.mock.set_connections(0);
    let jobs = rig.run(30);
    assert_one_pair_per_boundary(&jobs, 30);
    jobs.iter().for_each(assert_standby);
    assert!(
        jobs[1].video.ptr_eq(&jobs[0].video),
        "the standby black is one shared picture"
    );
    let st = rig.status();
    assert_eq!(st.no_source_boundaries, 30);
    assert_eq!(st.frames_received, 0);
    assert!(!st.connected);
    assert!(!rig.shared.is_connected());
    assert_eq!(rig.calls_matching("framesync_capture_video"), 0);
}

#[test]
fn a_source_that_drops_out_mid_stream_turns_to_standby_and_back() {
    let mut rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    let mut jobs = rig.run(3);
    assert!(rig.status().connected);
    rig.mock.set_connections(0);
    for k in 4..=6 {
        rig.input.service(b(k), b(k) + 2 * MS, &rig.bus);
        jobs.extend(drain(&rig.bus));
    }
    assert!(!rig.status().connected);
    rig.mock.set_connections(1);
    for k in 7..=8 {
        rig.input.service(b(k), b(k) + 2 * MS, &rig.bus);
        jobs.extend(drain(&rig.bus));
    }
    assert!(rig.status().connected);
    assert_one_pair_per_boundary(&jobs, 8);
    let sizes: Vec<u32> = jobs.iter().map(|j| j.width).collect(); // 4 = source, 2 = standby
    assert_eq!(sizes, vec![4, 4, 4, 2, 2, 2, 4, 4]);
    assert_eq!(rig.status().no_source_boundaries, 3);
}

#[test]
fn no_video_yet_is_standby_while_the_audio_is_still_pulled() {
    let mut rig = rig(Vec::new(), Vec::new()); // the all-zero frame
    let jobs = rig.run(3);
    jobs.iter().for_each(assert_standby);
    assert_eq!(rig.status().no_source_boundaries, 3);
    assert_eq!(rig.calls_matching("framesync_capture_audio"), 3);
    assert_eq!(rig.mock.outstanding_video(), 0, "every capture freed");
    assert_eq!(rig.mock.outstanding_audio(), 0);
}

#[test]
fn an_unsupported_fourcc_is_standby_and_counted_once_logged_as_its_format() {
    let mut bgra = uyvy(0, 30, 1, 7);
    bgra.four_cc = u32::from_le_bytes(*b"BGRA");
    let mut rig = rig(vec![bgra], vec![Some(0)]);
    let jobs = rig.run(3);
    jobs.iter().for_each(assert_standby);
    let st = rig.status();
    assert_eq!(st.unsupported_boundaries, 3);
    assert_eq!(st.frames_received, 0);
    assert_eq!(st.format.as_deref(), Some("BGRA"));
    assert_eq!(st.last_frame_size.as_deref(), Some("4x2"));
}

#[test]
fn a_uyva_frame_converts_its_uyvy_plane() {
    let mut f = uyvy(0, 30, 1, 9);
    f.four_cc = FOURCC_UYVA;
    f.data.extend([255u8; 8]); // the alpha plane after the UYVY plane
    let mut rig = rig(vec![f], vec![Some(0)]);
    let jobs = rig.run(1);
    assert_eq!(&jobs[0].video[..], &[9u8; 12][..]);
}

#[test]
fn a_stride_too_short_for_the_width_is_standby() {
    let mut f = uyvy(0, 30, 1, 9);
    f.line_stride = 7; // under 2 × 4
    let mut rig = rig(vec![f], vec![Some(0)]);
    rig.run(2).iter().for_each(assert_standby);
    let st = rig.status();
    assert_eq!(st.unsupported_boundaries, 2);
    assert_eq!(
        st.frames_received, 0,
        "a frame that cannot be converted is not received"
    );
    // A stride of exactly 2 × width is the tightest valid one.
    let mut tight = uyvy(0, 30, 1, 5);
    tight.line_stride = 8;
    let mut tight_rig = self::rig(vec![tight], vec![Some(0)]);
    assert_eq!(&tight_rig.run(1)[0].video[..], &[5u8; 12][..]);
}

#[test]
fn without_an_ndi_sdk_the_enabled_input_still_owns_its_boundaries() {
    let bus = ProgramBus::new();
    bus.select_initial(PROGRAM_INPUT_ID);
    bus.input().set_settings(enabled());
    let mut input = NdiInput::new(None, bus.input().clone(), 2, 4);
    for k in 1..=3 {
        input.service(b(k), b(k), &bus);
    }
    let jobs = drain(&bus);
    assert_eq!(jobs.len(), 3);
    jobs.iter().for_each(assert_standby);
}

// --- the audio block --------------------------------------------------------

#[test]
fn capture_audio_asks_for_48000_2_1600_and_the_samples_land_unchanged() {
    let mut rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    rig.mock.set_audio_queue_depth(4321);
    let jobs = rig.run(3);
    for job in &jobs {
        assert_eq!(job.audio[0].data, ramp_interleaved(), "bit-exact samples");
    }
    let asks: Vec<String> = rig
        .mock
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("framesync_capture_audio"))
        .collect();
    assert_eq!(asks, vec!["framesync_capture_audio(2,48000,2,1600)"; 3]);
    assert_eq!(rig.status().audio_queue_depth, 4321);
}

// --- program candidacy + settings --------------------------------------------

#[test]
fn off_program_the_input_captures_and_counts_but_offers_and_converts_nothing() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    let bus = Arc::new(ProgramBus::new()); // nothing on program
    rig.bus = bus;
    assert!(rig.run(2).is_empty(), "not a candidate: no job");
    let st = rig.status();
    assert_eq!(
        (st.boundaries, st.frames_received, st.video_repeats),
        (2, 1, 1)
    );
    assert_eq!(rig.calls_matching("framesync_capture_video"), 2);
    // Cut to the input: the next boundary converts the frame it still holds.
    rig.bus.select_initial(PROGRAM_INPUT_ID);
    rig.input.service(b(3), b(3), &rig.bus);
    let jobs = drain(&rig.bus);
    assert_eq!(jobs.len(), 1);
    assert_eq!(&jobs[0].video[..], &[0u8; 12][..]);
    assert_eq!(jobs[0].video_tc_100ns, b(3));
}

#[test]
fn a_disabled_input_receives_nothing_and_offers_only_while_still_on_program() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    rig.shared.set_settings(InputSettings {
        enabled: false,
        source: SOURCE.to_string(),
    });
    // Still selected on program (cut while active): standby, never a fill.
    let jobs = rig.run(3);
    assert_one_pair_per_boundary(&jobs, 3);
    jobs.iter().for_each(assert_standby);
    assert!(rig.mock.calls().is_empty(), "no receiver created");
    assert_eq!(rig.status().boundaries, 0, "nothing received");
    // Not on program: no program job.
    rig.bus = Arc::new(ProgramBus::new());
    assert!(rig.run(2).is_empty());
}

#[test]
fn an_enabled_input_without_a_source_receives_nothing() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    rig.shared.set_settings(InputSettings {
        enabled: true,
        source: String::new(),
    });
    let jobs = rig.run(2);
    assert_one_pair_per_boundary(&jobs, 2);
    jobs.iter().for_each(assert_standby);
    assert!(rig.mock.calls().is_empty(), "no receiver created");
    assert_eq!(rig.status().boundaries, 0, "nothing received");
    // Not on program: no program job.
    rig.bus = Arc::new(ProgramBus::new());
    assert!(rig.run(2).is_empty());
}

#[test]
fn a_source_change_closes_the_receiver_and_opens_the_new_one() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    rig.run(1);
    rig.shared.set_settings(InputSettings {
        enabled: true,
        source: "CAM (2)".to_string(),
    });
    rig.input.service(b(2), b(2), &rig.bus);
    let lifecycle: Vec<String> = rig
        .mock
        .calls()
        .into_iter()
        .filter(|c| !c.starts_with("framesync_capture") && !c.starts_with("framesync_free"))
        .collect();
    assert_eq!(
        lifecycle,
        vec![
            "recv_create(CG-OBS (manual),SongPlayer program input)",
            "framesync_create(1)",
            "framesync_destroy(2)",
            "recv_destroy(1)",
            "recv_create(CAM (2),SongPlayer program input)",
            "framesync_create(3)",
        ]
    );
    // Disabling closes it again.
    rig.shared.set_settings(InputSettings::default());
    rig.input.service(b(3), b(3), &rig.bus);
    assert_eq!(rig.mock.calls().last().unwrap(), "recv_destroy(3)");
}

#[test]
fn a_failed_receiver_is_retried_exactly_5_s_later() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    rig.mock.set_fail_create(true, false);
    let mut standby = Vec::new();
    for k in 1..=150 {
        rig.input.service(b(k), b(k), &rig.bus);
        standby.extend(drain(&rig.bus));
    }
    assert_eq!(rig.calls_matching("recv_create"), 1, "no retry inside 5 s");
    assert_eq!(standby.len(), 150, "standby on every boundary meanwhile");
    standby.iter().for_each(assert_standby);
    rig.mock.set_fail_create(false, false);
    rig.input.service(b(151), b(151), &rig.bus); // b(1) + 5 s exactly
    assert_eq!(rig.calls_matching("recv_create"), 2);
    let jobs = drain(&rig.bus);
    assert_eq!(jobs[0].width, 4, "connected: the source's frame");
}

#[test]
fn the_stop_path_closes_the_receiver() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    rig.run(1);
    rig.input.disconnect();
    assert_eq!(
        rig.mock.calls()[rig.mock.calls().len() - 2..],
        ["framesync_destroy(2)", "recv_destroy(1)"]
    );
    assert!(!rig.status().connected);
}

#[test]
fn two_distinct_frames_with_the_same_timecode_are_two_frames_not_a_repeat() {
    // A sender that does not advance its timecode: only the buffer differs.
    let mut rig = rig(
        vec![uyvy(0, 30, 1, 1), uyvy(0, 30, 1, 2)],
        vec![Some(0), Some(1)],
    );
    let jobs = rig.run(2);
    assert_eq!(
        &jobs[1].video[..],
        &[2u8; 12][..],
        "the second frame, not a repeat of the first"
    );
    let st = rig.status();
    assert_eq!((st.frames_received, st.video_repeats), (2, 0));
}

#[test]
fn the_converted_picture_is_a_pooled_buffer_of_exactly_the_nv12_size() {
    // 50×34 NV12 = 2550 bytes, a size class no other test uses: once every
    // holder is gone, the buffer is recycled into `frame_pool` under exactly
    // that capacity (a wrongly sized take would grow into another class).
    const CAP: usize = 50 * 34 * 3 / 2;
    let frame = MockVideoFrame {
        xres: 50,
        yres: 34,
        four_cc: FOURCC_UYVY,
        line_stride: 100,
        frame_rate_n: 30,
        frame_rate_d: 1,
        frame_format_type: FRAME_FORMAT_TYPE_PROGRESSIVE,
        timecode: 0,
        data: vec![77; 100 * 34],
    };
    let before = sp_decoder::frame_pool::pool_len(CAP);
    let mut rig = rig(vec![frame], vec![Some(0)]);
    let jobs = rig.run(1);
    assert_eq!(jobs[0].video.len(), CAP);
    assert_eq!(
        (jobs[0].width, jobs[0].height, jobs[0].stride),
        (50, 34, 50)
    );
    drop(jobs);
    drop(rig); // the input's own reference to the converted frame
    assert_eq!(sp_decoder::frame_pool::pool_len(CAP), before + 1);
}

#[test]
fn a_reconnect_does_not_count_the_outage_as_dropped_frames() {
    // Frame 10 arrives after an outage of 10 frames: nothing was DROPPED on
    // our side, the source was simply gone.
    let mut rig = rig(source_frames(30, 30), vec![Some(0), Some(1), Some(10)]);
    rig.run(2);
    rig.mock.set_connections(0);
    rig.input.service(b(3), b(3), &rig.bus);
    rig.mock.set_connections(1);
    rig.input.service(b(4), b(4), &rig.bus);
    let st = rig.status();
    assert_eq!((st.frames_received, st.video_drops), (3, 0));
    assert!(rig.shared.is_connected());
}

#[test]
fn a_field_that_still_arrives_is_standby_not_a_half_height_picture() {
    let mut field_0 = uyvy(0, 30, 1, 3);
    field_0.frame_format_type = 2; // NDIlib_frame_format_type_field_0
    let mut field_1 = uyvy(333_333, 30, 1, 4);
    field_1.frame_format_type = 3; // NDIlib_frame_format_type_field_1
    let mut rig = rig(vec![field_0, field_1], vec![Some(0), Some(1)]);
    let jobs = rig.run(2);
    assert_one_pair_per_boundary(&jobs, 2);
    jobs.iter().for_each(assert_standby);
    assert_eq!(rig.status().unsupported_boundaries, 2);
}

#[test]
fn an_interleaved_whole_frame_is_converted_like_a_progressive_one() {
    let mut interleaved = uyvy(0, 30, 1, 6);
    interleaved.frame_format_type = 0; // NDIlib_frame_format_type_interleaved
    let mut rig = rig(vec![interleaved], vec![Some(0)]);
    let jobs = rig.run(1);
    assert_eq!(&jobs[0].video[..], &[6u8; 12][..]);
    assert_eq!(rig.status().unsupported_boundaries, 0);
}

// --- status -----------------------------------------------------------------

#[test]
fn the_status_reports_the_source_its_stream_and_the_format() {
    let mut rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    rig.run(2);
    rig.shared
        .set_visible((0..20).map(|i| format!("M ({i})")).collect());
    let st = rig.status();
    assert_eq!(st.id, -1);
    assert_eq!(st.label, "OBS manuál");
    assert!(st.enabled);
    assert!(st.connected);
    assert!(!st.running, "no loop thread in this test");
    assert_eq!(st.source, SOURCE);
    assert_eq!(st.stream, "manual");
    assert_eq!(st.last_frame_size.as_deref(), Some("4x2"));
    assert_eq!(st.format.as_deref(), Some("UYVY"));
    assert_eq!(st.frame_rate.as_deref(), Some("30/1"));
    assert_eq!(st.visible_sources.len(), INPUT_MAX_VISIBLE);
    assert_eq!(st.visible_sources[15], "M (15)");
    assert_eq!((st.resyncs, st.relatches), (0, 0));
    assert!(rig.shared.is_connected());
    let json = serde_json::to_value(&st).unwrap();
    assert_eq!(json["frames_received"], 2);
    assert_eq!(json["last_frame_size"], "4x2");
}

#[test]
fn a_fresh_input_reports_nothing_received() {
    let shared = NdiInputShared::default();
    let st = shared.status(&InputSettings::default());
    assert!(!st.enabled && !st.connected && !st.running);
    assert_eq!((st.source.as_str(), st.stream.as_str()), ("", ""));
    assert_eq!(st.last_frame_size, None);
    assert_eq!(st.format, None);
    assert_eq!(st.frame_rate, None);
    assert!(st.visible_sources.is_empty());
    assert!(!shared.is_connected());
    assert!(!shared.is_stopped());
    shared.stop();
    assert!(shared.is_stopped());
}

// --- pure helpers -------------------------------------------------------------

#[test]
fn uyvy_to_nv12_is_exact_on_a_known_4x2_picture() {
    // Row 0: U0 Y0 V0 Y1 | U1 Y2 V1 Y3; row 1 the same layout.
    let src = [
        10, 1, 20, 2, 30, 3, 40, 4, //
        13, 5, 23, 6, 33, 7, 44, 8,
    ];
    let mut out = vec![99u8; 3];
    assert!(uyvy_to_nv12(&src, 4, 2, 8, &mut out));
    assert_eq!(
        out,
        vec![
            1, 2, 3, 4, 5, 6, 7, 8, // luma
            12, 22, 32, 42, // (U, V) = rounded row-pair means
        ]
    );
}

#[test]
fn uyvy_to_nv12_skips_row_padding_and_handles_two_row_pairs() {
    // 2×4, stride 6 (2 padding bytes per row, 0xEE).
    let src = [
        100, 1, 200, 2, 0xEE, 0xEE, //
        101, 3, 202, 4, 0xEE, 0xEE, //
        50, 5, 60, 6, 0xEE, 0xEE, //
        52, 7, 61, 8, 0xEE, 0xEE,
    ];
    let mut out = Vec::new();
    assert!(uyvy_to_nv12(&src, 2, 4, 6, &mut out));
    assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8, 101, 201, 51, 61]);
}

#[test]
fn uyvy_to_nv12_rejects_what_it_cannot_convert() {
    let src = [0u8; 64];
    let cases: [(usize, usize, usize, usize); 7] = [
        (0, 2, 8, 16),  // zero width
        (4, 0, 8, 16),  // zero height
        (3, 2, 8, 16),  // odd width
        (4, 3, 8, 24),  // odd height
        (4, 2, 7, 16),  // stride under 2 × width
        (4, 2, 8, 15),  // src one byte short of stride × height
        (4, 2, 16, 31), // a padded stride makes src short too
    ];
    for (w, h, stride, len) in cases {
        let mut out = vec![1u8; 4];
        assert!(
            !uyvy_to_nv12(&src[..len], w, h, stride, &mut out),
            "{w}x{h} stride {stride} len {len}"
        );
        assert!(out.is_empty(), "the output is cleared");
    }
    // The exact minimum is accepted.
    let mut out = Vec::new();
    assert!(uyvy_to_nv12(&src[..16], 4, 2, 8, &mut out));
    assert_eq!(out.len(), 12);
}

#[test]
fn skipped_frames_counts_the_source_frames_between_two_received_ones() {
    assert_eq!(skipped_frames(0, 333_333, 30, 1), 0, "the next frame");
    assert_eq!(
        skipped_frames(0, 333_333, 60, 1),
        1,
        "one 60 fps frame skipped"
    );
    assert_eq!(skipped_frames(0, 666_667, 30, 1), 1);
    assert_eq!(skipped_frames(0, 400_000, 25, 1), 0);
    assert_eq!(
        skipped_frames(0, 1_001_000, 30_000, 1_001),
        2,
        "29.97: 3 frames apart"
    );
    assert_eq!(skipped_frames(0, 0, 30, 1), 0, "a repeat stamp");
    assert_eq!(
        skipped_frames(500_000, 0, 30, 1),
        0,
        "backwards never counts"
    );
    assert_eq!(skipped_frames(0, 333_333, 30, 0), 0, "no rate");
    assert_eq!(
        skipped_frames(10_000_000, 0, 30, -1),
        0,
        "a negative rate counts nothing"
    );
    // Rounding: 1.4 frames apart is the next frame, 1.6 skips one.
    assert_eq!(skipped_frames(0, 466_666, 30, 1), 0);
    assert_eq!(skipped_frames(0, 533_334, 30, 1), 1);
}

#[test]
fn grid_step_waits_services_catches_up_resyncs_and_relatches() {
    let slot = interval_100ns(GENLOCK_GRID_FPS);
    assert_eq!(INPUT_RELATCH_100NS, 666_666);
    // Before the next boundary: wait exactly until it.
    assert_eq!(
        grid_step(b(5) + 1_000, b(5)),
        InputGridStep::Wait(b(6) - b(5) - 1_000)
    );
    assert_eq!(grid_step(b(6) - 1, b(5)), InputGridStep::Wait(1));
    // On it: service it.
    let on = InputGridStep::Service {
        boundary: b(6),
        resync: false,
    };
    assert_eq!(grid_step(b(6), b(5)), on);
    // Up to 8 boundaries behind: catch up one by one.
    assert_eq!(grid_step(b(14), b(5)), on);
    assert_eq!(grid_step(b(14) + slot / 2, b(5)), on);
    // More than 8 behind: resync on the current floor.
    assert_eq!(
        grid_step(b(15), b(5)),
        InputGridStep::Service {
            boundary: b(15),
            resync: true
        }
    );
    // A next boundary exactly two slots ahead still waits; further re-latches.
    assert_eq!(
        grid_step(b(6) - INPUT_RELATCH_100NS, b(5)),
        InputGridStep::Wait(INPUT_RELATCH_100NS)
    );
    assert_eq!(
        grid_step(b(6) - INPUT_RELATCH_100NS - 1, b(5)),
        InputGridStep::Relatch
    );
}

#[test]
fn needs_find_only_while_enabled_and_not_connected() {
    let every = INPUT_FIND_EVERY;
    assert_eq!(every, Duration::from_secs(30));
    assert!(needs_find(true, false, None));
    assert!(needs_find(true, false, Some(every)));
    assert!(!needs_find(
        true,
        false,
        Some(every - Duration::from_nanos(1))
    ));
    assert!(
        !needs_find(true, true, None),
        "connected: nothing to look for"
    );
    assert!(!needs_find(false, false, None), "disabled");
}

#[test]
fn video_format_support_and_fourcc_text() {
    let f = |four_cc, width, height| VideoFormat {
        four_cc,
        width,
        height,
        frame_rate_n: 30,
        frame_rate_d: 1,
        full_frame: true,
    };
    let field = VideoFormat {
        full_frame: false,
        ..f(FOURCC_UYVY, 4, 2)
    };
    assert!(!field.is_supported(), "a field is not converted");
    assert!(f(FOURCC_UYVY, 4, 2).is_supported());
    assert!(f(FOURCC_UYVA, 1920, 1080).is_supported());
    assert!(!f(u32::from_le_bytes(*b"BGRA"), 4, 2).is_supported());
    assert!(!f(FOURCC_UYVY, 0, 2).is_supported());
    assert!(!f(FOURCC_UYVY, 4, 0).is_supported());
    assert!(!f(FOURCC_UYVY, -2, 2).is_supported());
    assert!(!f(FOURCC_UYVY, 4, -2).is_supported());
    assert!(!f(FOURCC_UYVY, 3, 2).is_supported());
    assert!(!f(FOURCC_UYVY, 4, 3).is_supported());
    assert_eq!(f(FOURCC_UYVY, 4, 2).four_cc_str(), "UYVY");
    assert_eq!(
        f(0x0041_2000, 4, 2).four_cc_str(),
        "??A?",
        "NUL + space are unprintable"
    );
}

#[test]
fn input_settings_are_active_only_enabled_with_a_source() {
    assert!(enabled().active());
    assert!(
        !InputSettings {
            enabled: false,
            ..enabled()
        }
        .active()
    );
    assert!(
        !InputSettings {
            enabled: true,
            source: String::new()
        }
        .active()
    );
    assert_eq!(INPUT_RECONNECT_100NS, 50_000_000);
}

// --- the grid loop on its own thread -------------------------------------------

/// A virtual clock: a sleep advances it (never past `limit` — there it really
/// sleeps 1 ms so the idle loop does not spin), and landing exactly on `at`
/// jumps it to `to` once.
struct FakeClock {
    now: i64,
    limit: i64,
    jump: Option<(i64, i64)>,
}

impl VbanClock for FakeClock {
    fn now_100ns(&mut self) -> i64 {
        self.now
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        let next = self.now + d_100ns;
        if next > self.limit {
            thread::sleep(Duration::from_millis(1));
            return;
        }
        self.now = next;
        if let Some((at, to)) = self.jump
            && self.now == at
        {
            self.now = to;
            self.jump = None;
        }
    }
}

/// Run the loop on its own thread until `expected` boundaries were serviced,
/// stop it, and return the rig's status + the program jobs left queued. A loop
/// that never gets there or never stops fails in ≤ 10 s instead of hanging.
fn run_loop(clock: FakeClock, expected: u64) -> (NdiInputStatus, Vec<SubmitJob>, Rig) {
    let rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    let Rig {
        mock,
        shared,
        bus,
        input,
    } = rig;
    let (tx, rx) = mpsc::channel();
    let loop_bus = bus.clone();
    thread::spawn(move || {
        let mut input = input;
        let mut clock = clock;
        run_input_loop(&mut input, &loop_bus, &mut clock);
        let _ = tx.send(input);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.status(&enabled()).boundaries < expected {
        assert!(
            Instant::now() < deadline,
            "the loop never serviced {expected} boundaries"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(shared.is_running(), "the loop reports itself running");
    shared.stop();
    let input = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the loop stops on the stop flag");
    assert!(!shared.is_running());
    let status = shared.status(&enabled());
    let jobs = drain(&bus);
    (
        status,
        jobs,
        Rig {
            mock,
            shared,
            bus,
            input,
        },
    )
}

#[test]
fn the_loop_services_every_boundary_on_time_and_stops_on_the_flag() {
    let clock = FakeClock {
        now: b(0) + 1_000,
        limit: b(30),
        jump: None,
    };
    let (st, jobs, rig) = run_loop(clock, 30);
    assert_eq!(st.boundaries, 30, "exactly b(1)..=b(30)");
    assert_eq!((st.resyncs, st.relatches), (0, 0));
    // The bus queue keeps the newest 10: contiguous, audio stamped on time.
    let stamps: Vec<i64> = jobs.iter().map(|j| j.video_tc_100ns).collect();
    assert_eq!(stamps, (21..=30).map(b).collect::<Vec<_>>());
    for job in &jobs {
        assert_eq!(
            job.audio_tc_100ns, job.video_tc_100ns,
            "serviced on the boundary"
        );
    }
    let calls = rig.mock.calls();
    assert_eq!(
        calls[calls.len() - 2..],
        ["framesync_destroy(2)", "recv_destroy(1)"],
        "stopping closes the receiver"
    );
}

#[test]
fn the_loop_resyncs_after_more_than_8_missed_boundaries() {
    let clock = FakeClock {
        now: b(20) + 1_000,
        limit: b(50),
        jump: Some((b(25), b(45))),
    };
    let (st, jobs, _rig) = run_loop(clock, 10);
    // b(21)..=b(24), then the resync on b(45), then b(46)..=b(50).
    assert_eq!(st.boundaries, 10);
    assert_eq!(st.resyncs, 1);
    assert_eq!(st.relatches, 0);
    let stamps: Vec<i64> = jobs.iter().map(|j| j.video_tc_100ns).collect();
    assert!(stamps.ends_with(&(45..=50).map(b).collect::<Vec<_>>()));
}

#[test]
fn the_loop_relatches_after_a_backward_clock_step() {
    let clock = FakeClock {
        now: b(20) + 1_000,
        limit: b(50),
        jump: Some((b(25), b(15))),
    };
    let (st, _jobs, _rig) = run_loop(clock, 39);
    // b(21)..=b(24), re-latch, then b(16)..=b(50).
    assert_eq!(st.boundaries, 39);
    assert_eq!(st.relatches, 1);
    assert_eq!(st.resyncs, 0);
}
