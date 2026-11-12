//! #232 unit 2: the live Live Translate probe — the clip it picks and cuts,
//! the read-back of the dub, the verdict, and every refusal before anything is
//! sent. The child itself (Python + Gemini Live) runs only on the box; its
//! argv is pinned in `child.rs`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::SqlitePool;

use super::*;
use crate::dabing::child::DubSessionStats;
use crate::dabing::subtitles::DubTranscripts;

// ---- pure pieces -----------------------------------------------------------

#[test]
fn the_window_starts_at_the_first_line_else_past_the_intro() {
    assert_eq!(window_start(Some(12_345)), 12_345);
    assert_eq!(window_start(Some(0)), 0);
    assert_eq!(window_start(None), 30_000);
    assert_eq!(NO_LINE_START_MS, 30_000);
    assert_eq!(CLIP_MS, 20_000);
}

fn strings(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_clip_is_twenty_seconds_of_audio_from_its_start_as_a_wav() {
    let args = cut_args(
        Path::new("/c/a_audio.flac"),
        61_234,
        Path::new("/t/clip.wav"),
    );
    assert_eq!(
        strings(args),
        [
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-ss",
            "61.234",
            "-t",
            "20.000",
            "-i",
            "/c/a_audio.flac",
            "-vn",
            "-c:a",
            "pcm_s16le",
            "-y",
            "/t/clip.wav",
        ]
    );
    // A whole second and a start under one second keep their digits.
    let args = strings(cut_args(Path::new("i"), 5_007, Path::new("o")));
    assert_eq!(args[5], "5.007");
    let args = strings(cut_args(Path::new("i"), 999, Path::new("o")));
    assert_eq!(args[5], "0.999");
}

#[test]
fn the_dub_is_read_back_as_mono_float_at_sixteen_khz_on_stdout() {
    assert_eq!(
        strings(measure_args(Path::new("/t/probe_dub.flac"))),
        [
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-i",
            "/t/probe_dub.flac",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "f32le",
            "pipe:1",
        ]
    );
}

fn pcm(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// The length comes from the sample count at 16 kHz; the level is the RMS of
/// every sample (a constant ±0.5 is −6.02 dBFS); a trailing partial sample is
/// ignored.
#[test]
fn the_level_of_the_dub_is_its_length_and_rms() {
    let half: Vec<f32> = (0..24_000)
        .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
        .collect();
    let mut bytes = pcm(&half);
    bytes.extend_from_slice(&[1, 2, 3]);
    let level = level_of(&bytes);
    assert_eq!(level.dub_ms, 1_500);
    assert!(
        (level.rms_dbfs - -6.0206).abs() < 0.01,
        "{}",
        level.rms_dbfs
    );

    let silent = level_of(&pcm(&[0.0; 8_000]));
    assert_eq!(silent.dub_ms, 500);
    assert_eq!(silent.rms_dbfs, sp_core::audio_level::SILENCE_FLOOR_DBFS);

    let empty = level_of(&[]);
    assert_eq!(empty.dub_ms, 0);
}

/// A transcripts JSON in the child's one-chunk shape (`build_transcripts`).
fn transcripts(sk: &[(u64, &str)]) -> DubTranscripts {
    let sk_timed: Vec<Value> = sk
        .iter()
        .map(|(t, text)| json!({"t_ms": t, "text": text}))
        .collect();
    serde_json::from_value(json!({
        "engine": "gemini-live-translate",
        "target_lang": "sk",
        "chunks": [{
            "index": 0, "start_ms": 0, "end_ms": 20_000, "at_ms": 0, "tempo": 1.0,
            "en": "", "en_timed": [], "sk": "", "sk_timed": sk_timed,
        }],
    }))
    .unwrap()
}

/// The Slovak lines are the subtitle builder's own (one per sentence); the
/// sample is the first one.
#[test]
fn the_slovak_lines_are_the_subtitle_builders_lines() {
    let t = transcripts(&[
        (2_000, " Pane, ty si"),
        (3_000, " dobrý."),
        (6_000, " Chválime ťa."),
    ]);
    let (lines, sample) = sk_lines_of(&t);
    assert_eq!(lines, 2);
    assert_eq!(sample, "Pane, ty si dobrý.");
}

#[test]
fn a_long_first_line_is_cut_for_the_sample_and_no_line_is_none() {
    let long = format!(" {}.", "slovo ".repeat(19).trim_end());
    let (lines, sample) = sk_lines_of(&transcripts(&[(2_000, &long)]));
    assert_eq!(lines, 1);
    assert_eq!(sample, long.trim());
    let long = format!(" {}.", "dlhéslovo ".repeat(19).trim_end());
    let (_, sample) = sk_lines_of(&transcripts(&[(2_000, &long)]));
    assert_eq!(sample.chars().count(), 120);

    assert_eq!(sk_lines_of(&transcripts(&[])), (0, String::new()));
    assert_eq!(
        sk_lines_of(&transcripts(&[(2_000, "  ")])),
        (0, String::new())
    );
}

#[test]
fn the_verdict_needs_sound_and_a_slovak_line() {
    assert_eq!(verdict(-20.0, 3), None);
    // The floor itself still holds sound.
    assert_eq!(verdict(MIN_DUB_RMS_DBFS, 1), None);
    assert_eq!(
        verdict(-60.0, 1).as_deref(),
        Some("the dub is silent: -60.0 dBFS, under the -45 dBFS floor")
    );
    assert_eq!(
        verdict(-20.0, 0).as_deref(),
        Some("the session gave no Slovak transcript line")
    );
    assert_eq!(
        verdict(-180.0, 0).as_deref(),
        Some(
            "the dub is silent: -180.0 dBFS, under the -45 dBFS floor; \
             the session gave no Slovak transcript line"
        )
    );
}

fn clip() -> ProbeClip {
    ProbeClip {
        youtube_id: "fffffffffff".to_string(),
        input: PathBuf::from("/c/x_audio.flac"),
        source: ClipSource::Audio,
        start_ms: 9_000,
    }
}

fn session() -> DubSessionStats {
    serde_json::from_value(json!({"connections": 1, "reconnects": 0, "latency_ms": 3100})).unwrap()
}

/// A finished dub with sound and a line passes, with everything read back on
/// the answer.
#[test]
fn a_dub_with_sound_and_a_line_passes() {
    let base = DubProbeReport::new("m", "speaker").with_clip(&clip());
    let read = DubRead {
        session: session(),
        level: DubLevel {
            dub_ms: 21_500,
            rms_dbfs: -22.5,
        },
        transcripts: transcripts(&[(4_000, " Chválime ťa.")]),
    };
    let report = finish(base, Ok(read), 31_000, &["k-secret".to_string()]);
    assert_eq!(
        report,
        DubProbeReport {
            ok: true,
            model: "m".into(),
            voice: "speaker".into(),
            youtube_id: Some("fffffffffff".into()),
            start_ms: Some(9_000),
            clip_ms: 20_000,
            source: Some(ClipSource::Audio),
            elapsed_ms: 31_000,
            dub_ms: 21_500,
            dub_rms_dbfs: Some(-22.5),
            sk_lines: 1,
            sample: "Chválime ťa.".into(),
            session: Some(session()),
            error: None,
        }
    );
}

#[test]
fn a_silent_dub_fails_with_its_level() {
    let read = DubRead {
        session: session(),
        level: DubLevel {
            dub_ms: 20_000,
            rms_dbfs: -90.0,
        },
        transcripts: transcripts(&[(4_000, " Chválime ťa.")]),
    };
    let report = finish(DubProbeReport::new("m", "v"), Ok(read), 1, &[]);
    assert!(!report.ok);
    assert_eq!(report.dub_rms_dbfs, Some(-90.0));
    assert_eq!(report.sk_lines, 1);
    assert!(
        report
            .error
            .unwrap()
            .starts_with("the dub is silent: -90.0 dBFS")
    );
}

/// A failed child: its multi-line output tail on one line, the key redacted
/// (never answered), nothing read back.
#[test]
fn a_failed_session_never_answers_the_key() {
    let error = "the dub session failed: exited with status 1; output tail:\n\
                 Live reconnect refused: key=AIzaSECRETKEY123 invalid\n  bye";
    let keys = vec!["AIzaSECRETKEY123".to_string()];
    let report = finish(
        DubProbeReport::new("m", "v").with_clip(&clip()),
        Err(error.to_string()),
        7,
        &keys,
    );
    assert!(!report.ok);
    assert_eq!(report.elapsed_ms, 7);
    assert_eq!(
        report.error.as_deref(),
        Some(
            "the dub session failed: exited with status 1; output tail: \
             Live reconnect refused: key=<key> invalid bye"
        )
    );
    assert_eq!((report.session, report.dub_rms_dbfs), (None, None));
    assert_eq!(report.youtube_id.as_deref(), Some("fffffffffff"));
}

#[test]
fn a_long_error_is_cut_after_the_key_is_redacted() {
    let key = "K".repeat(30);
    let error = format!("{}{key}", "e".repeat(590));
    let report = finish(DubProbeReport::new("m", "v"), Err(error), 0, &[key]);
    let error = report.error.unwrap();
    assert_eq!(error.chars().count(), 595);
    assert!(error.ends_with("<key>"), "{error}");
}

#[test]
fn the_answer_names_its_source_in_snake_case() {
    assert_eq!(
        serde_json::to_value(ClipSource::VocalStem).unwrap(),
        json!("vocal_stem")
    );
    assert_eq!(
        serde_json::to_value(ClipSource::Audio).unwrap(),
        json!("audio")
    );
}

// ---- the pick ----------------------------------------------------------------

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'P', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, kind) VALUES (2, 'SP-test', 'url2', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// A catalogue row the pick reads. `audio` / `vocals` are the recorded paths
/// (they may not exist on disk).
struct Row<'a> {
    id: i64,
    playlist: i64,
    youtube_id: &'a str,
    normalized: i64,
    audio: Option<&'a Path>,
    vocals: Option<&'a Path>,
    stem_status: Option<&'a str>,
}

impl<'a> Row<'a> {
    fn new(id: i64, youtube_id: &'a str, audio: &'a Path) -> Self {
        Self {
            id,
            playlist: 1,
            youtube_id,
            normalized: 1,
            audio: Some(audio),
            vocals: None,
            stem_status: None,
        }
    }

    async fn insert(&self, pool: &SqlitePool) {
        let path = |p: Option<&Path>| p.map(|p| p.to_string_lossy().into_owned());
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, audio_file_path, \
             vocals_file_path, stem_status) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(self.id)
        .bind(self.playlist)
        .bind(self.youtube_id)
        .bind(self.normalized)
        .bind(path(self.audio))
        .bind(path(self.vocals))
        .bind(self.stem_status)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn on_disk(cache: &Path, name: &str) -> PathBuf {
    let p = cache.join(name);
    std::fs::write(&p, b"flac").unwrap();
    p
}

fn lyrics_on_disk(cache: &Path, youtube_id: &str, starts: &[u64]) {
    let lines: Vec<Value> = starts
        .iter()
        .map(|s| json!({"start_ms": s, "end_ms": s + 2_000, "en": "line"}))
        .collect();
    let track = json!({"version": 22, "source": "gemini-3-5-transcribe", "lines": lines});
    std::fs::write(
        cache.join(format!("{youtube_id}_lyrics.json")),
        track.to_string(),
    )
    .unwrap();
}

/// The lowest-id downloaded row with its audio on disk wins, passing over a
/// missing audio, a row not normalized, the test item and a song that left
/// its playlist; its window starts at its earliest served line; its input is
/// its vocal stem when the stems are done and the file is there.
#[tokio::test]
async fn the_probe_picks_the_lowest_downloaded_song_with_its_audio_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path();
    let pool = pool().await;
    Row::new(1, "aaaaaaaaaaa", &cache.join("gone_audio.flac"))
        .insert(&pool)
        .await;
    let b = on_disk(cache, "b_audio.flac");
    Row {
        normalized: 0,
        ..Row::new(2, "bbbbbbbbbbb", &b)
    }
    .insert(&pool)
    .await;
    let c = on_disk(cache, "c_audio.flac");
    Row {
        playlist: 2,
        ..Row::new(3, "ccccccccccc", &c)
    }
    .insert(&pool)
    .await;
    let d = on_disk(cache, "d_audio.flac");
    Row::new(4, "ddddddddddd", &d).insert(&pool).await;
    sqlx::query("UPDATE videos SET removed_at = 1 WHERE id = 4")
        .execute(&pool)
        .await
        .unwrap();
    let e = on_disk(cache, "e_audio.flac");
    Row {
        audio: None,
        ..Row::new(5, "eeeeeeeeeee", &e)
    }
    .insert(&pool)
    .await;
    let f = on_disk(cache, "f_audio.flac");
    let f_vocals = on_disk(cache, "f_audio_vocals.flac");
    Row {
        vocals: Some(&f_vocals),
        stem_status: Some("done"),
        ..Row::new(6, "fffffffffff", &f)
    }
    .insert(&pool)
    .await;
    lyrics_on_disk(cache, "fffffffffff", &[41_500, 12_345, 20_000]);
    let g = on_disk(cache, "g_audio.flac");
    Row::new(7, "ggggggggggg", &g).insert(&pool).await;

    let clip = pick_clip(&pool, cache).await.unwrap().unwrap();

    assert_eq!(
        clip,
        ProbeClip {
            youtube_id: "fffffffffff".to_string(),
            input: f_vocals,
            source: ClipSource::VocalStem,
            start_ms: 12_345,
        }
    );
}

/// Stems not done, or the stem not on disk: the audio. No served line: 30 s.
#[tokio::test]
async fn without_a_ready_vocal_stem_the_clip_is_cut_from_the_audio() {
    for (stem_status, write_stem) in [(None, true), (Some("done"), false), (Some("failed"), true)] {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let pool = pool().await;
        let audio = on_disk(cache, "f_audio.flac");
        let vocals = cache.join("f_audio_vocals.flac");
        if write_stem {
            std::fs::write(&vocals, b"flac").unwrap();
        }
        Row {
            vocals: Some(&vocals),
            stem_status,
            ..Row::new(1, "fffffffffff", &audio)
        }
        .insert(&pool)
        .await;

        let clip = pick_clip(&pool, cache).await.unwrap().unwrap();

        assert_eq!(
            clip,
            ProbeClip {
                youtube_id: "fffffffffff".to_string(),
                input: audio,
                source: ClipSource::Audio,
                start_ms: 30_000,
            },
            "{stem_status:?} {write_stem}"
        );
    }
}

#[tokio::test]
async fn an_empty_catalogue_has_no_song_to_probe() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(pick_clip(&pool().await, dir.path()).await, Ok(None));
}

