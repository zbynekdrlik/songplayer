//! #210: the program's audio goes through ONE peak limiter after the scene
//! transition's crossfade (finding 5986249387). The equal-power fade sums two
//! sources that are each at most 0.98 up to 0.98·√2 at mid-fade, and VBAN's
//! INT24 encoder clamps everything at ±1.0 flat, at FOH. The limiter is
//! `sp_decoder`'s (the #184 stem-mix limiter: ceiling 0.98, stereo-linked,
//! instant attack, 50 ms release, bit-identical at rest), one instance owned
//! by the `SP-program` output, run on every block BEFORE VBAN and the NDI
//! submit get it.
//! Wired via `#[cfg(test)] #[path = "program_output_tests_limit.rs"] mod tests_limit;`.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::{MixRun, ProgramOutput};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_transition::{AudioFormat, MixJob, mix_audio_block};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::{VbanBlock, VbanOut, VbanTake};

const T0: i64 = 17_900_000_000_000_000;

/// The stem mix's limiter ceiling (`sp_decoder`'s `LIMIT_CEILING`), which the
/// program's limiter shares: no sample the program sends goes over it.
const CEILING: f32 = 0.98;

/// A margin only: the limiter clamps a limited sample to the ceiling
/// exactly (release 0.71.0 review; the f32 gain alone could leave it a code
/// or two above).
const CEILING_SLACK: f32 = 1e-6;

/// Frames per boundary: 48 kHz on the 30 fps grid.
const FRAMES: usize = 1600;

/// The program's audio format, as `ProgramOutput::split` mixes a fade.
const FORMAT: AudioFormat = AudioFormat {
    frames: FRAMES,
    channels: 2,
    sample_rate: 48_000,
};

/// The k-th grid boundary after `floor(T0)`.
fn at(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

/// Boundary `k`'s stereo block of a 997 Hz tone at `amplitude`, the same on
/// both channels, its phase running on across boundaries. Two sources playing
/// it are fully correlated: the worst case for a crossfade's sum.
fn tone(k: usize, amplitude: f32) -> Vec<f32> {
    (0..FRAMES)
        .flat_map(|i| {
            let n = (k * FRAMES + i) as f64;
            let s = amplitude * (TAU * 997.0 * n / 48_000.0).sin() as f32;
            [s, s]
        })
        .collect()
}

/// A program block of `data`: 48 kHz stereo, one boundary.
fn block(data: Vec<f32>) -> AudioFrame {
    AudioFrame {
        data,
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: None,
    }
}

/// A source's boundary pair on `stamp`: a 2×2 picture (the test canvas) and
/// the block `data`.
fn pair(stamp: i64, data: Vec<f32>) -> SubmitJob {
    SubmitJob {
        width: 2,
        height: 2,
        stride: 2,
        video: SharedFrame::new(vec![90u8; 6]),
        audio: vec![block(data)],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live: true,
    }
}

/// Boundary `k` as slot `slot` of an `n`-boundary fade from `from` to `to`.
fn fade(k: usize, slot: u32, n: u32, from: Vec<f32>, to: Vec<f32>) -> ProgramJob {
    ProgramJob::Mix(MixJob {
        stamp_100ns: at(k),
        from: Some(pair(at(k), from)),
        to: Some(pair(at(k), to)),
        slot,
        n_slots: n,
    })
}

/// What that fade boundary's crossfade gives BEFORE any limiter: the same
/// `mix_audio_block` the sender runs.
fn unlimited_fade(slot: u32, n: u32, from: Vec<f32>, to: Vec<f32>) -> Vec<f32> {
    let frames = FRAMES as u64;
    mix_audio_block(
        Some(&block(from)),
        Some(&block(to)),
        u64::from(slot) * frames,
        u64::from(n) * frames,
        FORMAT,
    )
    .data
}

/// The program output on a mock sender with its VBAN output, and the mock.
fn output() -> (
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

/// The block VBAN got for the boundary just served.
fn vban_block(vban: &VbanOut) -> VbanBlock {
    match vban.take_timeout(Duration::ZERO) {
        VbanTake::Block(block) => block,
        other => panic!("VBAN got no block: {other:?}"),
    }
}

/// NDI's last planar block back in VBAN's interleaved order (L, R, L, R, …).
fn ndi_interleaved(backend: &MockNdiBackend) -> Vec<f32> {
    let planar = backend.last_audio_planar();
    let (left, right) = planar.split_at(planar.len() / 2);
    left.iter().zip(right).flat_map(|(&l, &r)| [l, r]).collect()
}

/// Serve `job`; return what VBAN's encoder gets (its input samples) and what
/// the `SP-program` NDI submit carried, interleaved.
fn serve(
    out: &mut ProgramOutput<MockNdiBackend>,
    vban: &VbanOut,
    backend: &MockNdiBackend,
    job: ProgramJob,
) -> (Vec<f32>, Vec<f32>) {
    out.submit(job);
    let samples = vban_block(vban).samples.expect("an audio block");
    (samples, ndi_interleaved(backend))
}

/// The largest |sample|.
fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |m, s| m.max(s.abs()))
}

