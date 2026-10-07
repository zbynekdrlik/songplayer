//! #229: the engine's pause after failed opens (`failure_retry.rs`), the
//! play recorded at the real start, and the health row's `open_failures`.
//!
//! On Linux the test pipeline (`pipeline_stub.rs`) answers every Play with an
//! Error; on Windows the real pipeline answers a Play of these test paths with
//! a decode Error. Only the paused-clock test reads those answers. The others
//! drive the failures themselves and tell a Play by the title clock every Play
//! clears (`begin_play`), never by the pipeline's replies (`rust-workspace.md`).

use std::sync::Arc;
use std::time::Duration;

use sp_core::playback::OpenFailures;
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use crate::playback::ndi_health::{NdiHealthRegistry, PlaybackStateLabel};
use crate::playback::pipeline::PipelineEvent;
use crate::playback::state::{PlayEvent, PlayState};
use crate::playback::title::TitleClock;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig, PlaylistPipeline};

/// The playlist whose videos cannot be opened (an id no other test uses:
/// the dashboard replay and the now-playing panel are process-global).
const PID: i64 = 22_901;
/// Its normalized songs.
const SONGS: [i64; 5] = [22_911, 22_912, 22_913, 22_914, 22_915];
/// What Media Foundation said at PP (6.10.2026) for every cached video.
const ERROR: &str = "No video: SetCurrentMediaType failed: No suitable transform";

struct Rig {
    engine: PlaybackEngine,
    registry: Arc<NdiHealthRegistry>,
}

/// An engine with the playlist's pipeline and its five songs.
async fn rig() -> Rig {
    rig_with(&SONGS).await
}

/// An engine with the playlist's pipeline and these normalized songs.
async fn rig_with(songs: &[i64]) -> Rig {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (?, 'sp-slow', 'u', 'SP-slow', 1)",
    )
    .bind(PID)
    .execute(&pool)
    .await
    .unwrap();
    for &video_id in songs {
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, \
             file_path, audio_file_path) VALUES (?, ?, ?, ?, 'Artist', 1, ?, ?)",
        )
        .bind(video_id)
        .bind(PID)
        .bind(format!("yt{video_id}"))
        .bind(format!("Song {video_id}"))
        .bind(format!("/tmp/sp-229-{video_id}_video.mp4"))
        .bind(format!("/tmp/sp-229-{video_id}_audio.flac"))
        .execute(&pool)
        .await
        .unwrap();
    }
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(64);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(64);
    let registry = Arc::new(NdiHealthRegistry::new());
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache-229"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: registry.clone(),
    });
    engine.ensure_pipeline(PID, "SP-slow");
    Rig { engine, registry }
}

fn out(engine: &PlaybackEngine) -> &PlaylistPipeline {
    engine.pipelines.get(&PID).expect("the playlist's pipeline")
}

/// The run as the health row shows it.
fn view(engine: &PlaybackEngine) -> Option<OpenFailures> {
    out(engine).failures.view()
}

/// The id of the pending retry, if one is armed.
fn pending_id(engine: &PlaybackEngine) -> Option<u64> {
    out(engine).failures.retry.as_ref().map(|retry| retry.id)
}

/// When the pending retry fires, if one is armed.
fn pending_due(engine: &PlaybackEngine) -> Option<Instant> {
    out(engine).failures.retry.as_ref().map(|retry| retry.due)
}

/// The Play in flight failed to open, as the pipeline reports it.
async fn fail(engine: &mut PlaybackEngine) {
    engine
        .handle_pipeline_event(PID, PipelineEvent::Error(ERROR.into()))
        .await;
}

/// The Play in flight opened: its song starts.
async fn started(engine: &mut PlaybackEngine) {
    engine
        .handle_pipeline_event(
            PID,
            PipelineEvent::Started {
                duration_ms: 180_000,
                position_ms: 0,
            },
        )
        .await;
}

/// A ▶: the playlist selects a song and sends its Play.
async fn start(engine: &mut PlaybackEngine) {
    engine.handle_engine_play(PID).await;
    assert!(
        matches!(out(engine).state, PlayState::Playing { .. }),
        "the ▶ sent a Play"
    );
}

