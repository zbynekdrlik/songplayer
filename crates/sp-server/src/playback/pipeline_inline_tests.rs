//! Inline pipeline tests extracted from pipeline.rs to keep that file
//! under the 1000-line cap.
//! Included via `#[path = "pipeline_inline_tests.rs"]` in pipeline.rs.

use super::*;
use crate::playback::ndi_health::PlaybackStateLabel;
use std::time::Instant;

#[test]
fn pipeline_spawn_and_shutdown() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let pipeline = PlaybackPipeline::spawn("test-ndi".into(), None, event_tx, 1);
    pipeline.shutdown();
    // If we get here, the thread joined successfully.
}

#[test]
fn pipeline_drop_sends_shutdown() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let _pipeline = PlaybackPipeline::spawn("test-drop".into(), None, event_tx, 2);
        // Pipeline dropped here — Drop impl should send Shutdown and join.
    }
    // If we get here without hanging, the Drop worked correctly.
}

#[test]
fn pipeline_send_command_before_shutdown() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let pipeline = PlaybackPipeline::spawn("test-cmd".into(), None, event_tx, 3);
    pipeline.send(PipelineCommand::Stop);
    pipeline.send(PipelineCommand::Pause);
    pipeline.send(PipelineCommand::Resume);
    pipeline.shutdown();
}

#[test]
fn pipeline_play_emits_event_on_non_windows() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let pipeline = PlaybackPipeline::spawn("test-play".into(), None, event_tx, 4);

    pipeline.send(PipelineCommand::Play {
        video: PathBuf::from("/tmp/test_video.mp4"),
        audio: PathBuf::from("/tmp/test_audio.flac"),
    });
    // Give the thread a moment to process.
    std::thread::sleep(std::time::Duration::from_millis(50));
    pipeline.shutdown();

    // On non-Windows, we expect an Error event.
    #[cfg(not(windows))]
    {
        let (id, event) = event_rx.try_recv().expect("should have received an event");
        assert_eq!(id, 4);
        match event {
            PipelineEvent::Error(msg) => {
                assert!(msg.contains("Windows"), "error should mention Windows");
            }
            other => panic!("expected Error event, got {other:?}"),
        }
    }

    let _ = event_rx;
}

#[test]
fn seek_variant_carries_position_ms() {
    let cmd = PipelineCommand::Seek { position_ms: 12345 };
    match cmd {
        PipelineCommand::Seek { position_ms } => assert_eq!(position_ms, 12345),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn seek_does_not_collide_with_other_variants() {
    // Compile-time check that every variant is still distinct.
    let variants = vec![
        PipelineCommand::Play {
            video: PathBuf::new(),
            audio: PathBuf::new(),
        },
        PipelineCommand::Pause,
        PipelineCommand::Resume,
        PipelineCommand::Seek { position_ms: 0 },
        PipelineCommand::Stop,
        PipelineCommand::Shutdown,
        PipelineCommand::RecreateSender,
    ];
    assert_eq!(variants.len(), 7);
}

#[test]
fn pipeline_send_seek_command() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let pipeline = PlaybackPipeline::spawn("test-seek".into(), None, event_tx, 6);
    pipeline.send(PipelineCommand::Seek { position_ms: 5000 });
    pipeline.shutdown();
    // No panic or hang means the Seek arm is handled in the loop.
}

/// Regression test for the NewPlay bug: the outer loop must continue to
/// receive and process subsequent Play commands after the first one returns.
/// Before the fix, NewPlay was discarded and the worker would hang waiting
/// for the next command instead of playing the new file.
///
/// On non-Windows the stub emits an Error per Play. On Windows the loop
/// path is structurally the same — verified live on win-resolume v0.7.2.
#[test]
fn pipeline_processes_multiple_sequential_plays() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let pipeline = PlaybackPipeline::spawn("test-multi-play".into(), None, event_tx, 5);

    pipeline.send(PipelineCommand::Play {
        video: PathBuf::from("/tmp/song-a_video.mp4"),
        audio: PathBuf::from("/tmp/song-a_audio.flac"),
    });
    std::thread::sleep(std::time::Duration::from_millis(30));
    pipeline.send(PipelineCommand::Play {
        video: PathBuf::from("/tmp/song-b_video.mp4"),
        audio: PathBuf::from("/tmp/song-b_audio.flac"),
    });
    std::thread::sleep(std::time::Duration::from_millis(30));
    pipeline.send(PipelineCommand::Play {
        video: PathBuf::from("/tmp/song-c_video.mp4"),
        audio: PathBuf::from("/tmp/song-c_audio.flac"),
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    pipeline.shutdown();

    // Drain all events. We expect at least one event per Play command —
    // proving the worker did not hang after the first Play.
    #[cfg(not(windows))]
    {
        let mut event_count = 0;
        while let Ok((id, event)) = event_rx.try_recv() {
            assert_eq!(id, 5);
            match event {
                PipelineEvent::Error(_) => event_count += 1,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(
            event_count, 3,
            "expected 3 Error events (one per Play), got {event_count}"
        );
    }

    let _ = event_rx;
}

/// Pin: `PipelineCommand::Play` must reset `paused = false` BEFORE
/// entering `decode_and_send`, so a stale `Pause` cannot leak across
/// video changes. Static check via `include_str!` — fires red if the
/// `paused = false` statement is moved out of the Play arm, deleted,
/// or commented out.
#[test]
fn play_command_clears_paused_state() {
    let src = include_str!("pipeline.rs");
    let play_arm_start = src
        .find("Ok(PipelineCommand::Play {")
        .expect("Play arm must exist");
    let decode_call = src[play_arm_start..]
        .find("decode_and_send(")
        .expect("Play arm must call decode_and_send");
    let play_block = &src[play_arm_start..play_arm_start + decode_call];

    // Strict match: an actual statement (semicolon-terminated), not a
    // commented-out line or a docstring mention. Strip any line whose
    // first non-whitespace chars are `//` before checking.
    let live_lines: String = play_block
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        live_lines.contains("paused = false;"),
        "PipelineCommand::Play must clear `paused = false;` (live statement, not \
         a comment) BEFORE decode_and_send. Current Play arm:\n{play_block}"
    );
}

#[test]
fn health_snapshot_variant_constructs_and_clones() {
    let now = Instant::now();
    let ev = PipelineEvent::HealthSnapshot {
        connections: 1,
        frames_submitted_total: 100,
        frames_submitted_last_5s: 30,
        observed_fps: 29.97,
        nominal_fps: 29.97,
        last_submit_ts: Some(now),
        last_heartbeat_ts: now,
        consecutive_bad_polls: 0,
        reported_state: PlaybackStateLabel::Playing,
    };
    let cloned = ev.clone();
    // Pattern-match to assert the variant exists and the fields round-trip.
    if let PipelineEvent::HealthSnapshot {
        connections,
        frames_submitted_last_5s,
        reported_state,
        ..
    } = cloned
    {
        assert_eq!(connections, 1);
        assert_eq!(frames_submitted_last_5s, 30);
        assert_eq!(reported_state, PlaybackStateLabel::Playing);
    } else {
        panic!("clone produced wrong variant");
    }
}