/// A catalogue that cannot be read is an error of the box, not "no song":
/// the probe ran (200) and failed naming it.
#[tokio::test]
async fn an_unreadable_catalogue_fails_the_probe_not_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let pool = pool().await;
    pool.close().await;
    let error = pick_clip(&pool, dir.path()).await.unwrap_err();
    assert!(
        error.starts_with("reading the catalogue failed: "),
        "{error}"
    );

    let (status, report) = probe(&pool, dir.path(), &tools, Some("ffmpeg".into()), key()).await;
    assert_eq!(status, ProbeStatus::Ran);
    assert!(!report.ok);
    let error = report.error.unwrap();
    assert!(
        error.starts_with("reading the catalogue failed: "),
        "{error}"
    );
}

// ---- run_probe: every refusal comes before anything is sent ------------------

/// `tools_dir` with the venv python on disk (an empty file: the refusals and
/// the cut never run it).
fn tools_with_python(dir: &Path) -> PathBuf {
    let tools = dir.join("tools");
    let python = crate::lyrics::bootstrap::venv_python_path(&tools);
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    std::fs::write(&python, b"").unwrap();
    tools
}

async fn probe(
    pool: &SqlitePool,
    cache: &Path,
    tools: &Path,
    ffmpeg: Option<PathBuf>,
    keys: Option<Vec<String>>,
) -> (ProbeStatus, DubProbeReport) {
    let input = ProbeInput {
        pool,
        cache_dir: cache,
        tools_dir: tools,
        ffmpeg,
        keys,
        model: "m",
        voice: "speaker",
    };
    run_probe(&input, LIMITS_FOR_TESTS).await
}

