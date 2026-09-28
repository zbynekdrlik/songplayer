//! Release 0.68.0 blockers from the cross-lane review of PR #220 (design
//! record 5863318980, #215 × #217): a playlist HELD off program through a
//! transition (#215's scene-off hold, `scene_off.rs`) has no side effects on
//! the wall. Its song's end, a failure or a skip pause it at once instead of
//! starting a song off program; a title timer writes the shared `#sp-title`
//! clip only while its scene is on program; a pause cancels the song's title
//! timers; and a scene back on program inside the hold keeps the song's
//! lyrics and resumes its wall lines.
//!
//! The hold is real: a program bus holds `OUT` through a cut to `IN` placed
//! a minute ahead on the live clock, so its re-check is never awaited. Every
//! Play calls `begin_play`, which clears the song's title clock: a clock
//! still there proves no Play was sent (on every platform; the Windows test
//! pipeline never answers a Play).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use sp_core::playback::PlaybackMode;
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::pipeline::PipelineEvent;
use super::program_bus::ProgramBus;
use super::program_transition::{SpecSource, TransitionSpec};
use super::state::{PlayEvent, PlayState};
use super::title::TitleClock;
use super::wallclock::utc_now_100ns;
use super::{PlaybackEngine, PlaybackEngineConfig};
use crate::lyrics::renderer::LyricsState;
use crate::resolume::ResolumeCommand;

/// The playlist whose OBS scene leaves program, and the one cut to.
const OUT: i64 = 7;
const IN: i64 = 8;
/// OUT plays `SONG`; `NEXT` is the song a selection would start.
const SONG: i64 = 42;
const NEXT: i64 = 43;
const SONG_MS: u64 = 180_000;
/// OUT's position when its scene leaves program.
const AT_MS: u64 = 60_000;

struct Rig {
    engine: PlaybackEngine,
    resolume: mpsc::Receiver<ResolumeCommand>,
    ws: broadcast::Receiver<ServerMsg>,
}

/// An engine with OUT (songs 42 and 43) and IN (song 44), both normalized.
async fn rig() -> Rig {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for (playlist_id, videos) in [(OUT, &[SONG, NEXT][..]), (IN, &[44][..])] {
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (?, 'p', 'u', ?, 1)",
        )
        .bind(playlist_id)
        .bind(format!("SP-{playlist_id}"))
        .execute(&pool)
        .await
        .unwrap();
        for &video_id in videos {
            sqlx::query(
                "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, \
                 file_path, audio_file_path) VALUES (?, ?, ?, ?, 'Artist', 1, ?, ?)",
            )
            .bind(video_id)
            .bind(playlist_id)
            .bind(format!("yt{video_id}"))
            .bind(format!("Song {video_id}"))
            .bind(format!("/tmp/sp-hold-{video_id}_video.mp4"))
            .bind(format!("/tmp/sp-hold-{video_id}_audio.flac"))
            .execute(&pool)
            .await
            .unwrap();
        }
    }
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, resolume) = mpsc::channel(64);
    let (ws_tx, ws) = broadcast::channel::<ServerMsg>(256);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache-hold"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    });
    engine.ensure_pipeline(OUT, "SP-7");
    engine.ensure_pipeline(IN, "SP-8");
    Rig {
        engine,
        resolume,
        ws,
    }
}

/// OUT plays `SONG` on program, `AT_MS` in, with its title clock (due now,
/// hiding an hour later) and no title timer armed.
fn playing(engine: &mut PlaybackEngine) -> TitleClock {
    let now = tokio::time::Instant::now();
    let clock = TitleClock {
        video_id: SONG,
        show_at: now,
        hide_at: Some(now + Duration::from_secs(3600)),
    };
    let pp = engine.pipelines.get_mut(&OUT).expect("OUT's pipeline");
    pp.state = PlayState::Playing { video_id: SONG };
    pp.current_video_id = Some(SONG);
    pp.title_clock = Some(clock);
    pp.cached_position_ms = AT_MS;
    pp.cached_duration_ms = SONG_MS;
    pp.scene_active.store(true, Ordering::Release);
    clock
}