/// Every Play clears the song's title clock: a marker clock still there
/// later proves that no Play went out since.
fn mark(engine: &mut PlaybackEngine) {
    let now = Instant::now();
    engine.pipelines.get_mut(&PID).unwrap().title_clock = Some(TitleClock {
        video_id: 0,
        show_at: now,
        hide_at: None,
    });
}

fn played_since_mark(engine: &PlaybackEngine) -> bool {
    out(engine).title_clock.is_none()
}

/// The songs recorded as played, in order.
async fn played(engine: &PlaybackEngine) -> Vec<i64> {
    sqlx::query_scalar("SELECT video_id FROM play_history WHERE playlist_id = ? ORDER BY id")
        .bind(PID)
        .fetch_all(&engine.pool)
        .await
        .unwrap()
}

/// A real-time bound for a whole test whose tokio clock is held still: no
/// tokio timer fires there unless the test moves the clock, so a stalled
/// engine must fail the test, never hang it.
fn real_time_watchdog(bound: Duration) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(bound);
        let _ = tx.send(());
    });
    rx
}

/// The engine's next event (a pipeline's answer, or its own retry).
async fn next_event(
    engine: &mut PlaybackEngine,
    watchdog: &mut oneshot::Receiver<()>,
) -> (i64, PipelineEvent) {
    tokio::select! {
        event = engine.recv_pipeline_event() => event.expect("the event channel stays open"),
        _ = watchdog => panic!("no engine event within the real-time bound: the engine stalled"),
    }
}

/// The finding's case (#229, PP 6.10.2026): every video of a playlist on
/// program fails to open. The playlist tries 3 songs in its first second,
/// then one every 5 s, 30 s, 120 s and 300 s: 7 Plays in 10 minutes, where
/// it used to try ~6 songs a second for as long as it stayed on program.
///
/// The clock is paused and held still: a running blocking task stops tokio's
/// auto-advance (tokio `time::pause`). SQLite answers on its own thread and
/// sqlx's acquire timeout is a tokio timer, so under auto-advance a DB await
/// jumps the clock to that timeout (`worker_tests_idle_gate.rs`, CI run
/// 34926435178). The test moves the clock itself, to each pending retry's
/// deadline, so every Play is timed exactly. Each Play's failure is the
/// pipeline's own answer to it, and it arrives at the Play's instant: the
/// clock does not move while a Play is under way.
#[tokio::test(start_paused = true)]
async fn when_every_open_fails_the_plays_follow_the_pause_table() {
    let (_hold, held) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let _ = held.recv();
    });
    let mut watchdog = real_time_watchdog(Duration::from_secs(30));
    let mut rig = rig().await;
    let t0 = Instant::now();
    rig.engine.handle_scene_change(PID, true).await; // on program: the 1st Play

    let mut answered = Vec::new();
    let mut last_error = String::new();
    for _ in 0..64 {
        if let Some(due) = pending_due(&rig.engine) {
            if due.duration_since(t0) > Duration::from_secs(600) {
                break; // the next attempt is past the first 10 minutes
            }
            tokio::time::advance(due.saturating_duration_since(Instant::now())).await;
        }
        let (playlist_id, event) = next_event(&mut rig.engine, &mut watchdog).await;
        if let PipelineEvent::Error(error) = &event {
            answered.push(t0.elapsed());
            last_error = error.clone();
        }
        rig.engine.handle_pipeline_event(playlist_id, event).await;
    }

    let expected: Vec<Duration> = [0, 0, 0, 5, 35, 155, 455]
        .into_iter()
        .map(Duration::from_secs)
        .collect();
    assert_eq!(
        answered, expected,
        "3 Plays in the first second, then one after 5 s, 30 s, 120 s and 300 s"
    );
    assert_eq!(
        view(&rig.engine).map(|f| (f.count, f.last_error)),
        Some((7, last_error)),
        "the run counts every failure and keeps the last error (the test \
         pipeline's own text: the Linux stub's, or the Windows decode error)"
    );
    assert_eq!(
        out(&rig.engine).state,
        PlayState::WaitingForScene,
        "the playlist waits for its next attempt"
    );
    assert_eq!(
        played(&rig.engine).await,
        Vec::<i64>::new(),
        "a song that never opened is not played: the rotation is untouched"
    );
}

