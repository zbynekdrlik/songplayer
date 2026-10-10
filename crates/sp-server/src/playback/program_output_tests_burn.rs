//! #228: the 911014 burn and the item on air, on the PRODUCTION canvas
//! (`ProgramOutput::fhd`) over the mock NDI backend. With the switch off the
//! wire picture is the canvas picture itself; on, a boundary that shows a
//! frame of the item carries the burn of that frame (painted into a copy:
//! the source's own picture, the one `SP-program-MAX` and the Spout FHD
//! sender get, is never written), and every other boundary goes out as it
//! was. The item record follows every served boundary. Wired via
//! `#[cfg(test)] #[path = "program_output_tests_burn.rs"] mod tests_burn;`.

use std::sync::Arc;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::ProgramOutput;
use crate::playback::fleet_shift;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_burn;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_item::{ItemStatus, ProgramItem};
use crate::playback::program_transition::{Layout, MixJob};
use crate::playback::submit_handoff::SubmitJob;

const W: u32 = 1920;
const H: u32 = 1080;
/// 2026-09 in 100 ns since the epoch.
const T0: i64 = 17_900_000_000_000_000;
/// The test item's playlist and video.
const PID: i64 = 5;
const VIDEO: i64 = 42;

/// The program canvas's layout.
fn canvas() -> Layout {
    Layout {
        width: W,
        height: H,
        stride: W,
        len: 3_110_400,
    }
}

/// The FHD output with an item record whose `PID` plays `VIDEO` from 0.
fn rig() -> (
    Arc<MockNdiBackend>,
    ProgramOutput<MockNdiBackend>,
    Arc<ProgramItem>,
) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let item = Arc::new(ProgramItem::default());
    item.mark(PID, VIDEO, 0);
    let out = ProgramOutput::fhd(sender).with_item(item.clone());
    (backend, out, item)
}

/// The `k`-th boundary after `T0`.
fn boundary(k: u32) -> i64 {
    let mut stamp = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        stamp = strict_next_boundary_100ns(stamp, GENLOCK_GRID_FPS);
    }
    stamp
}

/// A 1920×1080 canvas picture of luma `y` (chroma 128).
fn flat(y: u8) -> SharedFrame {
    let luma = (W * H) as usize;
    let mut data = vec![y; luma];
    data.resize(luma + luma / 2, 128);
    SharedFrame::new(data)
}

/// A live pair of `video` on boundary `k`, its frame at `media_pts_100ns`.
fn pair(video: &SharedFrame, k: u32, media_pts_100ns: Option<i64>) -> SubmitJob {
    SubmitJob {
        width: W,
        height: H,
        stride: W,
        video: video.clone(),
        audio: vec![AudioFrame {
            data: vec![0.25; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: boundary(k),
        audio_tc_100ns: boundary(k),
        live: media_pts_100ns.is_some(),
        media_pts_100ns,
    }
}

/// The picture the sender put on the wire last (the async holdover).
fn wire(out: &ProgramOutput<MockNdiBackend>) -> SharedFrame {
    out.submitter
        .held_frame()
        .cloned()
        .expect("a picture was sent")
}

/// The burn of item frame `frame` on boundary `k`, painted on `picture`.
fn expected_burn(picture: &[u8], frame: u32, k: u32) -> SharedFrame {
    let gen_ts_ns = fleet_shift::wire_100ns(boundary(k)) * 100;
    program_burn::burned(picture, canvas(), frame, gen_ts_ns).expect("room on the FHD canvas")
}

#[test]
fn with_the_burn_off_the_canvas_picture_goes_out_as_it_is() {
    let (_backend, mut out, item) = rig();
    let video = flat(255);
    out.serve(
        ProgramJob::Source(pair(&video, 0, Some(0))),
        Some(PID),
        || 0,
    );
    assert!(wire(&out).ptr_eq(&video), "the same allocation, no copy");
    assert_eq!(item.burned(), 0);
}

#[test]
fn with_the_burn_on_an_item_frame_carries_its_burn_in_a_copy() {
    let (_backend, mut out, item) = rig();
    item.set_burn(true);
    let video = flat(255);
    let before = video.to_vec();
    out.serve(
        ProgramJob::Source(pair(&video, 1, Some(330_000))),
        Some(PID),
        || 0,
    );
    let sent = wire(&out);
    assert!(!sent.ptr_eq(&video), "painted into a copy");
    assert!(
        video[..] == before[..],
        "the source's picture (MAX's) is untouched"
    );
    assert!(
        sent[..] == expected_burn(&before, 1, 1)[..],
        "frame 1's burn"
    );
    assert_eq!(item.burned(), 1);
    // Off again: the next frame goes out as it is.
    item.set_burn(false);
    out.serve(
        ProgramJob::Source(pair(&video, 2, Some(660_000))),
        Some(PID),
        || 0,
    );
    assert!(wire(&out).ptr_eq(&video));
    assert_eq!(item.burned(), 1);
}

/// The program's standby, a standby pair of the source (a fill, a paused
/// picture, the pre-roll) and an NDI input pair show no item frame: no burn.
#[test]
fn a_boundary_with_no_item_frame_carries_no_burn() {
    let (_backend, mut out, item) = rig();
    item.set_burn(true);
    let video = flat(255);
    out.serve(ProgramJob::Source(pair(&video, 0, None)), Some(PID), || 0);
    assert!(wire(&out).ptr_eq(&video), "a standby pair as it came");
    out.serve(
        ProgramJob::Standby {
            stamp_100ns: boundary(1),
        },
        Some(PID),
        || 0,
    );
    assert!(wire(&out).iter().take((W * H) as usize).all(|&y| y == 16));
    assert_eq!(item.burned(), 0);
}

/// A fade burns the incoming side's frame onto the fade's picture.
#[test]
fn a_fade_carries_the_burn_of_its_incoming_item_frame() {
    let (_backend, mut out, item) = rig();
    item.set_burn(true);
    let (from, to) = (flat(60), flat(200));
    let mix = MixJob {
        stamp_100ns: boundary(3),
        from: Some(pair(&from, 3, Some(9_990_000))),
        to: Some(pair(&to, 3, Some(660_000))),
        slot: 4,
        n_slots: 9,
    };
    let (_, faded) = out.mix_picture(&mix).expect("both sides here");
    out.serve(ProgramJob::Mix(mix), Some(PID), || 0);
    assert!(
        wire(&out)[..] == expected_burn(&faded, 2, 3)[..],
        "frame 2 (66 ms)"
    );
    assert_eq!(item.burned(), 1);
}

/// The item on the wire, as the API reads it: published on every boundary,
/// ended by a boundary of another source.
#[test]
fn every_served_boundary_publishes_the_item_on_the_wire() {
    let (_backend, mut out, item) = rig();
    let video = flat(255);
    out.serve(
        ProgramJob::Source(pair(&video, 1, Some(330_000))),
        Some(PID),
        || 0,
    );
    let wire_1 = fleet_shift::wire_100ns(boundary(1));
    assert_eq!(
        item.on_air(),
        Some(ItemStatus {
            playlist_id: PID,
            video_id: VIDEO,
            started_at_utc_ns: (wire_1 - 330_000) * 100,
            position_ms: 33,
            frame: 1,
            frame_utc_ns: wire_1 * 100,
        })
    );
    out.serve(
        ProgramJob::Standby {
            stamp_100ns: boundary(2),
        },
        Some(-2),
        || 0,
    );
    assert_eq!(item.on_air(), None, "Blank on program: no item");
}