/// cg OBS switches from OUT's scene to IN's: the program bus cuts to IN
/// (its window a minute ahead of now, so OUT's hold is never over within the
/// test) and OUT's scene leaves program, held through the transition.
async fn hold(rig: &mut Rig) -> Arc<ProgramBus> {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(OUT, None);
    assert!(bus.set_transition(TransitionSpec::fade(300, SpecSource::Obs)));
    assert!(rig.engine.program.set(bus.clone()).is_ok());
    let status = bus.cut(IN, utc_now_100ns() + 60 * 10_000_000, None);
    assert!(status.cut_boundary_100ns.is_some(), "the cut is recorded");
    rig.engine.handle_scene_change(OUT, false).await;
    assert_eq!(
        out(&rig.engine).state,
        PlayState::Playing { video_id: SONG },
        "OUT keeps playing through the transition (#215)"
    );
    sent(&mut rig.resolume); // the scene-off's own hides
    bus
}

fn out(engine: &PlaybackEngine) -> &super::PlaylistPipeline {
    engine.pipelines.get(&OUT).expect("OUT's pipeline")
}

/// Whether OUT has a (show, hide) title timer armed.
fn timers(engine: &PlaybackEngine) -> (bool, bool) {
    let pp = out(engine);
    (pp.title_show_abort.is_some(), pp.title_hide_abort.is_some())
}

/// Every command waiting on the Resolume channel.
fn sent(rx: &mut mpsc::Receiver<ResolumeCommand>) -> Vec<ResolumeCommand> {
    let mut cmds = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        cmds.push(cmd);
    }
    cmds
}

/// The `line_en` of every karaoke `LyricsUpdate` waiting on the WS channel.
fn lyrics_updates(rx: &mut broadcast::Receiver<ServerMsg>) -> Vec<Option<String>> {
    let mut lines = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let ServerMsg::LyricsUpdate { line_en, .. } = msg {
            lines.push(line_en);
        }
    }
    lines
}

/// The title of every `Resync` in `cmds` (`None` = a resync naming no title).
fn resyncs(cmds: &[ResolumeCommand]) -> Vec<Option<String>> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            ResolumeCommand::Resync { title } => Some(title.clone()),
            _ => None,
        })
        .collect()
}

/// The `en` text of every `ShowSubtitles` in `cmds`.
fn subtitle_lines(cmds: &[ResolumeCommand]) -> Vec<String> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            ResolumeCommand::ShowSubtitles { en, .. } => Some(en.clone()),
            _ => None,
        })
        .collect()
}

async fn plays_recorded(engine: &PlaybackEngine) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM play_history WHERE playlist_id = ?")
        .bind(OUT)
        .fetch_one(&engine.pool)
        .await
        .unwrap()
}

/// What happens to OUT: its song ends (Continuous, Loop or Single), it
/// fails, or the operator skips it.
#[derive(Clone, Copy, Debug)]
enum Happens {
    Ended,
    Looped,
    EndedSingle,
    Failed,
    Skipped,
}

impl Happens {
    /// The playback mode OUT is in when it happens.
    fn mode(self) -> Option<PlaybackMode> {
        match self {
            Happens::Looped => Some(PlaybackMode::Loop),
            Happens::EndedSingle => Some(PlaybackMode::Single),
            _ => None,
        }
    }
}

async fn happens(engine: &mut PlaybackEngine, what: Happens) {
    match what {
        Happens::Ended | Happens::Looped | Happens::EndedSingle => {
            engine
                .handle_pipeline_event(OUT, PipelineEvent::Ended)
                .await
        }
        Happens::Failed => {
            engine
                .handle_pipeline_event(OUT, PipelineEvent::Error("decode failed".into()))
                .await
        }
        Happens::Skipped => engine.handle_command(OUT, PlayEvent::Skip).await,
    }
}

