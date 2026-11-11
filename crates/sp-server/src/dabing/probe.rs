//! #232 unit 2: the live post-deploy probe of Gemini Live Translate (the dub).
//!
//! The dub worker streams a whole video through ONE continuous Live Translate
//! session (`dabing::child::run_live_translate`), and no post-deploy check ever
//! opened such a session: a refused key, a renamed or retired model, a session
//! protocol the API stopped accepting stayed invisible with CI green while
//! every new dub failed (the owner's rule of 29.9.2026: every external
//! provider and model gets a live check). `POST /api/v1/dabing/probe`
//! (`api::dabing_probe`) dubs ONE short clip from the box through the worker's
//! own child call, with the worker's own model, voice and key, and
//! `e2e/post-deploy-dub-live.spec.ts` fails the deploy unless it answers with a
//! dub that holds sound and a Slovak transcript. Design record: #232 comment
//! 6807882928.
//!
//! The clip:
//!
//! - the song: the lowest `videos.id` with `normalized = 1` and its audio on
//!   disk, never the test item nor a song that left its playlist
//!   (`db::queue_row!`) ([`pick_clip`]);
//! - the input: the dub's own rule, `dabing::worker::dub_input_audio` (its
//!   vocal stem when the stems are done and the file is there, else the
//!   audio);
//! - the window: [`CLIP_MS`] from the first served lyrics line's start when
//!   the song has a `{yt}_lyrics.json` with a line (singing, not an intro),
//!   else from [`NO_LINE_START_MS`] ([`window_start`]);
//! - cut by the app's ffmpeg into a WAV in a temp dir ([`cut_args`]); the pick
//!   and the cut hold `cache::SONG_FILES` (a rename holds it from its read to
//!   its record), the cut bounded by [`CUT_TIMEOUT`].
//!
//! The dub, its transcripts, its session event log and its work dir all stay
//! in that temp dir, dropped after the call. It runs WITHOUT the heavy slot:
//! the child is network-bound (no torch), and a gate must not wait behind a
//! stems separation. The child and its set-up (the scripts, the SDK check) are
//! bounded together by [`DUB_TIMEOUT`], the dub's read-back by
//! [`MEASURE_TIMEOUT`]: with the cut, below the spec's request timeout.
//!
//! `ok` = the child finished, the dub holds sound (RMS at least
//! [`MIN_DUB_RMS_DBFS`]) and the subtitle builder makes at least one Slovak
//! line of its transcripts ([`verdict`]). The answer never carries a key: every
//! error text is redacted with the key ([`finish`]). Refused with 409 (nothing
//! sent) while paid AI is off, with no Gemini key, with no venv python, or with
//! no song or no ffmpeg ([`ProbeStatus::Refused`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::dabing::child::{DubSessionStats, DubSummary};
use crate::dabing::subtitles::{DubTranscripts, transcripts_to_track};

/// Length of the clip dubbed (ms): a few sung or spoken lines, a few cents.
pub const CLIP_MS: u64 = 20_000;

/// Where the window starts (ms) in a song with no served lyrics line: past a
/// typical intro.
pub const NO_LINE_START_MS: u64 = 30_000;

/// Bound of the ffmpeg cut (well under a second): a hung ffmpeg fails the
/// probe with its own error and releases the song-files lock.
pub const CUT_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound of the dub itself: the scripts, the SDK check and the child (a 20 s
/// clip streams in ~20 s, then the drain and the loudness passes). A hung
/// session fails the probe with this error, the child killed on drop.
pub const DUB_TIMEOUT: Duration = Duration::from_secs(240);

/// Bound of the read-back of the dub (one ffmpeg decode of ~20 s).
pub const MEASURE_TIMEOUT: Duration = Duration::from_secs(15);

/// The dub's RMS floor: a translated voice, loudness-matched to the clip
/// (round F), reads −30…−15 dBFS; a session that answered silence reads the
/// silence floor.
pub const MIN_DUB_RMS_DBFS: f32 = -45.0;

/// The rate the dub is read back at for its level (mono).
pub const MEASURE_RATE: u64 = 16_000;

/// Characters of the first Slovak line the answer quotes (`sample`).
const SAMPLE_CHARS: usize = 120;

/// Characters an error text keeps (a child's output tail is long).
const ERROR_CHARS: usize = 600;

/// Which file of the song the clip was cut from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipSource {
    /// The vocal stem (the stems are done and the file is there).
    VocalStem,
    /// The song's audio (the full mix).
    Audio,
}

