//! #289 step 2: characterization of the program's audio side. One scripted
//! run through the bus and the `SP-program` output (a source on program, its
//! hot blocks limited, a 300 ms fade to a second source, a boundary that
//! source misses and the program fills, its hot blocks after the fade) pins,
//! per served boundary, the block the audio outputs got (its stamp and every
//! sample) and the audio the NDI submit carried. The pin was taken over the
//! sender that ran the crossfade, the limiter and the outputs' feed itself
//! (before step 2), so the audio side must give exactly these blocks wherever
//! it runs.
//! Wired via `#[cfg(test)] #[path = "program_output_tests_audio.rs"] mod tests_audio;`.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::ProgramOutput;
use crate::playback::audio_out::AudioOutputs;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_audio::{AudioLine, AudioSide, LineStatus, take_audio};
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramCore, ProgramJob};
use crate::playback::program_transition::{SpecSource, TransitionSpec};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::{VbanOut, VbanTake};

const T0: i64 = 17_900_000_000_000_000;
const MS: i64 = 10_000;
const FRAMES: usize = 1600;
const SRC_A: i64 = 1;
const SRC_B: i64 = 2;

/// The boundaries the script runs.
pub(super) const BOUNDARIES: usize = 30;

/// The k-th grid boundary after `floor(T0)`.
pub(super) fn b(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

/// Boundary `k`'s stereo block of a `hz` tone at `amplitude`, its phase
/// running on across boundaries, the right channel at half the left.
fn tone(k: usize, hz: f64, amplitude: f32) -> AudioFrame {
    let data = (0..FRAMES)
        .flat_map(|i| {
            let n = (k * FRAMES + i) as f64;
            let s = amplitude * (TAU * hz * n / 48_000.0).sin() as f32;
            [s, s * 0.5]
        })
        .collect();
    AudioFrame {
        data,
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    }
}

/// Source `pid`'s pair for boundary `k`: a 2×2 picture (the test canvas)
/// and its tone, hot (over the limiter's ceiling) where the script says.
fn pair(pid: i64, k: usize) -> SubmitJob {
    let (hz, hot) = if pid == SRC_A {
        (997.0, (2..=4).contains(&k))
    } else {
        (440.0, (24..=26).contains(&k))
    };
    let amplitude = if hot { 1.4 } else { 0.3 };
    SubmitJob {
        width: 2,
        height: 2,
        stride: 2,
        video: SharedFrame::new(vec![pid as u8; 6]).into(),
        audio: vec![tone(k, hz, amplitude)],
        video_tc_100ns: b(k),
        audio_tc_100ns: b(k),
        live: true,
        media_pts_100ns: None,
        blend: None,
    }
}

/// The script, on `core`: A on program, both sources offering every
/// boundary, a 300 ms fade to B cut after b(5), B missing b(20). Returns the
/// program boundaries in the order the bus queued them.
pub(super) fn script(core: &mut ProgramCore) -> Vec<ProgramJob> {
    let mut jobs = Vec::new();
    script_with(core, |core| {
        while let Some(job) = core.take() {
            jobs.push(job);
        }
    });
    jobs
}

/// The script, with `step` run on the core after every boundary's release.
pub(super) fn script_with(core: &mut ProgramCore, mut step: impl FnMut(&mut ProgramCore)) {
    core.select_initial(SRC_A);
    core.set_transition(TransitionSpec::fade(300, SpecSource::Setting));
    for k in 1..=BOUNDARIES {
        core.touch(SRC_A, b(k));
        core.offer(SRC_A, pair(SRC_A, k));
        if k != 20 {
            core.touch(SRC_B, b(k));
            core.offer(SRC_B, pair(SRC_B, k));
        }
        if k == 5 {
            assert!(core.cut(SRC_B, b(5) + 5 * MS));
        }
        core.release(b(k) + MS);
        step(core);
    }
}

/// The program output on a mock sender with one VBAN output, and both.
pub(super) fn output() -> (
    ProgramOutput<MockNdiBackend>,
    Arc<VbanOut>,
    Arc<MockNdiBackend>,
) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let vban = Arc::new(VbanOut::new());
    let out = ProgramOutput::new(sender, 2, 2).with_vban(vban.clone());
    (out, vban, backend)
}