/// Bounds long enough for every test (the lock test sets its own).
const LIMITS_FOR_TESTS: Limits = Limits {
    lock: Duration::from_secs(60),
    dub: Duration::from_secs(5),
};

fn key() -> Option<Vec<String>> {
    Some(vec!["k-live".to_string(), "k-spare".to_string()])
}

fn refused(error: &str) -> (ProbeStatus, DubProbeReport) {
    let mut report = DubProbeReport::new("m", "speaker");
    report.error = Some(error.to_string());
    (ProbeStatus::Refused, report)
}

#[tokio::test]
async fn while_paid_ai_is_off_the_probe_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let got = probe(
        &pool().await,
        dir.path(),
        &tools,
        Some("ffmpeg".into()),
        None,
    )
    .await;
    assert_eq!(got, refused(crate::paid_ai::OFF_REASON));
}

#[tokio::test]
async fn with_no_key_the_probe_is_refused_naming_the_setting() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let got = probe(
        &pool().await,
        dir.path(),
        &tools,
        Some("ffmpeg".into()),
        Some(vec![]),
    )
    .await;
    assert_eq!(
        got,
        refused("no Gemini API key configured (setting gemini_api_key is empty)")
    );
}

#[tokio::test]
async fn with_no_venv_python_the_probe_is_refused_naming_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    let python = crate::lyrics::bootstrap::venv_python_path(&tools);
    let got = probe(
        &pool().await,
        dir.path(),
        &tools,
        Some("ffmpeg".into()),
        key(),
    )
    .await;
    let error = format!(
        "no venv python at {} (the lyrics bootstrap has not made it yet)",
        python.display()
    );
    assert_eq!(got, refused(&error));
}