/// The clip a probe dubs: the song, the file it is cut from, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeClip {
    pub youtube_id: String,
    pub input: PathBuf,
    pub source: ClipSource,
    pub start_ms: u64,
}

/// Whether the probe ran (200) or was refused before anything was sent (409).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Ran,
    Refused,
}

/// `POST /api/v1/dabing/probe` answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DubProbeReport {
    pub ok: bool,
    /// The Live Translate model the session used (the `dub_model` setting).
    pub model: String,
    /// The voice (the `dub_voice` setting; `speaker` = the speaker's own).
    pub voice: String,
    /// The song; `None` when the probe stopped before picking one.
    pub youtube_id: Option<String>,
    /// Where the window starts in the song (ms).
    pub start_ms: Option<u64>,
    /// The clip's length (ms); 0 when no clip was picked.
    pub clip_ms: u64,
    pub source: Option<ClipSource>,
    /// How long the dub took (ms), set-up included; 0 when none ran.
    pub elapsed_ms: u64,
    /// The dub's length (ms); 0 when none was read back.
    pub dub_ms: u64,
    /// The dub's RMS level (dBFS); `None` when none was read back.
    pub dub_rms_dbfs: Option<f32>,
    /// Slovak subtitle lines the dub's transcripts make.
    pub sk_lines: usize,
    /// The first Slovak line (cut to 120 characters).
    pub sample: String,
    /// The child's session stats; `None` when it did not finish.
    pub session: Option<DubSessionStats>,
    /// Why the probe failed (never a key); `None` when `ok`.
    pub error: Option<String>,
}

impl DubProbeReport {
    /// A report with nothing run yet: `model` / `voice` and no clip.
    pub fn new(model: &str, voice: &str) -> Self {
        Self {
            ok: false,
            model: model.to_string(),
            voice: voice.to_string(),
            youtube_id: None,
            start_ms: None,
            clip_ms: 0,
            source: None,
            elapsed_ms: 0,
            dub_ms: 0,
            dub_rms_dbfs: None,
            sk_lines: 0,
            sample: String::new(),
            session: None,
            error: None,
        }
    }

    /// This report carrying `clip`.
    fn with_clip(mut self, clip: &ProbeClip) -> Self {
        self.youtube_id = Some(clip.youtube_id.clone());
        self.start_ms = Some(clip.start_ms);
        self.clip_ms = CLIP_MS;
        self.source = Some(clip.source);
        self
    }

    /// This report failed with `error`.
    fn failed(mut self, error: String) -> Self {
        self.ok = false;
        self.error = Some(error);
        self
    }
}

/// The window's start (ms): the first served lyrics line, else
/// [`NO_LINE_START_MS`].
pub fn window_start(first_line_ms: Option<u64>) -> u64 {
    first_line_ms.unwrap_or(NO_LINE_START_MS)
}

/// `ms` as ffmpeg seconds with millisecond precision: 61_234 → `61.234`.
fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// ffmpeg's arguments for the clip: [`CLIP_MS`] of `input` from `start_ms`,
/// audio only, as a 16-bit PCM WAV at `out` with the input's own rate and
/// channels (the child decodes and measures it like any dub input). `-ss`
/// before `-i` seeks the input.
pub fn cut_args(input: &Path, start_ms: u64, out: &Path) -> Vec<OsString> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-ss".into(),
        seconds(start_ms).into(),
        "-t".into(),
        seconds(CLIP_MS).into(),
        "-i".into(),
        input.as_os_str().to_os_string(),
        "-vn".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        "-y".into(),
        out.as_os_str().to_os_string(),
    ]
}

/// ffmpeg's arguments for the dub's read-back: `dub` decoded to mono 32-bit
/// float at [`MEASURE_RATE`] on stdout.
pub fn measure_args(dub: &Path) -> Vec<OsString> {
    vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        dub.as_os_str().to_os_string(),
        "-vn".into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        MEASURE_RATE.to_string().into(),
        "-f".into(),
        "f32le".into(),
        "pipe:1".into(),
    ]
}

