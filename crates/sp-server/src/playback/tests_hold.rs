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
    bus.select_initial(OUT);
    assert!(bus.set_transition(TransitionSpec::fade(300, SpecSource::Obs)));
    assert!(rig.engine.program.set(bus.clone()).is_ok());
    let status = bus.cut(IN, utc_now_100ns() + 60 * 10_000_000);
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

/// What happens to OUT: its song ends (Continuous or Loop), it fails, or
/// the operator skips it.
#[derive(Clone, Copy, Debug)]
enum Happens {
    Ended,
    Looped,
    Failed,
    Skipped,
}

async fn happens(engine: &mut PlaybackEngine, what: Happens) {
    match what {
        Happens::Ended | Happens::Looped => {
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
/// for a Loop replay, a failure and a skip. Now OUT pauses at once, exactly
/// like the hold's own end: no Play, no play recorded, no timer, nothing sent
/// to the wall.
#[tokio::test]
async fn a_held_playlist_pauses_instead_of_starting_a_song_off_program() {
    for what in [
        Happens::Ended,
        Happens::Looped,
        Happens::Failed,
        Happens::Skipped,
    ] {
        let mut rig = rig().await;
        if let Happens::Looped = what {
            rig.engine
                .handle_command(OUT, PlayEvent::SetMode(PlaybackMode::Loop))
                .await;
        }
        let clock = playing(&mut rig.engine);
        let _bus = hold(&mut rig).await;

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
        assert_eq!(timers(&rig.engine), (false, false), "{what:?}: no timer");
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

/// Three lines: "alpha" 1–3 s, "beta" 4–6 s, "gamma" 7–9 s.
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
            line(1000, 3000, "alpha"),
            line(4000, 6000, "beta"),
            line(7000, 9000, "gamma"),
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
/// anywhere (as before), and a scene back on program re-sends the line at
/// once, although the wall already had it before the scene-off cleared it.
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
    assert_eq!(lyrics_updates(&mut rig.ws), [Some("alpha".to_string())]);

    let _bus = hold(&mut rig).await;
    assert!(
        out(&rig.engine).lyrics_state.is_some(),
        "the scene-off keeps the song's lyrics through the hold"
    );
    lyrics_updates(&mut rig.ws);

    position(&mut rig.engine, 2000).await;
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
    position(&mut rig.engine, 2200).await;
    assert_eq!(
        subtitle_lines(&sent(&mut rig.resolume)),
        ["alpha"],
        "back on program: the line goes to the wall again at the next Position"
    );
}