/// A started song ends the run: the count, the error and the retry go, and
/// the next failure counts from one again (the next song at once). The Play
/// that started it already ended the pending retry, and the health row names
/// none while that attempt runs.
#[tokio::test]
async fn a_started_song_ends_the_run_of_failed_opens() {
    let mut rig = rig().await;
    start(&mut rig.engine).await;
    for _ in 0..3 {
        fail(&mut rig.engine).await;
    }
    assert_eq!(view(&rig.engine).map(|f| f.count), Some(3));
    assert!(
        pending_id(&rig.engine).is_some(),
        "the 3rd failure armed a retry"
    );
    assert_eq!(
        out(&rig.engine).state,
        PlayState::WaitingForScene,
        "the playlist waits instead of selecting the next song"
    );

    rig.engine.handle_play_video(PID, SONGS[2], None).await; // the operator's pick
    assert_eq!(
        pending_id(&rig.engine),
        None,
        "the Play ended the pending retry"
    );
    assert_eq!(
        view(&rig.engine).map(|f| (f.count, f.retry_at_ms)),
        Some((3, None)),
        "the count waits for the attempt's outcome; no retry is due while it runs"
    );

    started(&mut rig.engine).await;
    assert_eq!(view(&rig.engine), None, "a started song ends the run");

    fail(&mut rig.engine).await;
    assert_eq!(
        view(&rig.engine).map(|f| f.count),
        Some(1),
        "counted from one"
    );
    assert_eq!(pending_id(&rig.engine), None, "no retry after one failure");
    assert!(
        matches!(out(&rig.engine).state, PlayState::Playing { .. }),
        "the next song was sent at once"
    );
}

/// How the wait of a playlist whose opens fail can end before its retry.
#[derive(Clone, Copy, Debug)]
enum Ends {
    /// The operator skips: the next song is tried at once.
    Skip,
    /// The operator picks a song.
    PlayVideo,
    /// The playlist is cut off program.
    CutOff,
    /// The dashboard's Pause.
    Pause,
}

/// A retry the wait no longer needs is stale: its `RetryDue`, already
/// queued, starts nothing. After a skip or a pick (whose attempt fails too,
/// so a NEW retry waits) it must not try a song before the new pause is
/// over; after a cut off program or a pause no song may start at all.
#[tokio::test]
async fn a_retry_that_the_wait_no_longer_needs_is_ignored() {
    for how in [Ends::Skip, Ends::PlayVideo, Ends::CutOff, Ends::Pause] {
        let mut rig = rig().await;
        rig.engine.handle_scene_change(PID, true).await; // on program: a Play
        for _ in 0..3 {
            fail(&mut rig.engine).await;
        }
        let stale = pending_id(&rig.engine).expect("the 3rd failure armed a retry");

        match how {
            Ends::Skip => rig.engine.handle_command(PID, PlayEvent::Skip).await,
            Ends::PlayVideo => rig.engine.handle_play_video(PID, SONGS[1], None).await,
            Ends::CutOff => rig.engine.handle_scene_change(PID, false).await,
            Ends::Pause => rig.engine.handle_command(PID, PlayEvent::SceneOff).await,
        }
        let tried_now = matches!(how, Ends::Skip | Ends::PlayVideo);
        assert_eq!(
            matches!(out(&rig.engine).state, PlayState::Playing { .. }),
            tried_now,
            "{how:?}: a skip or a pick tries a song at once, a cut or a pause does not"
        );
        assert_eq!(
            pending_id(&rig.engine),
            None,
            "{how:?}: the retry is no longer pending"
        );
        if tried_now {
            fail(&mut rig.engine).await; // that attempt fails too
            assert_eq!(view(&rig.engine).map(|f| f.count), Some(4), "{how:?}");
            assert!(
                pending_id(&rig.engine).is_some_and(|id| id != stale),
                "{how:?}: a new retry waits (30 s)"
            );
        }
        let pending = pending_id(&rig.engine);
        mark(&mut rig.engine);

        rig.engine
            .handle_pipeline_event(PID, PipelineEvent::RetryDue(stale))
            .await;

        assert!(
            !played_since_mark(&rig.engine),
            "{how:?}: the stale retry started no song"
        );
        assert_eq!(
            out(&rig.engine).state,
            PlayState::WaitingForScene,
            "{how:?}: the playlist still waits"
        );
        assert_eq!(
            pending_id(&rig.engine),
            pending,
            "{how:?}: the pending retry (if any) is untouched"
        );
    }
}

