//! #210: the program hands a boundary's audio block to VBAN BEFORE its
//! `SP-program` NDI submit, so FOH audio never waits for the video side (a
//! slow NDI send, a mixed picture). The NDI backend here holds the first send
//! of a pair's submit (its audio: the submitter sends the audio first) behind
//! a gate, and each test reads the VBAN queue while that send is still held:
//! a forwarded pair, the standby pair and a mixed boundary. No sleeps: the
//! held send tells the test it is waiting, and every wait is bounded.
//! Wired via `#[cfg(test)] #[path = "program_output_tests_order.rs"] mod tests_order;`.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, FourCCVideoType, NdiBackend, NdiError, NdiSender};

use super::ProgramOutput;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_transition::{MixJob, crossfade_gains};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::{VbanBlock, VbanOut, VbanTake};

const T0: i64 = 17_900_000_000_000_000;

/// An NDI backend that records like [`MockNdiBackend`] and runs `on_audio`
/// inside every `send_audio` — the first call of a pair's NDI submit —
/// before it records the send.
struct HookedNdi {
    inner: MockNdiBackend,
    on_audio: Box<dyn Fn() + Send + Sync>,
}

impl NdiBackend for HookedNdi {
    fn send_create_with_clocking(
        &self,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<usize, NdiError> {
        self.inner
            .send_create_with_clocking(name, clock_video, clock_audio)
    }

    fn send_destroy(&self, handle: usize) {
        self.inner.send_destroy(handle)
    }

    fn send_video(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        self.inner.send_video(
            handle,
            four_cc,
            width,
            height,
            stride,
            frame_rate_n,
            frame_rate_d,
            data,
            timecode_100ns,
        )
    }

    unsafe fn send_video_async(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        // SAFETY: the caller's contract (`data` stays valid until the next
        // synchronising call on this sender) is passed on unchanged.
        unsafe {
            self.inner.send_video_async(
                handle,
                four_cc,
                width,
                height,
                stride,
                frame_rate_n,
                frame_rate_d,
                data,
                timecode_100ns,
            )
        }
    }

    fn send_video_flush(&self, handle: usize) {
        self.inner.send_video_flush(handle)
    }

    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples_per_channel: i32,
        interleaved: &[f32],
        timecode_100ns: Option<i64>,
    ) {
        (self.on_audio)();
        self.inner.send_audio(
            handle,
            sample_rate,
            channels,
            samples_per_channel,
            interleaved,
            timecode_100ns,
        )
    }

    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
        self.inner.send_get_tally(handle, timeout_ms)
    }

    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32 {
        self.inner.send_get_no_connections(handle, timeout_ms)
    }

    fn send_get_source_url(&self, handle: usize) -> Option<String> {
        self.inner.send_get_source_url(handle)
    }

    fn discover_local_sources(
        &self,
        want_names: &[String],
        overall_timeout_ms: u32,
    ) -> Vec<(String, String)> {
        self.inner
            .discover_local_sources(want_names, overall_timeout_ms)
    }
}

/// The gate a held send waits behind.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.opened.notify_all();
    }

    /// Wait until the gate opens. Bounded, so no bug can hang the test
    /// binary; every test opens it on every path ([`Opener`]).
    fn wait(&self) {
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        let _open = self
            .opened
            .wait_timeout_while(open, Duration::from_secs(60), |open| !*open)
            .unwrap_or_else(|p| p.into_inner());
    }
}

/// Opens the gate when dropped, a failed assertion included.
struct Opener(Arc<Gate>);

