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
use crate::playback::frame_buf::SharedFrame;
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
    core.select_initial(SRC_A);
    core.set_transition(TransitionSpec::fade(300, SpecSource::Setting));
    let mut jobs = Vec::new();
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
        while let Some(job) = core.take() {
            jobs.push(job);
        }
    }
    jobs
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