/// The dub as read back: its length and its level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DubLevel {
    pub dub_ms: u64,
    pub rms_dbfs: f32,
}

/// The level of `pcm`, mono little-endian f32 at [`MEASURE_RATE`] (a trailing
/// partial sample is ignored).
pub fn level_of(pcm: &[u8]) -> DubLevel {
    let samples: Vec<f32> = pcm
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let frames = samples.len() as u64;
    DubLevel {
        dub_ms: frames * 1000 / MEASURE_RATE,
        rms_dbfs: sp_core::audio_level::rms_dbfs(&samples),
    }
}

/// The Slovak lines the subtitle builder makes of `transcripts` (the one the
/// dub worker stores for the wall) and the first one, cut to
/// [`SAMPLE_CHARS`]. A line with a blank Slovak text does not count.
pub fn sk_lines_of(transcripts: &DubTranscripts) -> (usize, String) {
    let track = transcripts_to_track(transcripts);
    let sk: Vec<&str> = track
        .lines
        .iter()
        .filter_map(|l| l.sk.as_deref().map(str::trim))
        .filter(|t| !t.is_empty())
        .collect();
    let sample = sk
        .first()
        .map(|t| t.chars().take(SAMPLE_CHARS).collect())
        .unwrap_or_default();
    (sk.len(), sample)
}

/// Why a finished dub fails the gate; `None` = it passes. The dub must hold
/// sound (RMS at least [`MIN_DUB_RMS_DBFS`]) and make at least one Slovak line.
pub fn verdict(rms_dbfs: f32, sk_lines: usize) -> Option<String> {
    let mut failures = Vec::new();
    if rms_dbfs < MIN_DUB_RMS_DBFS {
        failures.push(format!(
            "the dub is silent: {rms_dbfs:.1} dBFS, under the {MIN_DUB_RMS_DBFS} dBFS floor"
        ));
    }
    if sk_lines == 0 {
        failures.push("the session gave no Slovak transcript line".to_string());
    }
    (!failures.is_empty()).then(|| failures.join("; "))
}

/// What the dub child left, read back.
#[derive(Debug, Clone)]
pub struct DubRead {
    pub session: DubSessionStats,
    pub level: DubLevel,
    pub transcripts: DubTranscripts,
}

/// `report` (with its clip) completed by the dub's `outcome` after
/// `elapsed_ms`. An error text is put on one line, redacted with `keys` and
/// then cut, so no key (nor a key's prefix at the cut) is ever answered.
pub fn finish(
    report: DubProbeReport,
    outcome: Result<DubRead, String>,
    elapsed_ms: u64,
    keys: &[String],
) -> DubProbeReport {
    let mut report = DubProbeReport {
        elapsed_ms,
        ..report
    };
    match outcome {
        Err(error) => {
            let error = crate::gemini_api::body_excerpt(&error, keys, ERROR_CHARS);
            report.failed(error)
        }
        Ok(read) => {
            let (sk_lines, sample) = sk_lines_of(&read.transcripts);
            report.dub_ms = read.level.dub_ms;
            report.dub_rms_dbfs = Some(read.level.rms_dbfs);
            report.sk_lines = sk_lines;
            report.sample = sample;
            report.session = Some(read.session);
            report.error = verdict(read.level.rms_dbfs, sk_lines);
            report.ok = report.error.is_none();
            report
        }
    }
}

/// The probe's song (module doc). `Err` says why there is none.
pub async fn pick_clip(pool: &SqlitePool, cache_dir: &Path) -> Result<ProbeClip, String> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(concat!(
        "SELECT youtube_id, audio_file_path, vocals_file_path, stem_status FROM videos \
         WHERE normalized = 1 AND audio_file_path IS NOT NULL AND ",
        crate::db::queue_row!(),
        " ORDER BY id"
    ))
    .fetch_all(pool)
    .await
    .map_err(|e| format!("reading the catalogue failed: {e}"))?;
    for (youtube_id, audio, vocals, stem_status) in rows {
        if !Path::new(&audio).is_file() {
            continue;
        }
        let vocals_exists = vocals.as_deref().is_some_and(|v| Path::new(v).is_file());
        let input = crate::dabing::worker::dub_input_audio(
            &audio,
            vocals.as_deref(),
            stem_status.as_deref(),
            vocals_exists,
        );
        let source = if input == Path::new(&audio) {
            ClipSource::Audio
        } else {
            ClipSource::VocalStem
        };
        let first = crate::lyrics::g35t_probe::first_line_start(cache_dir, &youtube_id).await;
        return Ok(ProbeClip {
            youtube_id,
            input,
            source,
            start_ms: window_start(first),
        });
    }
    Err("no song to probe: no downloaded song has its audio on disk".to_string())
}

