//! #277: a video that ends long before its duration is a failure the engine
//! counts (#229's run, `open_failures`), and the engine asks for a repair of
//! that video. Its own songs (ids no other test uses): the repair requests
//! are process-wide.

use std::sync::PoisonError;

use super::{PID, current, fail, mark, out, played_since_mark, rig_with, start, started, view};
use crate::playback::early_end::{EarlyEnd, after_start};
use crate::playback::pipeline::PipelineEvent;
use crate::playback::state::PlayEvent;

/// An early end's `Error` text, as the pipeline sends it before the song's
/// first picture (the Play's answer).
fn early_end(file: &str) -> String {
    let early = EarlyEnd {
        at_ms: 66,
        duration_ms: 250_965,
    };
    early.reason(file)
}

/// Whether the engine asked for a repair of video row `video_id`.
fn asked(video_id: i64) -> bool {
    crate::video_upgrade::repair::requests()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_asked(video_id)
}

#[tokio::test]
async fn an_early_end_before_the_first_picture_is_a_failed_open_that_asks_a_repair() {
    let mut rig = rig_with(&[27_701]).await;
    start(&mut rig.engine).await;
    mark(&mut rig.engine);
    let reason = early_end("a.mp4");
    rig.engine
        .handle_pipeline_event(PID, PipelineEvent::Error(reason.clone()))
        .await;
    assert_eq!(
        view(&rig.engine).map(|f| (f.count, f.last_error)),
        Some((1, reason)),
        "#229's run counts it, with the early end's text on the health row"
    );
    assert!(asked(27_701), "a repair of the video is asked");
    assert!(
        played_since_mark(&rig.engine),
        "the first failure sends the next song at once"
    );
}

#[tokio::test]
async fn a_failed_open_asks_no_repair() {
    let mut rig = rig_with(&[27_702]).await;
    start(&mut rig.engine).await;
    fail(&mut rig.engine).await;
    assert_eq!(view(&rig.engine).map(|f| f.count), Some(1));
    assert!(!asked(27_702), "only an early end asks for a repair");
}

/// The failure of a song that already started (`Started` answered its
/// Play): counted and repaired while no newer Play is under way.
#[tokio::test]
async fn an_early_end_after_its_start_counts_while_no_newer_play_waits() {
    let mut rig = rig_with(&[27_703]).await;
    start(&mut rig.engine).await;
    started(&mut rig.engine).await;
    mark(&mut rig.engine);
    let reason = after_start(&early_end("b.mp4"));
    rig.engine
        .handle_pipeline_event(PID, PipelineEvent::Error(reason.clone()))
        .await;
    assert_eq!(
        view(&rig.engine).map(|f| (f.count, f.last_error)),
        Some((1, reason))
    );
    assert!(asked(27_703));
    assert!(played_since_mark(&rig.engine), "the next song at once");
}

/// A late early end of song A after the operator's skip sent Play B: it
/// answers no Play, so it is A's, not B's. It counts nothing, sends nothing
/// and asks no repair (the pipeline's `current` is B by then), and B's
/// answer is still awaited.
#[tokio::test]
async fn an_early_end_after_its_start_is_ignored_while_a_newer_play_waits() {
    let mut rig = rig_with(&[27_704, 27_705]).await;
    start(&mut rig.engine).await;
    let a = current(&rig.engine).expect("Play A");
    started(&mut rig.engine).await;
    rig.engine.handle_command(PID, PlayEvent::Skip).await; // Play B
    let b = current(&rig.engine).expect("Play B");
    assert_ne!(a, b);
    mark(&mut rig.engine);
    let reason = after_start(&early_end("a.mp4"));
    rig.engine
        .handle_pipeline_event(PID, PipelineEvent::Error(reason))
        .await;
    assert!(!played_since_mark(&rig.engine), "no Play replaced B");
    assert_eq!(current(&rig.engine), Some(b));
    assert_eq!(view(&rig.engine), None, "nothing counted");
    assert!(!asked(a) && !asked(b), "no repair asked");
    assert_eq!(
        out(&rig.engine).pending_plays.pending(),
        1,
        "B's own answer is still awaited"
    );
}