#[tokio::test]
async fn with_no_song_the_probe_is_refused_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let got = probe(
        &pool().await,
        dir.path(),
        &tools,
        Some("ffmpeg".into()),
        key(),
    )
    .await;
    assert_eq!(got, refused(NO_SONG));
}

/// A pool with one song the probe can pick.
async fn one_song(cache: &Path) -> SqlitePool {
    let pool = pool().await;
    let audio = on_disk(cache, "f_audio.flac");
    Row::new(1, "fffffffffff", &audio).insert(&pool).await;
    lyrics_on_disk(cache, "fffffffffff", &[9_000]);
    pool
}

fn with_the_clip(report: DubProbeReport) -> DubProbeReport {
    DubProbeReport {
        youtube_id: Some("fffffffffff".into()),
        start_ms: Some(9_000),
        clip_ms: CLIP_MS,
        source: Some(ClipSource::Audio),
        ..report
    }
}

#[tokio::test]
async fn without_ffmpeg_the_probe_is_refused_naming_the_clip() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let pool = one_song(dir.path()).await;
    let (status, report) = probe(&pool, dir.path(), &tools, None, key()).await;
    let (want_status, want) = refused("ffmpeg is not ready yet (the tools are still starting)");
    assert_eq!((status, report), (want_status, with_the_clip(want)));
}