/// The samples' exact bits, for bit-for-bit comparisons.
fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|s| s.to_bits()).collect()
}

/// The finding's case: slot 4 of a 9-boundary (300 ms) fade between two
/// sources playing the same full-scale content. The crossfade alone sums it
/// to 0.98·(cos θ + sin θ) ≈ 1.39, which VBAN's encoder clamps flat at ±1.0.
#[test]
fn a_mid_fade_boundary_of_two_correlated_full_scale_sources_stays_under_the_vban_clamp() {
    let (mut out, vban, backend) = output();
    let job = fade(4, 4, 9, tone(4, CEILING), tone(4, CEILING));
    let unlimited = unlimited_fade(4, 9, tone(4, CEILING), tone(4, CEILING));
    assert!(
        peak(&unlimited) > 1.3,
        "the crossfade alone goes over full scale: {}",
        peak(&unlimited)
    );

    let (encoder_input, ndi) = serve(&mut out, &vban, &backend, job);

    let peak = peak(&encoder_input);
    assert!(
        peak <= CEILING + CEILING_SLACK,
        "VBAN's encoder input peaks at {peak}: over the 0.98 ceiling, and its INT24 clamp \
         cuts everything from 1.0 on flat at FOH"
    );
    assert!(
        peak > CEILING - 0.01,
        "limited down to the ceiling, not attenuated further: {peak}"
    );
    assert_eq!(
        bits(&encoder_input),
        bits(&ndi),
        "VBAN and SP-program's NDI audio carry the same limited block"
    );
}

/// The limiter's state carries from one boundary to the next, so the gain is
/// continuous: it drops at once to what a frame needs (instant attack) and
/// rises at most by the 50 ms release per frame (1/2400 at 48 kHz), boundary
/// edges included. The fade's release tail reaches into the forwarded
/// boundary right after it. Fade: 9 boundaries of two correlated full-scale
/// tones; then the incoming source alone, forwarded.
#[test]
fn the_gain_is_continuous_through_the_fade_and_its_tail_reaches_the_next_boundary() {
    let (mut out, vban, backend) = output();
    let n = 9;
    // (frame index across the boundaries, the frame's gain = out / in) on
    // the left channel, for every frame whose input is far enough from 0 to
    // read its gain.
    let mut gains: Vec<(usize, f64)> = Vec::new();
    for k in 0..=n {
        let src = tone(k, CEILING);
        let (job, input) = if k < n {
            let slot = k as u32;
            let input = unlimited_fade(slot, n as u32, src.clone(), src.clone());
            (fade(k, slot, n as u32, src.clone(), src), input)
        } else {
            (ProgramJob::Source(pair(at(k), src.clone())), src)
        };
        let (limited, _) = serve(&mut out, &vban, &backend, job);
        for i in 0..FRAMES {
            let x = input[2 * i];
            if x.abs() >= 0.05 {
                gains.push((k * FRAMES + i, f64::from(limited[2 * i]) / f64::from(x)));
            }
        }
    }

    for w in gains.windows(2) {
        let ((i0, g0), (i1, g1)) = (w[0], w[1]);
        assert!(
            g1 - g0 <= (i1 - i0) as f64 / 2400.0 + 1e-5,
            "frame {i0} → {i1}: the gain jumped from {g0} to {g1}, faster than the release"
        );
        assert!(g1 <= 1.0 + 1e-6, "frame {i1}: gain {g1} over unity");
    }
    let (frame, tail) = gains
        .iter()
        .copied()
        .find(|&(i, _)| i >= n * FRAMES)
        .expect("the forwarded boundary has audible frames");
    assert!(
        tail < 0.99,
        "frame {frame} (the first boundary after the fade): gain {tail}, the fade's \
         release tail must carry over, never jump back to unity at the boundary edge"
    );
}