impl Drop for Opener {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// What VBAN and NDI got for one boundary submitted over a held NDI send.
struct Seen {
    /// Blocks in the VBAN queue WHILE the NDI submit was held in its first
    /// send.
    queued_while_held: usize,
    /// The block VBAN got.
    block: VbanBlock,
    /// The stamp `submit` returned.
    stamp: i64,
    video_timecodes: Vec<i64>,
    audio_timecodes: Vec<i64>,
    /// The audio NDI got, planar (L then R).
    ndi_planar: Vec<f32>,
}

/// Submit `job` on its own thread over an NDI backend whose first send waits
/// behind a gate, read the VBAN queue while it waits, then open the gate and
/// collect what VBAN and NDI got.
fn submit_with_ndi_held(job: ProgramJob) -> Seen {
    let gate = Arc::new(Gate::default());
    let _opener = Opener(gate.clone());
    let (entered_tx, entered_rx) = mpsc::channel();
    let held = gate.clone();
    let backend = Arc::new(HookedNdi {
        inner: MockNdiBackend::new(),
        on_audio: Box::new(move || {
            let _ = entered_tx.send(());
            held.wait();
        }),
    });
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let vban = Arc::new(VbanOut::new());
    let out = ProgramOutput::new(sender, 2, 2).with_vban(vban.clone());
    let submit = std::thread::spawn(move || {
        let mut out = out;
        let stamp = out.submit(job);
        // Hand the output back so its sender outlives the reads below.
        (out, stamp)
    });
    entered_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the pair's NDI submit started");
    let queued_while_held = vban.queued();
    gate.open();
    let (_out, stamp) = submit.join().expect("the submit thread");
    let block = match vban.take_timeout(Duration::ZERO) {
        VbanTake::Block(block) => block,
        other => panic!("VBAN got no block: {other:?}"),
    };
    Seen {
        queued_while_held,
        block,
        stamp,
        video_timecodes: backend.inner.video_timecodes(),
        audio_timecodes: backend.inner.audio_timecodes(),
        ndi_planar: backend.inner.last_audio_planar(),
    }
}

/// NDI's planar block back in VBAN's interleaved order (L, R, L, R, …).
fn interleaved(planar: &[f32]) -> Vec<f32> {
    let (left, right) = planar.split_at(planar.len() / 2);
    left.iter().zip(right).flat_map(|(&l, &r)| [l, r]).collect()
}

/// The k-th grid boundary after `floor(T0)`.
fn at(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

/// One stereo 1600-frame block: `level` + `step` per interleaved sample, so
/// with a step the left and right samples differ.
fn samples(level: f32, step: f32) -> Vec<f32> {
    (0..3200).map(|i| level + i as f32 * step).collect()
}

/// A source's boundary pair: a `width`×2 NV12 picture and one stereo
/// 1600-frame block, its audio stamped `audio_tc`.
fn pair(width: u32, stamp: i64, audio_tc: i64, data: Vec<f32>) -> SubmitJob {
    SubmitJob {
        width,
        height: 2,
        stride: width,
        video: SharedFrame::new(vec![90u8; width as usize * 3]),
        audio: vec![AudioFrame {
            data,
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: audio_tc,
        live: true,
    }
}

#[test]
fn a_forwarded_pairs_block_reaches_vban_while_its_ndi_submit_is_held() {
    let stamp = at(0);
    let data = samples(0.25, 1e-4);
    let seen = submit_with_ndi_held(ProgramJob::Source(pair(4, stamp, stamp + 77, data.clone())));
    assert_eq!(
        seen.queued_while_held, 1,
        "VBAN has the pair's block before its NDI submit returns"
    );
    assert_eq!(
        seen.block,
        VbanBlock {
            due_100ns: stamp,
            samples: Some(data.clone()),
            substituted: false,
        },
        "the pair's own block, on its boundary"
    );
    assert_eq!(
        interleaved(&seen.ndi_planar),
        data,
        "NDI still carries the same block"
    );
    assert_eq!(
        (seen.stamp, seen.video_timecodes, seen.audio_timecodes),
        (stamp, vec![stamp], vec![stamp + 77]),
        "the source's own stamps"
    );
}

#[test]
fn the_standby_silence_reaches_vban_while_its_ndi_submit_is_held() {
    let stamp = at(1);
    let seen = submit_with_ndi_held(ProgramJob::Standby { stamp_100ns: stamp });
    assert_eq!(
        seen.queued_while_held, 1,
        "VBAN has the standby silence before its NDI submit returns"
    );
    assert_eq!(seen.block, VbanBlock::silence(stamp));
    assert_eq!(
        seen.ndi_planar,
        vec![0.0; 3200],
        "NDI still gets the silence"
    );
    assert_eq!(
        (seen.stamp, seen.video_timecodes, seen.audio_timecodes),
        (stamp, vec![stamp], vec![stamp]),
        "stamped on its boundary (#224)"
    );
}

#[test]
fn a_mixed_boundarys_block_reaches_vban_before_its_picture_and_ndi_submit() {
    // The two pictures differ in size, so the NDI side also fits and blends
    // the outgoing picture: none of that may come before the VBAN hand-off.
    let stamp = at(2);
    let mix = MixJob {
        stamp_100ns: stamp,
        from: Some(pair(4, stamp, stamp + 11, samples(0.25, 0.0))),
        to: Some(pair(8, stamp, stamp + 22, samples(0.5, 0.0))),
        slot: 4,
        n_slots: 9,
    };
    let seen = submit_with_ndi_held(ProgramJob::Mix(mix));
    assert_eq!(
        seen.queued_while_held, 1,
        "VBAN has the crossfaded block before the mixed picture goes to NDI"
    );
    assert_eq!(
        (seen.block.due_100ns, seen.block.substituted),
        (stamp, false),
        "the program's own block, on its window boundary (#224)"
    );
    let vban = seen.block.samples.expect("the crossfaded block");
    assert_eq!(
        vban,
        interleaved(&seen.ndi_planar),
        "VBAN and NDI carry the same crossfaded block"
    );
    let (g_from, g_to) = crossfade_gains(4 * 1600, 9 * 1600);
    assert_eq!(
        vban[0],
        g_from * 0.25 + g_to * 0.5,
        "slot 4 of 9 on the window's curve"
    );
    assert_eq!(
        (seen.stamp, seen.video_timecodes, seen.audio_timecodes),
        (stamp, vec![stamp], vec![stamp])
    );
}