/// An ffmpeg that cannot run: the probe RAN (200) and failed with the reason;
/// no dub was attempted.
#[tokio::test]
async fn a_clip_that_cannot_be_cut_fails_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let pool = one_song(dir.path()).await;
    let ffmpeg = dir.path().join("no-such-ffmpeg");
    let (status, report) = probe(&pool, dir.path(), &tools, Some(ffmpeg), key()).await;
    assert_eq!(status, ProbeStatus::Ran);
    assert!(!report.ok);
    assert_eq!(report.elapsed_ms, 0);
    assert_eq!(report.youtube_id.as_deref(), Some("fffffffffff"));
    let error = report.error.unwrap();
    assert!(error.starts_with("ffmpeg did not start: "), "{error}");
    // No dub scripts were written: the child's set-up never ran.
    assert!(!tools.join("dub_worker.py").exists());
}

/// The probe reads its song under `cache::SONG_FILES`: while the lock is
/// held it waits, and a record made under the lock is what it sees (the
/// "must not finish yet" window is the safe direction).
#[tokio::test]
async fn the_probe_reads_its_song_under_the_song_files_lock() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let pool = one_song(dir.path()).await;
    let held = crate::downloader::cache::SONG_FILES.lock().await;

    let task = tokio::spawn({
        let pool = pool.clone();
        let cache = dir.path().to_path_buf();
        async move { probe(&pool, &cache, &tools, None, key()).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !task.is_finished(),
        "the probe waits for the song-files lock"
    );
    sqlx::query("UPDATE videos SET normalized = 0")
        .execute(&pool)
        .await
        .unwrap();
    drop(held);

    assert_eq!(task.await.unwrap(), refused(NO_SONG));
}

/// Song files held past the probe's bound: the probe RAN (200) and failed
/// naming the busy files, without picking a song (the run is bounded under
/// the spec's request timeout, never parked behind a sweep).
#[tokio::test]
async fn song_files_held_past_the_bound_fail_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_with_python(dir.path());
    let pool = one_song(dir.path()).await;
    let held = crate::downloader::cache::SONG_FILES.lock().await;
    let input = ProbeInput {
        pool: &pool,
        cache_dir: dir.path(),
        tools_dir: &tools,
        ffmpeg: None,
        keys: key(),
        model: "m",
        voice: "speaker",
    };
    let limits = Limits {
        lock: Duration::from_millis(200),
        dub: Duration::from_secs(5),
    };

    let (status, report) = run_probe(&input, limits).await;
    drop(held);

    assert_eq!(status, ProbeStatus::Ran);
    assert_eq!(
        report.error.as_deref(),
        Some("the song files stayed busy for 0.2 s (a rename or a sweep)")
    );
    assert_eq!(report.youtube_id, None);
    assert_eq!(
        LIMITS,
        Limits {
            lock: LOCK_TIMEOUT,
            dub: DUB_TIMEOUT
        }
    );
    assert_eq!((LOCK_TIMEOUT.as_secs(), DUB_TIMEOUT.as_secs()), (15, 240));
}

/// The scripts the probe runs are the dub worker's: every embedded script is
/// written into the tools dir and the worker script's path is answered.
#[tokio::test]
async fn the_probe_ships_the_dub_workers_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    let script = crate::dabing::worker::materialise_scripts(&tools)
        .await
        .unwrap();
    assert_eq!(script, tools.join("dub_worker.py"));
    for name in [
        "dub_worker.py",
        "dub_live_session.py",
        "dub_loudness.py",
        "win_replace.py",
    ] {
        let text = std::fs::read_to_string(tools.join(name)).unwrap();
        assert!(!text.is_empty(), "{name}");
    }
    assert!(
        std::fs::read_to_string(&script)
            .unwrap()
            .contains("def cmd_live_translate")
    );
}