/// Design record 5863318980 item 1 (🔴): the held OUT's song ended inside
/// the hold, `VideoEnded` → `SelectAndPlay` started the next song off
/// program (`record_play` of an unaired song), its `Started` armed its title
/// timers, and 3.5 s before ITS end the hide timer faded out IN's live
/// title; the ungated song-end clear blanked IN's `#sp-subs` too. The same
/// for a Loop replay, a failure and a skip (Single's black too). Now OUT
/// pauses at once, exactly like the hold's own end: no Play, no play
/// recorded, its title timers cancelled, nothing sent to the wall. The
/// pause's resume point is where OUT was; for a song that ENDED that is its
/// end (an accepted residual: a later ▶ plays its last moment, then the next
/// song).
#[tokio::test]
async fn a_held_playlist_pauses_instead_of_starting_a_song_off_program() {
    for what in [
        Happens::Ended,
        Happens::Looped,
        Happens::EndedSingle,
        Happens::Failed,
        Happens::Skipped,
    ] {
        let mut rig = rig().await;
        if let Some(mode) = what.mode() {
            rig.engine
                .handle_command(OUT, PlayEvent::SetMode(mode))
                .await;
        }
        let clock = playing(&mut rig.engine);
        let _bus = hold(&mut rig).await;
        // A timer armed inside the hold (a `Started` that came in it).
        rig.engine
            .arm_title_timers(OUT, tokio::time::Instant::now());
        assert_eq!(
            timers(&rig.engine),
            (false, true),
            "{what:?}: the hide timer"
        );

        happens(&mut rig.engine, what).await;

        let pp = out(&rig.engine);
        assert_eq!(
            pp.state,
            PlayState::WaitingForScene,
            "{what:?}: OUT is paused, as at the hold's end"
        );
        assert_eq!(
            pp.paused_at,
            Some((SONG, AT_MS)),
            "{what:?}: paused where it was"
        );
        assert_eq!(
            pp.current_video_id,
            Some(SONG),
            "{what:?}: no song selected"
        );
        assert_eq!(
            pp.title_clock,
            Some(clock),
            "{what:?}: no Play was sent (every Play clears the clock)"
        );
        assert_eq!(
            timers(&rig.engine),
            (false, false),
            "{what:?}: the pause cancelled the timer"
        );
        assert_eq!(
            plays_recorded(&rig.engine).await,
            0,
            "{what:?}: no play of an unaired song recorded"
        );
        let cmds = sent(&mut rig.resolume);
        assert!(
            cmds.is_empty(),
            "{what:?}: nothing reaches the wall IN is on, got {cmds:?}"
        );
    }
}

/// The control: on program (no hold), a song's end still selects and starts
/// the next song and records its play, and the song-end clear still goes
/// out. What the held case checks is therefore the hold, not the rig.
#[tokio::test]
async fn on_program_a_song_s_end_still_starts_the_next_song() {
    let mut rig = rig().await;
    playing(&mut rig.engine);

    rig.engine
        .handle_pipeline_event(OUT, PipelineEvent::Ended)
        .await;

    let pp = out(&rig.engine);
    assert!(
        matches!(pp.state, PlayState::Playing { .. }),
        "the next song plays, got {:?}",
        pp.state
    );
    assert_eq!(pp.title_clock, None, "a Play cleared the clock");
    assert_eq!(plays_recorded(&rig.engine).await, 1, "its play is recorded");
    assert!(
        sent(&mut rig.resolume)
            .iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideSubtitles)),
        "the song-end clear still goes out on program"
    );
}

/// Item 1c: a pause (the dashboard's, or the end of a hold) cancels the
/// song's title timers. A paused song's hide timer used to fire at the
/// song's planned end and take down whatever title was on the wall then.
#[tokio::test]
async fn a_pause_cancels_the_song_s_title_timers() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    let now = tokio::time::Instant::now();
    let hour = Duration::from_secs(3600);
    rig.engine.pipelines.get_mut(&OUT).unwrap().title_clock = Some(TitleClock {
        video_id: SONG,
        show_at: now + hour,
        hide_at: Some(now + 2 * hour),
    });
    rig.engine.arm_title_timers(OUT, now);
    assert_eq!(timers(&rig.engine), (true, true), "both timers armed");

    rig.engine.handle_command(OUT, PlayEvent::SceneOff).await; // the dashboard's Pause

    assert_eq!(out(&rig.engine).state, PlayState::WaitingForScene);
    assert_eq!(
        timers(&rig.engine),
        (false, false),
        "a paused song's timers are cancelled"
    );
}