/// The retry that IS pending tries the next song when it comes: a `Start`
/// through the state machine, which selects a song and sends its Play.
#[tokio::test]
async fn the_pending_retry_tries_the_next_song() {
    let mut rig = rig().await;
    start(&mut rig.engine).await;
    for _ in 0..3 {
        fail(&mut rig.engine).await;
    }
    let due = pending_id(&rig.engine).expect("the 3rd failure armed a retry");
    mark(&mut rig.engine);

    rig.engine
        .handle_pipeline_event(PID, PipelineEvent::RetryDue(due))
        .await;

    assert!(played_since_mark(&rig.engine), "the retry sent a Play");
    assert!(matches!(out(&rig.engine).state, PlayState::Playing { .. }));
    assert_eq!(pending_id(&rig.engine), None, "the retry is used up");
    assert_eq!(
        view(&rig.engine).map(|f| (f.count, f.retry_at_ms)),
        Some((3, None)),
        "the attempt is under way: no retry is due"
    );
}

/// A failure that would not select the next song arms no retry: a pause
/// came first (here the dashboard's, during the wait), so the playlist stays
/// paused; the failure is still counted.
#[tokio::test]
async fn a_failure_while_paused_arms_no_retry() {
    let mut rig = rig().await;
    start(&mut rig.engine).await;
    for _ in 0..3 {
        fail(&mut rig.engine).await;
    }
    rig.engine.handle_command(PID, PlayEvent::SceneOff).await; // Pause
    assert_eq!(pending_id(&rig.engine), None);

    fail(&mut rig.engine).await;

    assert_eq!(view(&rig.engine).map(|f| f.count), Some(4), "counted");
    assert_eq!(
        pending_id(&rig.engine),
        None,
        "no retry of a paused playlist"
    );
    assert_eq!(out(&rig.engine).state, PlayState::WaitingForScene);
}

/// #134's rule for both paths, at the real start (#229): a selected song and
/// a picked song are recorded when their `Started` comes, a song that failed
/// to open never is. A Play that records nothing (Previous) drops the mark
/// of the Play before it.
#[tokio::test]
async fn a_song_is_recorded_as_played_when_it_starts() {
    let mut rig = rig().await;
    start(&mut rig.engine).await; // a selection
    assert_eq!(
        played(&rig.engine).await,
        Vec::<i64>::new(),
        "a sent Play is no play"
    );

    fail(&mut rig.engine).await; // it never opened; the next song is sent
    assert_eq!(
        played(&rig.engine).await,
        Vec::<i64>::new(),
        "a failed open is no play"
    );

    let selected = out(&rig.engine).current_video_id.expect("a selected song");
    started(&mut rig.engine).await;
    assert_eq!(
        played(&rig.engine).await,
        vec![selected],
        "the song that started is recorded, once"
    );

    rig.engine.handle_play_video(PID, SONGS[4], None).await; // a pick
    assert_eq!(played(&rig.engine).await, vec![selected], "not at the send");
    started(&mut rig.engine).await;
    assert_eq!(played(&rig.engine).await, vec![selected, SONGS[4]]);

    rig.engine.handle_command(PID, PlayEvent::Skip).await; // a selection, marked
    rig.engine.handle_previous(PID).await; // back to SONGS[4]: records nothing
    started(&mut rig.engine).await; // the skipped selection's answer, late
    started(&mut rig.engine).await; // Previous's song opened
    assert_eq!(
        played(&rig.engine).await,
        vec![selected, SONGS[4]],
        "the skipped selection never started, and Previous records nothing"
    );
}