/// The fade of the case above, then the incoming source alone on boundary
/// `k_after` at a level well under the ceiling. Returns that boundary's input
/// and what VBAN got.
fn fade_then(k_after: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut out, vban, backend) = output();
    for k in 0..9 {
        let job = fade(k, k as u32, 9, tone(k, CEILING), tone(k, CEILING));
        serve(&mut out, &vban, &backend, job);
    }
    let data = tone(k_after, 0.5);
    let job = ProgramJob::Source(pair(at(k_after), data.clone()));
    let (limited, _) = serve(&mut out, &vban, &backend, job);
    (data, limited)
}

/// Where the program's timeline restarts (its stamps skip boundaries: a
/// resync), the audio after it is unrelated, so the limiter drops the fade's
/// release tail and that boundary goes out bit for bit. The same boundary
/// right after the fade carries the tail.
#[test]
fn a_restart_of_the_program_timeline_drops_the_release_tail() {
    let (data, next) = fade_then(9);
    assert_ne!(
        bits(&next),
        bits(&data),
        "the boundary right after the fade is still in the fade's release tail"
    );
    let (data, after_gap) = fade_then(9 + 20);
    assert_eq!(
        bits(&after_gap),
        bits(&data),
        "20 boundaries skipped (a resync): the tail is dropped, the block passes bit for bit"
    );
}

/// Once the release tail has decayed (the gain is exactly 1.0 again), the
/// forwarded audio is bit-identical once more.
#[test]
fn the_audio_is_bit_identical_again_once_the_release_ends() {
    let (mut out, vban, backend) = output();
    for k in 0..9 {
        let job = fade(k, k as u32, 9, tone(k, CEILING), tone(k, CEILING));
        serve(&mut out, &vban, &backend, job);
    }
    let mut last = (Vec::new(), Vec::new());
    for k in 9..49 {
        let data = tone(k, 0.5);
        let job = ProgramJob::Source(pair(at(k), data.clone()));
        let (limited, ndi) = serve(&mut out, &vban, &backend, job);
        assert!(
            peak(&limited) <= 0.5,
            "boundary {k}: never louder than its input"
        );
        last = (data, limited);
        assert_eq!(bits(&last.1), bits(&ndi), "boundary {k}: VBAN = NDI");
    }
    assert_eq!(
        bits(&last.1),
        bits(&last.0),
        "40 boundaries (1.3 s) after the fade the tail is over: bit for bit"
    );
}

/// Outside a fade SongPlayer's own playlists are at or under the ceiling (the
/// stem mix limits at 0.98, a plain song is loudness-normalized far under
/// it), so the limiter is at rest and a forwarded boundary goes out bit for
/// bit, on VBAN and on NDI, a peak right at the ceiling included. The
/// standby silence stays silence.
#[test]
fn outside_a_fade_a_block_at_or_under_the_ceiling_passes_bit_for_bit() {
    let (mut out, vban, backend) = output();
    for k in 0..3 {
        let data = tone(k, CEILING);
        let job = ProgramJob::Source(pair(at(k), data.clone()));
        let (limited, ndi) = serve(&mut out, &vban, &backend, job);
        assert_eq!(
            bits(&limited),
            bits(&data),
            "boundary {k}: VBAN, bit for bit"
        );
        assert_eq!(bits(&ndi), bits(&data), "boundary {k}: NDI, bit for bit");
    }
    let mut at_ceiling = vec![CEILING; FRAMES * 2];
    at_ceiling[1] = -CEILING;
    let job = ProgramJob::Source(pair(at(3), at_ceiling.clone()));
    let (limited, _) = serve(&mut out, &vban, &backend, job);
    assert_eq!(bits(&limited), bits(&at_ceiling), "a block AT the ceiling");

    out.submit(ProgramJob::Standby { stamp_100ns: at(4) });
    assert_eq!(vban_block(&vban), VbanBlock::silence(at(4)));
    assert_eq!(
        bits(&backend.last_audio_planar()),
        vec![0; FRAMES * 2],
        "the standby silence stays exactly 0.0"
    );
}