/// Arm OUT's hide timer 50 ms ahead (its show point is past), with OUT's
/// scene `on_program` or not, wait until the timer has run (bounded), and
/// return what it sent to the wall.
async fn hide_timer_fires(on_program: bool) -> Vec<ResolumeCommand> {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    let now = tokio::time::Instant::now();
    let pp = rig.engine.pipelines.get_mut(&OUT).unwrap();
    pp.title_clock = Some(TitleClock {
        video_id: SONG,
        show_at: now,
        hide_at: Some(now + Duration::from_millis(50)),
    });
    pp.scene_active.store(on_program, Ordering::Release);
    rig.engine.arm_title_timers(OUT, now);
    assert_eq!(timers(&rig.engine), (false, true), "the hide timer only");
    let timer = out(&rig.engine).title_hide_abort.clone().unwrap();
    for _ in 0..400 {
        if timer.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(timer.is_finished(), "the hide timer ran");
    sent(&mut rig.resolume)
}

/// Item 1a: the show timer re-checked the scene when it fired, the hide
/// timer did not. A hide timer of a playlist off program (held through a
/// transition, or its scene gone after the timer was armed) faded out the
/// title of the playlist on program. Every title timer now writes the
/// shared clip only while its own scene is on program.
#[tokio::test]
async fn a_hide_timer_writes_the_wall_only_on_program() {
    let cmds = hide_timer_fires(false).await;
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideTitle)),
        "off program: no HideTitle, got {cmds:?}"
    );
    let cmds = hide_timer_fires(true).await;
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideTitle)),
        "on program: the title hides, got {cmds:?}"
    );
}

/// Three sentences: "alpha." 1–3 s, "beta." 4–6 s, "gamma." 7–9 s. The #217
/// display plan groups lines into sentences; the wall strips the period.
fn track() -> LyricsTrack {
    let line = |start_ms, end_ms, en: &str| LyricsLine {
        start_ms,
        end_ms,
        en: en.into(),
        sk: Some(format!("{en} sk")),
        words: None,
    };
    LyricsTrack {
        version: 22,
        source: "test".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![
            line(1000, 3000, "alpha."),
            line(4000, 6000, "beta."),
            line(7000, 9000, "gamma."),
        ],
    }
}

async fn position(engine: &mut PlaybackEngine, position_ms: u64) {
    engine
        .handle_pipeline_event(
            OUT,
            PipelineEvent::Position {
                position_ms,
                duration_ms: SONG_MS,
            },
        )
        .await;
}

/// Item 2 (🟡): the scene-off dropped the song's lyrics (`lyrics_state =
/// None`). A scene back on program inside the hold keeps the song playing
/// (`(Playing, SceneOn)` does nothing) and lyrics load only at `Started`,
/// so the rest of the song had no subtitles on the wall, the stage display
/// or the karaoke. Now the scene-off keeps them; while held, no line goes
/// anywhere (as before), not even a new one, and a scene back on program
/// sends the line at the next Position. (The Presenter sits behind the same
/// gate as the karaoke WS, `dispatch_lyrics_if_changed`.)
#[tokio::test]
async fn a_scene_back_on_inside_the_hold_keeps_the_lyrics_and_resumes_the_lines() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    rig.engine.pipelines.get_mut(&OUT).unwrap().lyrics_state = Some(LyricsState::new(track()));
    position(&mut rig.engine, 1500).await;
    assert_eq!(
        subtitle_lines(&sent(&mut rig.resolume)),
        ["alpha"],
        "on program the line goes to the wall"
    );
    assert_eq!(lyrics_updates(&mut rig.ws), [Some("alpha.".to_string())]);

    let _bus = hold(&mut rig).await;
    assert!(
        out(&rig.engine).lyrics_state.is_some(),
        "the scene-off keeps the song's lyrics through the hold"
    );
    lyrics_updates(&mut rig.ws);

    position(&mut rig.engine, 4500).await; // "beta": a new line
    assert!(
        subtitle_lines(&sent(&mut rig.resolume)).is_empty(),
        "held off program: no line to the wall"
    );
    assert!(
        lyrics_updates(&mut rig.ws).is_empty(),
        "held off program: no karaoke line either, as before"
    );

    rig.engine.handle_scene_change(OUT, true).await;
    sent(&mut rig.resolume); // the scene-on's title re-sync
    position(&mut rig.engine, 4600).await;
    assert_eq!(
        subtitle_lines(&sent(&mut rig.resolume)),
        ["beta"],
        "back on program: the line goes to the wall at the next Position"
    );
    assert_eq!(lyrics_updates(&mut rig.ws), [Some("beta.".to_string())]);
}