/// #229 follow-up (design record 6029071745): a `Started` names no Play.
/// After Play A, then Play B (a skip in A's pre-roll), A's `Started` comes
/// after B was sent. It answers the EARLIER Play: B has not opened, so it
/// records nothing and leaves B's run of failed opens alone (it used to
/// record B and end the run). B's own `Started` then records B, once.
#[tokio::test]
async fn a_late_started_of_an_earlier_play_neither_records_nor_ends_the_run() {
    let mut rig = rig().await;
    start(&mut rig.engine).await; // its open fails:
    fail(&mut rig.engine).await; // a run of one, and Play A at once
    let a = current(&rig.engine).expect("Play A");
    rig.engine.handle_command(PID, PlayEvent::Skip).await; // Play B
    let b = current(&rig.engine).expect("Play B");
    assert_ne!(b, a, "the skip sent another song");

    started(&mut rig.engine).await; // A opened, late
    assert_eq!(
        played(&rig.engine).await,
        Vec::<i64>::new(),
        "B has not opened: nothing is played yet"
    );
    assert_eq!(
        view(&rig.engine).map(|f| f.count),
        Some(1),
        "B's run is not reset by A's answer"
    );
    assert_eq!(
        out(&rig.engine).title_clock,
        None,
        "and A's answer fixes no title clock for B"
    );

    started(&mut rig.engine).await; // B opened
    assert_eq!(
        played(&rig.engine).await,
        vec![b],
        "exactly one play row, for B"
    );
    assert_eq!(view(&rig.engine), None, "B's start ends the run");
}

/// The same for a late failure: A's `Error` after B was sent is not B's.
/// It counts no failure and selects no song (it used to replace B before B
/// opened); B's own answer counts.
#[tokio::test]
async fn a_late_error_of_an_earlier_play_neither_counts_nor_replaces_the_newer_play() {
    let mut rig = rig().await;
    start(&mut rig.engine).await; // Play A
    rig.engine.handle_command(PID, PlayEvent::Skip).await; // Play B
    let b = current(&rig.engine).expect("Play B");
    mark(&mut rig.engine);

    fail(&mut rig.engine).await; // A did not open, late
    assert!(!played_since_mark(&rig.engine), "no Play replaced B");
    assert_eq!(current(&rig.engine), Some(b), "B is the song under way");
    assert_eq!(view(&rig.engine), None, "A's failure is not counted");

    fail(&mut rig.engine).await; // B did not open
    assert_eq!(
        view(&rig.engine).map(|f| f.count),
        Some(1),
        "B's own failure counts"
    );
    assert!(
        played_since_mark(&rig.engine),
        "and the next song is sent at once"
    );
}