/// Outside a fade a block OVER the ceiling is limited too. SongPlayer's own
/// playlists never carry one, but the NDI input "OBS manuál" forwards cg
/// OBS's audio as it comes: a hot feed now goes out at the ceiling, VBAN and
/// NDI alike, where VBAN used to clamp everything from 1.0 on flat.
#[test]
fn a_hot_block_outside_a_fade_is_limited_too() {
    let (mut out, vban, backend) = output();
    let job = ProgramJob::Source(pair(at(0), tone(0, 1.2)));
    let (limited, ndi) = serve(&mut out, &vban, &backend, job);
    let peak = peak(&limited);
    assert!(
        peak <= CEILING + CEILING_SLACK && peak > CEILING - 0.01,
        "a 1.2 peak goes out at the ceiling: {peak}"
    );
    assert_eq!(bits(&limited), bits(&ndi), "VBAN = NDI");
}

/// The standby silence carries the release tail on: a fade's tail decays
/// through the standby boundaries after it as through audio (0 × gain stays
/// 0), so a source that comes back 30 boundaries later goes out bit for bit.
/// A standby that skipped the limiter would freeze the tail (a reduction of
/// ~0.07) and that source would come out reduced.
#[test]
fn the_release_tail_decays_through_the_standby_boundaries() {
    let (mut out, vban, backend) = output();
    for k in 0..9 {
        let job = fade(k, k as u32, 9, tone(k, CEILING), tone(k, CEILING));
        serve(&mut out, &vban, &backend, job);
    }
    for k in 9..39 {
        out.submit(ProgramJob::Standby { stamp_100ns: at(k) });
        assert_eq!(vban_block(&vban), VbanBlock::silence(at(k)));
        assert_eq!(
            bits(&backend.last_audio_planar()),
            vec![0; FRAMES * 2],
            "boundary {k}: the silence stays exactly 0.0"
        );
    }
    let data = tone(39, 0.5);
    let job = ProgramJob::Source(pair(at(39), data.clone()));
    let (limited, _) = serve(&mut out, &vban, &backend, job);
    assert_eq!(
        bits(&limited),
        bits(&data),
        "the tail decayed through the standby: bit for bit"
    );
}

/// A block that is not one program block (48 kHz stereo, one boundary: what
/// VBAN carries) is not the limiter's: VBAN sends silence for it, and NDI
/// gets it as it came.
#[test]
fn a_block_that_is_not_one_program_block_passes_as_it_came() {
    let (mut out, vban, backend) = output();
    let mut job = pair(at(0), Vec::new());
    job.audio = vec![AudioFrame {
        data: vec![1.5; FRAMES],
        channels: 1,
        sample_rate: 48_000,
        timecode_100ns: None,
    }];
    out.submit(ProgramJob::Source(job));
    let block = vban_block(&vban);
    assert!(block.substituted, "VBAN sends silence for it");
    assert_eq!(backend.last_audio_planar(), vec![1.5; FRAMES]);
}

/// The fade's INFO line counts the frames the limiter scaled while the run
/// went out (`MixRun::limited`): two mid-fade boundaries whose every frame is
/// over the ceiling (a constant 0.98 from both sides sums to
/// 0.98·(cos θ + sin θ) > 0.98 for every θ inside the window). The boundary
/// that ends the run takes the run with it.
#[test]
fn the_fade_line_counts_the_frames_the_limiter_scaled() {
    let (mut out, vban, backend) = output();
    let full = || vec![CEILING; FRAMES * 2];
    serve(&mut out, &vban, &backend, fade(4, 4, 9, full(), full()));
    assert_eq!(
        out.mix_run.limited, 1600,
        "every frame of slot 4 was scaled"
    );
    serve(&mut out, &vban, &backend, fade(5, 5, 9, full(), full()));
    assert_eq!(out.mix_run.limited, 3200, "and every frame of slot 5");
    let job = ProgramJob::Source(pair(at(6), vec![0.5; FRAMES * 2]));
    serve(&mut out, &vban, &backend, job);
    assert_eq!(out.mix_run, MixRun::default(), "the run ended");
}