// -- the hold marker (`scene_off_due`) itself ------------------------------

/// Wait (bounded) until `task` has finished. The hold's re-check sleeps a
/// minute, so within the test only an abort finishes it.
async fn finished(task: &tokio::task::AbortHandle) -> bool {
    for _ in 0..400 {
        if task.is_finished() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// OUT's pending hold re-check (the hold marker).
/// Its id and its sleeping task.
fn re_check(engine: &PlaybackEngine) -> (u64, tokio::task::AbortHandle) {
    out(engine)
        .scene_off_due
        .clone()
        .expect("OUT is held: its re-check is pending")
}

/// Item 1b: the pause of a held playlist ends its hold. Its pending re-check
/// is cancelled, so the playlist is no longer held (a later song of its own
/// is not paused by a stale marker), and its lyrics go with the pause, as
/// the scene-off dropped them before the hold existed: a later scene-on
/// starts a new song and must not re-push the old song's lines before the
/// new `Started` loads its own.
#[tokio::test]
async fn the_pause_of_a_held_playlist_cancels_its_re_check_and_drops_its_lyrics() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    rig.engine.pipelines.get_mut(&OUT).unwrap().lyrics_state = Some(LyricsState::new(track()));
    let _bus = hold(&mut rig).await;
    let (_, due) = re_check(&rig.engine);

    rig.engine
        .handle_pipeline_event(OUT, PipelineEvent::Ended)
        .await;

    let pp = out(&rig.engine);
    assert!(pp.scene_off_due.is_none(), "no longer held");
    assert!(pp.lyrics_state.is_none(), "the pause drops the lyrics");
    assert!(finished(&due).await, "the pending re-check was cancelled");
}

/// A scene back on program ends the hold: nothing is pending, and the
/// playlist is an ordinary on-program one again, so its song's end starts
/// the next song as always. The scene-off cleared the wall's line and the
/// stage display moved on, so their dedup keys were reset for the resume.
#[tokio::test]
async fn a_scene_back_on_program_ends_the_hold() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    {
        let pp = rig.engine.pipelines.get_mut(&OUT).unwrap();
        pp.last_presenter_text = Some(("alpha".into(), "alfa".into()));
        pp.last_resolume_subtitles_signature = Some("show|alpha".into());
    }
    let _bus = hold(&mut rig).await;
    let pp = out(&rig.engine);
    assert_eq!(pp.last_presenter_text, None, "the stage display moved on");
    assert_eq!(
        pp.last_resolume_subtitles_signature, None,
        "the scene-off cleared the wall's line"
    );
    let (_, due) = re_check(&rig.engine);

    rig.engine.handle_scene_change(OUT, true).await;

    assert!(out(&rig.engine).scene_off_due.is_none(), "no longer held");
    assert!(finished(&due).await, "the pending re-check was cancelled");
    rig.engine
        .handle_pipeline_event(OUT, PipelineEvent::Ended)
        .await;
    assert!(
        matches!(out(&rig.engine).state, PlayState::Playing { .. }),
        "on program again: the next song starts"
    );
    assert_eq!(plays_recorded(&rig.engine).await, 1);
}