/// A playing heartbeat of the playlist, as its pipeline reports one.
fn heartbeat(engine: &mut PlaybackEngine) {
    let now = std::time::Instant::now();
    engine.handle_health_snapshot(
        PID,
        PipelineEvent::HealthSnapshot {
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            source_fps: 30.0,
            last_submit_ts: None,
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Idle,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
}

/// The playlist's row of `GET /api/v1/ndi/health`, as JSON.
fn health_row(registry: &NdiHealthRegistry) -> serde_json::Value {
    let row = registry
        .snapshots()
        .into_iter()
        .find(|s| s.playlist_id == PID)
        .expect("the playlist's health row");
    serde_json::to_value(row).unwrap()
}

/// The operator sees why the program is black: the health row carries
/// `open_failures {count, last_error, retry_at_ms}` (UTC ms of the next
/// attempt), and `null` while no open failed since the last song started.
#[tokio::test]
async fn the_health_row_names_the_failed_opens_and_the_next_attempt() {
    let mut rig = rig().await;
    heartbeat(&mut rig.engine);
    assert!(
        health_row(&rig.registry)["open_failures"].is_null(),
        "null while no open failed"
    );

    start(&mut rig.engine).await;
    fail(&mut rig.engine).await;
    fail(&mut rig.engine).await;
    let before_arm = chrono::Utc::now().timestamp_millis();
    fail(&mut rig.engine).await; // the 3rd: the next attempt in 5 s
    heartbeat(&mut rig.engine);
    let after_view = chrono::Utc::now().timestamp_millis();

    let row = health_row(&rig.registry);
    let failures = &row["open_failures"];
    assert_eq!(failures["count"], 3);
    assert_eq!(failures["last_error"], ERROR);
    let retry_at_ms = failures["retry_at_ms"]
        .as_i64()
        .expect("the next attempt's instant");
    assert!(
        (before_arm + 5_000 - 5..=after_view + 5_000 + 5).contains(&retry_at_ms),
        "the next attempt is 5 s after the 3rd failure: {retry_at_ms} not in \
         [{before_arm} + 5 s, {after_view} + 5 s]"
    );

    rig.engine.handle_play_video(PID, SONGS[0], None).await;
    started(&mut rig.engine).await;
    heartbeat(&mut rig.engine);
    assert!(
        health_row(&rig.registry)["open_failures"].is_null(),
        "null again once a song started"
    );
}

/// The song the playlist sent last.
fn current(engine: &PlaybackEngine) -> Option<i64> {
    out(engine).current_video_id
}

/// The review of the lane (#229): a song that never opened is not recorded
/// as played, so at the end of a rotation it is the only unplayed song left.
/// Picked again and again, it held the program black for good (5 s, 30 s,
/// 120 s, then every 300 s). The selection leaves out the songs that failed
/// since the last start: the rotation restarts without it.
#[tokio::test]
async fn one_song_that_cannot_be_opened_never_stalls_the_rotation() {
    let mut rig = rig().await;
    let broken = SONGS[0];
    for &song in &SONGS[1..] {
        crate::db::models::record_play(&rig.engine.pool, PID, song)
            .await
            .unwrap();
    }
    start(&mut rig.engine).await;
    assert_eq!(
        current(&rig.engine),
        Some(broken),
        "the rotation's last song"
    );

    fail(&mut rig.engine).await;

    let next = current(&rig.engine).expect("a song was sent");
    assert_ne!(
        next, broken,
        "the next Play is another song, not the one that failed"
    );
    assert!(matches!(out(&rig.engine).state, PlayState::Playing { .. }));
    assert_eq!(
        played(&rig.engine).await,
        Vec::<i64>::new(),
        "the rotation restarted: its history was cleared, as when every song has played"
    );
}

/// The 1st and 2nd failures select the next song at once, and never a song
/// that failed while another one can still be tried: with three songs the
/// third Play is the one song that has not failed yet. (Picked at random, a
/// failed song came back one time in three.) Ten playlists, so a random pick
/// cannot pass by luck.
#[tokio::test]
async fn a_song_that_failed_is_not_picked_again_while_another_can_be() {
    for round in 0..10 {
        let mut rig = rig_with(&SONGS[..3]).await;
        start(&mut rig.engine).await;
        let first = current(&rig.engine).expect("a song");
        fail(&mut rig.engine).await;
        let second = current(&rig.engine).expect("a song");
        fail(&mut rig.engine).await;
        let third = current(&rig.engine).expect("a song");
        assert_ne!(second, first, "round {round}: the 2nd Play is another song");
        assert!(
            third != first && third != second,
            "round {round}: the 3rd Play is the song that has not failed \
             ({first}, {second}, {third})"
        );
    }
}

/// A song is recorded only when it starts, so a skip in its pre-roll found it
/// still unplayed and could pick it again (the review's second finding). The
/// selection leaves out the song just sent: with two songs, every skip before
/// a start alternates between them.
#[tokio::test]
async fn a_skip_before_the_song_starts_never_picks_it_again() {
    let mut rig = rig_with(&SONGS[..2]).await;
    start(&mut rig.engine).await;
    for round in 0..30 {
        let before = current(&rig.engine);
        rig.engine.handle_command(PID, PlayEvent::Skip).await;
        assert_ne!(
            current(&rig.engine),
            before,
            "round {round}: a skip never sends the song it skipped while another is unplayed"
        );
    }
}

/// The health row follows the run as it changes, not only at the pipeline's
/// 5 s heartbeat: a 5 s pause would otherwise come and go unseen. The wait
/// left is reported on the SERVER's clock (`retry_in_ms`, at the read), since
/// the browser that shows it runs on another machine whose clock can be off.
/// Every place that changes the run writes the row: a pause, an operator's
/// pick and Previous (each a Play that ends the retry), a cut off program,
/// a start.
#[tokio::test]
async fn the_health_row_follows_the_run_between_heartbeats() {
    let mut rig = rig().await;
    heartbeat(&mut rig.engine); // the row exists; no heartbeat from here on
    start(&mut rig.engine).await;
    fail(&mut rig.engine).await;
    fail(&mut rig.engine).await;
    let before_arm = std::time::Instant::now();
    fail(&mut rig.engine).await; // the 3rd: a 5 s pause

    let row = health_row(&rig.registry);
    let waited_ms = u64::try_from(before_arm.elapsed().as_millis()).unwrap();
    let failures = &row["open_failures"];
    assert_eq!(failures["count"], 3, "written at the failure");
    let retry_in_ms = failures["retry_in_ms"]
        .as_u64()
        .expect("the wait left, on the server's clock");
    assert!(
        retry_in_ms <= 5_000 && retry_in_ms + waited_ms + 5 >= 5_000,
        "the 5 s pause less the {waited_ms} ms since it was armed: {retry_in_ms} ms left"
    );

    // An operator's pick is the attempt: no retry is due while it runs.
    rig.engine.handle_play_video(PID, SONGS[1], None).await;
    let row = health_row(&rig.registry);
    assert_eq!(
        row["open_failures"]["count"], 3,
        "the count waits for the outcome"
    );
    assert!(
        row["open_failures"]["retry_at_ms"].is_null(),
        "the pick ended the retry"
    );
    fail(&mut rig.engine).await; // the 4th: a 30 s pause
    assert!(health_row(&rig.registry)["open_failures"]["retry_at_ms"].is_i64());

    rig.engine.handle_scene_change(PID, false).await; // cut off program
    let row = health_row(&rig.registry);
    assert_eq!(row["open_failures"]["count"], 4, "the run is kept");
    assert!(
        row["open_failures"]["retry_at_ms"].is_null(),
        "off program no retry is due"
    );

    rig.engine.handle_play_video(PID, SONGS[2], None).await;
    fail(&mut rig.engine).await; // the 5th: a 120 s pause
    assert!(health_row(&rig.registry)["open_failures"]["retry_at_ms"].is_i64());
    rig.engine.handle_previous(PID).await;
    let row = health_row(&rig.registry);
    assert_eq!(row["open_failures"]["count"], 5);
    assert!(
        row["open_failures"]["retry_at_ms"].is_null(),
        "Previous ended the retry"
    );

    started(&mut rig.engine).await;
    assert!(
        health_row(&rig.registry)["open_failures"].is_null(),
        "a started song clears the row at once"
    );
}

/// ROZHODNUTÉ 6029773698: the row says whether the pending retry was armed
/// while the playlist was SP-program's source; only then does the Player's
/// badge claim the program. A ▶ off air backs off too (the state machine's
/// `Playing` + `VideoError` reads no scene), and its retry is not on program.
#[tokio::test]
async fn a_retry_armed_off_program_says_so_on_the_row() {
    let mut rig = rig().await;
    heartbeat(&mut rig.engine); // the row exists
    start(&mut rig.engine).await; // a ▶, nothing on air
    for _ in 0..3 {
        fail(&mut rig.engine).await;
    }
    assert!(
        pending_id(&rig.engine).is_some(),
        "a ▶ off air backs off too"
    );
    let failures = &health_row(&rig.registry)["open_failures"];
    assert!(failures["retry_at_ms"].is_i64(), "{failures}");
    assert_eq!(
        failures["on_program"], false,
        "not SP-program's source: {failures}"
    );
}

/// The same on program: SP-program's source, its ON handled. The flag holds
/// while that retry is pending; the cut off program ends the retry, and the
/// claim with it.
#[tokio::test]
async fn a_retry_armed_on_program_says_so_until_it_ends() {
    let mut rig = rig().await;
    heartbeat(&mut rig.engine); // the row exists
    rig.engine.put_on_air_for_test(PID);
    rig.engine.handle_scene_change(PID, true).await; // the 1st Play
    for _ in 0..3 {
        fail(&mut rig.engine).await;
    }
    assert!(
        pending_id(&rig.engine).is_some(),
        "the 3rd failure armed a retry"
    );
    let failures = &health_row(&rig.registry)["open_failures"];
    assert!(failures["retry_at_ms"].is_i64(), "{failures}");
    assert_eq!(
        failures["on_program"], true,
        "SP-program's source: {failures}"
    );

    rig.engine.handle_scene_change(PID, false).await; // cut off program
    let failures = &health_row(&rig.registry)["open_failures"];
    assert!(
        failures["retry_at_ms"].is_null(),
        "the retry ended: {failures}"
    );
    assert_eq!(
        failures["on_program"], false,
        "no retry pending, no program claimed: {failures}"
    );
}