/// Cut `clip` into the WAV `out` with `ffmpeg` ([`cut_args`]).
#[cfg_attr(test, mutants::skip)] // shells out to ffmpeg; its arguments are `cut_args`, tested
async fn cut_clip(ffmpeg: &Path, clip: &ProbeClip, out: &Path) -> Result<(), String> {
    let output = ffmpeg_output(ffmpeg, cut_args(&clip.input, clip.start_ms, out)).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = crate::metadata::health::bounded_error(stderr.trim());
        return Err(format!(
            "ffmpeg could not cut the clip ({}): {stderr}",
            output.status
        ));
    }
    Ok(())
}

/// Run `ffmpeg` with `args`, stdout and stderr captured, killed on drop.
#[cfg_attr(test, mutants::skip)] // process spawn; its callers' arguments are tested
async fn ffmpeg_output(ffmpeg: &Path, args: Vec<OsString>) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::downloader::hide_console_window(&mut cmd);
    cmd.output()
        .await
        .map_err(|e| format!("ffmpeg did not start: {e}"))
}

/// Read the dub at `dub` back with `ffmpeg` ([`measure_args`], [`level_of`]).
#[cfg_attr(test, mutants::skip)] // shells out to ffmpeg; `measure_args` + `level_of` are tested
async fn measure_dub(ffmpeg: &Path, dub: &Path) -> Result<DubLevel, String> {
    let output = ffmpeg_output(ffmpeg, measure_args(dub)).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = crate::metadata::health::bounded_error(stderr.trim());
        return Err(format!(
            "ffmpeg could not read the dub back ({}): {stderr}",
            output.status
        ));
    }
    Ok(level_of(&output.stdout))
}

/// One dub run: where everything goes and what the child is given.
struct DubRun<'a> {
    tools_dir: &'a Path,
    python: &'a Path,
    clip_wav: &'a Path,
    dir: &'a Path,
    key: &'a str,
    model: &'a str,
    voice: &'a str,
}

/// Ship the dub scripts, check the SDK, run the worker's child on the clip
/// (everything in `run.dir`) and read its dub and transcripts back. NOT under
/// the heavy slot (module doc).
#[cfg_attr(test, mutants::skip)] // spawns the Python child + Gemini Live; its pieces (`live_translate_args`, `measure_args`, `level_of`, `finish`) are tested
async fn run_dub(run: &DubRun<'_>) -> Result<DubSummary, String> {
    let script = crate::dabing::worker::materialise_scripts(run.tools_dir)
        .await
        .map_err(|e| format!("the dub scripts could not be written: {e:#}"))?;
    crate::lyrics::bootstrap::ensure_genai(run.python)
        .await
        .map_err(|e| format!("google-genai is not available in the venv: {e:#}"))?;
    let work_dir = run.dir.join("work");
    tokio::fs::create_dir_all(&work_dir)
        .await
        .map_err(|e| format!("no work dir for the dub: {e}"))?;
    let plan = crate::lyrics::heavy_plan::HeavyStepPlan::cpu_idle();
    crate::dabing::child::run_live_translate(
        run.python,
        &script,
        run.clip_wav,
        &run.dir.join("probe_dub.flac"),
        &run.dir.join("probe_dub_transcripts.json"),
        &work_dir,
        run.key,
        run.model,
        run.voice,
        Duration::from_millis(CLIP_MS) + Duration::from_secs(120),
        &plan,
    )
    .await
    .map_err(|e| format!("the dub session failed: {e:#}"))
}