/// A hold that asks again (its window is not over at the re-check) replaces
/// its pending re-check, never leaving a second one behind.
#[tokio::test]
async fn a_newer_hold_supersedes_the_pending_re_check() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    let _bus = hold(&mut rig).await;
    let (first_id, first) = re_check(&rig.engine);

    rig.engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue(first_id))
        .await;

    assert_eq!(
        out(&rig.engine).state,
        PlayState::Playing { video_id: SONG },
        "still held: the window is a minute away"
    );
    let (second_id, second) = re_check(&rig.engine);
    assert_ne!(second_id, first_id, "a new re-check replaced it");
    assert!(finished(&first).await, "the superseded one was cancelled");
    assert!(
        !second.is_finished(),
        "a new re-check is pending, a minute away"
    );

    // Review round 1: the superseded hold's re-check, already queued (an
    // A→B→A→B inside one hold), is stale. Taken as the newer hold's, it
    // skipped that hold's `CUT_SETTLE`; it is ignored.
    rig.engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue(first_id))
        .await;
    assert_eq!(
        re_check(&rig.engine).0,
        second_id,
        "the newer hold's re-check is still the pending one"
    );
    assert!(!second.is_finished(), "and it still runs");
}

// -- review round 1: nothing else of a held or paused song reaches the wall --

async fn started(engine: &mut PlaybackEngine) {
    engine
        .handle_pipeline_event(
            OUT,
            PipelineEvent::Started {
                duration_ms: SONG_MS,
            },
        )
        .await;
}

/// Review round 1 (🔴): a song whose Play went out on program (its song
/// ended, or a skip) has its `Started` INSIDE the hold when the cut lands in
/// its pre-roll. A song without lyrics cleared the subtitle clips and the
/// stage display at its `Started`, blanking the line of the playlist now on
/// program. A held playlist clears nothing.
#[tokio::test]
async fn a_started_inside_the_hold_clears_nothing() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    let _bus = hold(&mut rig).await;
    lyrics_updates(&mut rig.ws);

    started(&mut rig.engine).await;

    let cmds = sent(&mut rig.resolume);
    assert!(
        cmds.is_empty(),
        "nothing reaches the wall IN is on, got {cmds:?}"
    );
    assert!(
        lyrics_updates(&mut rig.ws).is_empty(),
        "no karaoke clear either"
    );
}

/// Review round 1 (🟡): a playlist off program (played by hand, no hold)
/// still cleared the shared subtitle clips at its song's end, blanking the
/// on-program playlist's line. It stays off them, as its lines do
/// (`dispatch_lyrics_if_changed`); its own karaoke clear still goes out, and
/// its end still starts the next song.
#[tokio::test]
async fn off_program_a_song_s_end_leaves_the_subtitle_clips_alone() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    rig.engine.set_scene_active_for_test(OUT, false);
    lyrics_updates(&mut rig.ws);

    rig.engine
        .handle_pipeline_event(OUT, PipelineEvent::Ended)
        .await;

    assert!(
        matches!(out(&rig.engine).state, PlayState::Playing { .. }),
        "not held: the next song starts"
    );
    let cmds = sent(&mut rig.resolume);
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideSubtitles)),
        "the on-program line stays, got {cmds:?}"
    );
    assert_eq!(
        lyrics_updates(&mut rig.ws),
        [None::<String>],
        "its karaoke clear still goes out"
    );
}

/// Review rounds 1-2 (🔵): with its timers cancelled and its lyrics
/// dropped, a song paused on program kept its title and its line up until
/// some re-sync (a recovery, another Play) took them down. The pause clears
/// both at once: a paused song's title and line are not due. And a
/// `Started` the pause overtook (its Play went out just before) shows
/// nothing: no clock, no timer, no clear. The resume's `Started` does all of
/// it.
#[tokio::test]
async fn a_pause_on_program_takes_the_title_and_line_down_and_a_late_started_shows_nothing() {
    let mut rig = rig().await;
    let clock = playing(&mut rig.engine);
    rig.engine.handle_command(OUT, PlayEvent::SceneOff).await; // the dashboard's Pause
    let cmds = sent(&mut rig.resolume);
    assert_eq!(
        resyncs(&cmds),
        [None::<String>],
        "the paused song's title goes down"
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, ResolumeCommand::HideSubtitles)),
        "and its line, got {cmds:?}"
    );
    assert_eq!(
        lyrics_updates(&mut rig.ws),
        [None::<String>],
        "its karaoke clear too"
    );

    started(&mut rig.engine).await;

    assert_eq!(out(&rig.engine).title_clock, Some(clock), "no new clock");
    assert_eq!(timers(&rig.engine), (false, false), "no timer");
    let cmds = sent(&mut rig.resolume);
    assert!(cmds.is_empty(), "nothing to the wall, got {cmds:?}");
    assert!(lyrics_updates(&mut rig.ws).is_empty(), "no karaoke clear");
}