/// One served boundary as the audio outputs and NDI saw it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Heard {
    /// The block's stamp and every sample's bits (`None`: the silence).
    pub(super) out_due: i64,
    pub(super) out_samples: Option<Vec<u32>>,
    /// The NDI submit's audio, planar, every sample's bits.
    pub(super) ndi: Vec<u32>,
}

/// What VBAN and NDI got for the boundary just served.
pub(super) fn heard(vban: &VbanOut, backend: &MockNdiBackend) -> Heard {
    let block = match vban.take_timeout(Duration::ZERO) {
        VbanTake::Block(block) => block,
        other => panic!("VBAN got no block: {other:?}"),
    };
    Heard {
        out_due: block.due_100ns,
        out_samples: block
            .samples
            .map(|s| s.iter().map(|x| x.to_bits()).collect()),
        ndi: backend
            .last_audio_planar()
            .iter()
            .map(|x| x.to_bits())
            .collect(),
    }
}

/// 64-bit FNV-1a over the records' `Debug` text, in order.
pub(super) fn fingerprint(heard: &[Heard]) -> u64 {
    let text = format!("{heard:?}");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Whether the block the outputs got peaks at the limiter's ceiling
/// (0.98): a hot block, limited.
pub(super) fn at_the_ceiling(h: &Heard) -> bool {
    let peak = h
        .out_samples
        .iter()
        .flatten()
        .fold(0.0_f32, |m, &b| m.max(f32::from_bits(b).abs()));
    (0.979..0.981).contains(&peak)
}

/// The pin: how many boundaries, how many the limiter scaled (any sample
/// left at the ceiling's level), and the fingerprint of every one.
pub(super) const PIN_BOUNDARIES: usize = BOUNDARIES;
pub(super) const PIN_FINGERPRINT: u64 = 15_627_807_239_806_358_378;

/// The sender before step 2 ran the whole audio side itself.
#[test]
fn the_sender_alone_gives_the_pinned_audio() {
    let mut core = ProgramCore::new();
    let jobs = script(&mut core);
    let (mut out, vban, backend) = output();
    let mut all = Vec::new();
    for job in jobs {
        out.submit(job);
        all.push(heard(&vban, &backend));
    }
    assert_eq!(all.len(), PIN_BOUNDARIES, "every boundary served once");
    let mixed = all
        .windows(2)
        .all(|w| strict_next_boundary_100ns(w[0].out_due, GENLOCK_GRID_FPS) == w[1].out_due);
    assert!(mixed, "contiguous stamps");
    let silent = all.iter().filter(|h| h.out_samples.is_none()).count();
    assert_eq!(silent, 1, "one fill: b(20), B's missing boundary");
    let limited = all.iter().filter(|h| at_the_ceiling(h)).count();
    assert!(limited >= 2, "the hot blocks were limited: {limited}");
    assert_eq!(fingerprint(&all), PIN_FINGERPRINT, "every block as before");
}

/// #289 step 2: the bus moves each boundary's audio out at its commit, the
/// audio side (the `program-audio` thread's) limits it and feeds the outputs,
/// and the sender takes the limited block from the line: the outputs and NDI
/// get exactly the pinned blocks.
#[test]
fn the_audio_side_on_its_own_gives_the_pinned_audio() {
    let mut core = ProgramCore::new().with_audio_ahead(0);
    let line = Arc::new(AudioLine::default());
    let vban = Arc::new(VbanOut::new());
    let outputs = Arc::new(AudioOutputs::single_vban(vban.clone()));
    let mut side = AudioSide::new().with_outputs(outputs);
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let mut out = ProgramOutput::new(sender, 2, 2).with_audio_line(line.clone());
    let mut all = Vec::new();
    script_with(&mut core, |core| {
        let mut audio = Vec::new();
        while let Some(job) = core.take_audio() {
            audio.push(job.stamp_100ns());
            line.push(side.process(job, &|| 0));
        }
        let mut jobs = Vec::new();
        while let Some(job) = core.take() {
            jobs.push(job);
        }
        let stamps: Vec<i64> = jobs.iter().map(ProgramJob::stamp_100ns).collect();
        assert_eq!(
            audio, stamps,
            "one audio job per committed boundary, in order"
        );
        for job in jobs {
            if let ProgramJob::Source(source) = &job {
                assert!(source.audio.is_empty(), "the job's audio was moved out");
            }
            out.submit(job);
            all.push(heard(&vban, &backend));
        }
    });
    assert_eq!(all.len(), PIN_BOUNDARIES);
    assert_eq!(fingerprint(&all), PIN_FINGERPRINT, "every block as before");
    assert_eq!(
        line.status(),
        LineStatus::default(),
        "nothing stale or missing"
    );
}

/// A boundary whose block never reached the line goes to NDI with the
/// silence, counted, and the next one still finds its own block.
#[test]
fn a_block_missing_from_the_line_is_the_silence_on_ndi() {
    let line = Arc::new(AudioLine::default());
    let mut side = AudioSide::new();
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let mut out = ProgramOutput::new(sender, 2, 2).with_audio_line(line.clone());
    let mut job = ProgramJob::Source(pair(SRC_A, 1));
    let mut next = ProgramJob::Source(pair(SRC_A, 2));
    let _lost = take_audio(&mut job);
    line.push(side.process(take_audio(&mut next), &|| 0));
    out.submit(job);
    assert!(
        backend.last_audio_planar().iter().all(|&s| s == 0.0),
        "the silence"
    );
    assert_eq!(backend.last_audio_planar().len(), 3200);
    out.submit(next);
    assert!(
        backend.last_audio_planar().iter().any(|&s| s != 0.0),
        "its own block"
    );
    let status = line.status();
    assert_eq!((status.missing, status.stale), (1, 0));
}

/// A clock that records the wait asked of it and how many NDI audio sends
/// had gone out by then.
struct HoldClock {
    start: std::time::Instant,
    backend: Arc<MockNdiBackend>,
    waited: Arc<std::sync::Mutex<Option<(std::time::Duration, usize)>>>,
}

impl crate::playback::program_max_send::SendClock for HoldClock {
    fn now(&mut self) -> std::time::Instant {
        self.start
    }
    fn wait_until(&mut self, due: std::time::Instant) {
        let sent = self
            .backend
            .calls()
            .iter()
            .filter(|c| c.starts_with("send_audio"))
            .count();
        let mut waited = self.waited.lock().unwrap();
        waited.get_or_insert((due - self.start, sent));
    }
}

/// #289 step 2, the production order: a job that came a slot early is
/// held to its stamp BEFORE anything of it goes out, the boundary is taken
/// after the hold (`ready_late_us`), and NDI carries its own limited block.
#[test]
fn a_job_ahead_is_held_to_its_stamp_before_its_ndi_submit() {
    let line = Arc::new(AudioLine::default());
    let mut side = AudioSide::new();
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let waited = Arc::new(std::sync::Mutex::new(None));
    let clock = HoldClock {
        start: std::time::Instant::now(),
        backend: backend.clone(),
        waited: waited.clone(),
    };
    let mut out = ProgramOutput::new(sender, 2, 2)
        .with_audio_line(line.clone())
        .with_video_clock(Box::new(clock));
    let mut job = ProgramJob::Source(pair(SRC_A, 3));
    let block = side.process(take_audio(&mut job), &|| 0);
    let sent_block = block.audio.as_ref().unwrap()[0].data.clone();
    line.push(block);
    let reads = std::cell::Cell::new(0);
    let now = || {
        reads.set(reads.get() + 1);
        if reads.get() == 1 {
            b(3) - 300_000
        } else {
            b(3) + 40
        }
    };
    let marks = out.serve(job, Some(SRC_A), now);
    let held = *waited.lock().unwrap();
    assert_eq!(
        held,
        Some((std::time::Duration::from_millis(30), 0)),
        "held 30 ms to its stamp, before any NDI send"
    );
    assert_eq!(marks.taken_100ns, b(3) + 40, "taken after the hold");
    let ndi = backend.last_audio_planar();
    let left: Vec<f32> = sent_block.iter().step_by(2).copied().collect();
    assert_eq!(
        &ndi[..1600],
        &left[..],
        "NDI carries the boundary's own block"
    );
    let _ = out;
}