/// Read the finished dub's transcripts and level back.
#[cfg_attr(test, mutants::skip)] // file + ffmpeg glue; `level_of` / `sk_lines_of` are tested
async fn read_back(ffmpeg: &Path, summary: DubSummary) -> Result<DubRead, String> {
    let text = tokio::fs::read_to_string(&summary.transcripts_path)
        .await
        .map_err(|e| format!("the dub's transcripts could not be read: {e}"))?;
    let transcripts: DubTranscripts = serde_json::from_str(&text)
        .map_err(|e| format!("the dub's transcripts are not readable: {e}"))?;
    let level = match tokio::time::timeout(
        MEASURE_TIMEOUT,
        measure_dub(ffmpeg, Path::new(&summary.out_path)),
    )
    .await
    {
        Ok(level) => level?,
        Err(_) => {
            let secs = MEASURE_TIMEOUT.as_secs();
            return Err(format!("ffmpeg did not read the dub back within {secs} s"));
        }
    };
    Ok(DubRead {
        session: summary.session,
        level,
        transcripts,
    })
}

/// What the probe is given: the box's state and the dub settings as read.
pub struct ProbeInput<'a> {
    pub pool: &'a SqlitePool,
    pub cache_dir: &'a Path,
    /// `cache_dir/tools`: the venv and the dub scripts.
    pub tools_dir: &'a Path,
    /// The app's ffmpeg (`None` while the tools are still starting).
    pub ffmpeg: Option<PathBuf>,
    /// The Gemini keys (`None` while paid AI is off).
    pub keys: Option<Vec<String>>,
    pub model: &'a str,
    pub voice: &'a str,
}

/// The whole probe (module doc). Each missing precondition refuses it before
/// anything is sent; the pick and the cut hold `cache::SONG_FILES`; the dub
/// runs after the lock is released, bounded by `dub_limit`.
pub async fn run_probe(
    input: &ProbeInput<'_>,
    dub_limit: Duration,
) -> (ProbeStatus, DubProbeReport) {
    let report = DubProbeReport::new(input.model, input.voice);
    let refused =
        |report: DubProbeReport, error: String| (ProbeStatus::Refused, report.failed(error));
    let Some(keys) = input.keys.as_deref() else {
        return refused(report, crate::paid_ai::OFF_REASON.to_string());
    };
    let Some(key) = keys.first() else {
        let error = "no Gemini API key configured (setting gemini_api_key is empty)";
        return refused(report, error.to_string());
    };
    let python = crate::lyrics::bootstrap::venv_python_path(input.tools_dir);
    if !python.is_file() {
        let error = format!(
            "no venv python at {} (the lyrics bootstrap has not made it yet)",
            python.display()
        );
        return refused(report, error);
    }
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => return (ProbeStatus::Ran, report.failed(format!("no temp dir: {e}"))),
    };
    let clip_wav = dir.path().join("probe_clip.wav");
    let (report, ffmpeg) = {
        // A rename holds this lock from its read to its record: holding it
        // from the pick through the cut, the clip's input never moves between.
        let _files = crate::downloader::cache::SONG_FILES.lock().await;
        let clip = match pick_clip(input.pool, input.cache_dir).await {
            Ok(clip) => clip,
            Err(error) => return refused(report, error),
        };
        let report = report.with_clip(&clip);
        let Some(ffmpeg) = input.ffmpeg.as_deref() else {
            let error = "ffmpeg is not ready yet (the tools are still starting)";
            return refused(report, error.to_string());
        };
        match tokio::time::timeout(CUT_TIMEOUT, cut_clip(ffmpeg, &clip, &clip_wav)).await {
            Ok(Ok(())) => (report, ffmpeg),
            Ok(Err(error)) => return (ProbeStatus::Ran, report.failed(error)),
            Err(_) => {
                let secs = CUT_TIMEOUT.as_secs();
                let error = format!("ffmpeg did not cut the clip within {secs} s");
                return (ProbeStatus::Ran, report.failed(error));
            }
        }
    };
    let started = Instant::now();
    let run = DubRun {
        tools_dir: input.tools_dir,
        python: &python,
        clip_wav: &clip_wav,
        dir: dir.path(),
        key,
        model: input.model,
        voice: input.voice,
    };
    let outcome = match tokio::time::timeout(dub_limit, run_dub(&run)).await {
        Ok(Ok(summary)) => read_back(ffmpeg, summary).await,
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!(
            "no dub within {} s (the session was ended)",
            dub_limit.as_secs()
        )),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    (ProbeStatus::Ran, finish(report, outcome, elapsed_ms, keys))
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