/// Review round 1 (🔵): the operator's pick inside the hold (a PlayVideo, a
/// Previous, a ▶ resume) started a song the hold kept muted, and the hold's
/// re-check paused it a moment later. A pick ends the hold: the song plays
/// like any song played off program by hand, and its end starts the next.
#[tokio::test]
async fn an_operator_pick_inside_the_hold_ends_it() {
    for pick in ["play video", "previous"] {
        let mut rig = rig().await;
        playing(&mut rig.engine);
        let _bus = hold(&mut rig).await;
        let (due_id, due) = re_check(&rig.engine);
        lyrics_updates(&mut rig.ws);
        if pick == "play video" {
            rig.engine.handle_play_video(OUT, NEXT, None).await;
            // Review round 3: the hold ends BEFORE the PlayVideo's clear, so
            // the old song's karaoke line goes, as for any song played off
            // program by hand (held, the clear was skipped for good).
            assert_eq!(
                lyrics_updates(&mut rig.ws),
                [None::<String>],
                "the PlayVideo clear"
            );
        } else {
            rig.engine
                .pipelines
                .get_mut(&OUT)
                .unwrap()
                .history
                .push_back(NEXT);
            rig.engine.handle_previous(OUT).await;
        }

        assert!(
            out(&rig.engine).scene_off_due.is_none(),
            "{pick}: no longer held"
        );
        assert!(finished(&due).await, "{pick}: the re-check is cancelled");
        // Review round 2: the re-check may already be queued when the pick
        // comes (the engine's select! is unbiased). It is stale: no hold is
        // pending, so it neither holds the pick again nor pauses it.
        rig.engine
            .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue(due_id))
            .await;
        assert!(
            out(&rig.engine).scene_off_due.is_none(),
            "{pick}: a stale re-check holds nothing"
        );
        rig.engine
            .handle_pipeline_event(OUT, PipelineEvent::Ended)
            .await;
        assert!(
            matches!(out(&rig.engine).state, PlayState::Playing { .. }),
            "{pick}: its end starts the next song"
        );
    }
}

/// Review round 1 (🔵): a Play kept the last song's lyrics and position
/// until the new `Started`. A recovery in between re-pushed the old song's
/// line, and a pause there recorded the old song's position for the new one
/// (a resume then started it there).
#[tokio::test]
async fn a_play_drops_the_last_song_s_lyrics_and_position() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    {
        let pp = rig.engine.pipelines.get_mut(&OUT).unwrap();
        pp.lyrics_state = Some(LyricsState::new(track()));
        pp.cached_position_ms = 1500; // inside "alpha"
    }

    rig.engine.handle_command(OUT, PlayEvent::Skip).await;

    let pp = out(&rig.engine);
    assert!(pp.lyrics_state.is_none(), "the old song's lyrics are gone");
    assert_eq!(pp.cached_position_ms, 0, "the new song starts at 0");
    sent(&mut rig.resolume);
    rig.engine.handle_resolume_recovery("127.0.0.1").await;
    assert!(
        subtitle_lines(&sent(&mut rig.resolume)).is_empty(),
        "a recovery before the new `Started` pushes no old line"
    );
    rig.engine.handle_play_video(OUT, SONG, Some(30_000)).await;
    assert_eq!(
        out(&rig.engine).cached_position_ms,
        30_000,
        "a resume starts where it resumes"
    );
}

/// Review round 2 (🔵): a pause's resume point outlived the song it was
/// taken in. A pause (the hold's end, the dashboard's), then a scene-on that
/// starts the next song, then a ▶ while that song is held again resumed the
/// OLD song over the new one. Every Play makes the snapshot obsolete.
#[tokio::test]
async fn a_play_drops_the_last_pause_s_resume_point() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    rig.engine.handle_command(OUT, PlayEvent::SceneOff).await; // paused
    assert_eq!(out(&rig.engine).paused_at, Some((SONG, AT_MS)));

    rig.engine.handle_scene_change(OUT, true).await; // starts a song

    let pp = out(&rig.engine);
    assert!(
        matches!(pp.state, PlayState::Playing { .. }),
        "a song plays"
    );
    assert_eq!(pp.paused_at, None, "the old resume point is gone");
}

/// Review round 4 (🔵): a Play resets the position to its start
/// (`begin_play`), but the OLD song's last report can still arrive after it
/// (the pipeline reports before it reads the Play; `Position` names no
/// video). It moved the new song's pause point until the new song's first
/// report, so a pause there resumed the new song at the old one's position.
/// A report before the new song's `Started` (a Play clears the clock) is the
/// old song's.
#[tokio::test]
async fn the_old_song_s_late_report_does_not_move_the_new_song_s_pause_point() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    rig.engine.handle_command(OUT, PlayEvent::Skip).await; // a Play
    let playing_now = out(&rig.engine).current_video_id.expect("a song");

    position(&mut rig.engine, 50_000).await; // the old song's last report
    rig.engine.handle_command(OUT, PlayEvent::SceneOff).await; // a pause

    assert_eq!(
        out(&rig.engine).paused_at,
        Some((playing_now, 0)),
        "the new song pauses at its start"
    );
}

/// Review round 5 (🔵): the "a pause overtook the Play" guard read the state
/// (`WaitingForScene`), which a FAILED selection leaves too: a Skip between
/// a Play and its `Started` whose selection starts nothing sends no Pause, so
/// the pipeline keeps playing the song, and its `Started` was swallowed (no
/// clock, no timers, no lyrics for the whole song). The guard is now "a
/// pause came after the last Play" (`paused_at`, which every Play clears).
#[tokio::test]
async fn a_started_after_a_failed_selection_still_starts_the_song() {
    let mut rig = rig().await;
    playing(&mut rig.engine);
    rig.engine.handle_command(OUT, PlayEvent::Skip).await; // a Play
    sqlx::query("DELETE FROM videos WHERE playlist_id = ?")
        .bind(OUT)
        .execute(&rig.engine.pool)
        .await
        .unwrap();
    rig.engine.handle_command(OUT, PlayEvent::Skip).await; // selects nothing
    assert_eq!(out(&rig.engine).state, PlayState::WaitingForScene);

    started(&mut rig.engine).await; // the first Play's song plays on

    assert!(
        out(&rig.engine).title_clock.is_some(),
        "its Started fixed the song's title clock"
    );
}

/// Review round 6 (🔵): a ▶ took the pause's resume point BEFORE looking the
/// song up. When the lookup failed (a transient DB error, or the row gone
/// with its playlist; the test deletes it), no Play went out and the
/// pipeline stayed paused, but the resume point was gone: a queued `Started`
/// of the Play the pause overtook then armed the title timers of a paused
/// song. A failed resume keeps the resume point (a later ▶ retries it).
#[tokio::test]
async fn a_failed_resume_keeps_the_pause_s_resume_point() {
    let mut rig = rig().await;
    let clock = playing(&mut rig.engine);
    rig.engine.handle_command(OUT, PlayEvent::SceneOff).await; // paused
    sqlx::query("DELETE FROM videos WHERE playlist_id = ?")
        .bind(OUT)
        .execute(&rig.engine.pool)
        .await
        .unwrap();

    rig.engine.handle_engine_play(OUT).await; // ▶: the song lookup fails

    assert_eq!(
        out(&rig.engine).paused_at,
        Some((SONG, AT_MS)),
        "still paused: the resume point stays"
    );
    started(&mut rig.engine).await; // the overtaken Play's Started
    assert_eq!(out(&rig.engine).title_clock, Some(clock), "no new clock");
    assert_eq!(timers(&rig.engine), (false, false), "no timer");
}
