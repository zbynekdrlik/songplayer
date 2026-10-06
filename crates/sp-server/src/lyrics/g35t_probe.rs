//! #144: the live post-deploy probe of Gemini 3.5 Transcribe (g35t).
//!
//! No post-deploy gate used to send a g35t request. A dead or refused Gemini
//! key, a renamed model or a request field the API refuses (the
//! `language_codes` hint #144 added) stayed invisible with CI green, while
//! every new transcript failed: base-tier lyrics and the ★-tier reference
//! gate both lose their transcript. `POST /api/v1/lyrics/g35t/probe`
//! (`api::lyrics_g35t`) sends ONE short real request from the box through
//! the worker's own call, [`g35t_client::transcribe_at`]: the same upload,
//! request body, [`g35t_client::LANGUAGE_CODES`] hint and key rotation.
//! `e2e/post-deploy-g35t.spec.ts` fails the deploy when it does not answer
//! with words. One short paid call per deploy, like the metadata probe.
//!
//! The clip (design record: #144 comment 5996959706):
//!
//! - the song: the lowest `videos.id` with `normalized = 1`,
//!   `has_lyrics = 1`, its audio on disk and a `{yt}_lyrics.json` with a
//!   line ([`pick_clip`]);
//! - the input: the isolated vocal the worker itself uploads
//!   (`aligner::isolated_vocal_path`, `{yt}_vocals16k.wav`) when on disk,
//!   else its vocal stem (`stems::stem_paths`), else the audio itself (the
//!   full mix);
//! - the window: [`CLIP_MS`] from the first served line's start, so the clip
//!   holds singing (an instrumental intro would answer 0 words, a false red);
//! - cut by the app's ffmpeg into a 16 kHz mono float WAV ([`clip_args`]),
//!   the format of the worker's own isolated vocal, in a temp dir dropped
//!   after the call. The pick and the cut hold `cache::SONG_FILES`, the lock
//!   a rename holds, so the input is never renamed between the two; the cut
//!   is bounded by [`CUT_TIMEOUT`].
//!
//! The answer never carries a key: the client's error texts name a key by
//! its place in the list (`key 2 of 5`) and are redacted with every key.
//! It lists every key refused before the one that decided the outcome
//! (`refused_keys`), so a dead key shows even while a later one answers.
//! The post-deploy gate (`e2e/g35t-gate.ts`) fails on such a key unless it
//! was a 429; keys after the answering one are not tried (that would cost a
//! paid call per key).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::lyrics::g35t_client::{self, KeyRefusal, LANGUAGE_CODES, MODEL_SLUG};

/// Length of the clip sent (ms): long enough for a few sung lines, short
/// enough to cost a fraction of a cent.
pub const CLIP_MS: u64 = 20_000;

/// Bound of the whole transcription in a probe: below the post-deploy spec's
/// 220 s request timeout, so a hung call fails the gate WITH this error
/// instead of as a bare Playwright timeout (the same bound as the metadata
/// probe; a 20 s clip answers in seconds).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(180);

/// Bound of the ffmpeg cut (it takes well under a second): a hung ffmpeg
/// fails the probe with its own error, its process killed on drop, and the
/// song-files lock it holds is released. With [`PROBE_TIMEOUT`] it stays
/// below the spec's 220 s.
pub const CUT_TIMEOUT: Duration = Duration::from_secs(15);

/// Words of the transcript the answer quotes (`sample`): the read-back that
/// the model really heard the clip.
const SAMPLE_WORDS: usize = 8;

/// Which file of the song the clip was cut from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipSource {
    /// The isolated vocal the worker uploads (`{yt}_vocals16k.wav`).
    IsolatedVocal,
    /// The vocal stem (`…_audio_vocals.flac`): no isolated vocal on disk.
    VocalStem,
    /// The song's audio, the full mix: neither vocal on disk.
    Mix,
}

/// The clip a probe sends: the song, the file it is cut from, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeClip {
    pub youtube_id: String,
    pub input: PathBuf,
    pub source: ClipSource,
    /// Where the window starts in the song (ms): the first served line.
    pub start_ms: u64,
}

impl ProbeClip {
    /// The clip as the answer shows it.
    pub fn info(&self) -> ClipInfo {
        ClipInfo {
            youtube_id: self.youtube_id.clone(),
            source: self.source,
            start_ms: self.start_ms,
            duration_ms: CLIP_MS,
        }
    }
}

/// The clip on the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipInfo {
    pub youtube_id: String,
    pub source: ClipSource,
    pub start_ms: u64,
    pub duration_ms: u64,
}

/// `POST /api/v1/lyrics/g35t/probe` answer. `ok` = the model answered the
/// clip with at least one word. Always sent with 200: a failure is
/// `ok: false` + `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct G35tProbeReport {
    pub ok: bool,
    /// The model every request names: [`MODEL_SLUG`], the constant
    /// `interactions_body` sends.
    pub model: String,
    /// The key (0-based place in the `gemini_api_key` list) that answered,
    /// or whose answer ended the call; `None` when no key was tried, or the
    /// call was cut by [`PROBE_TIMEOUT`] (then `refused_keys` is empty too).
    pub key_index: Option<usize>,
    /// The hint every request carries: [`LANGUAGE_CODES`], the constant
    /// `interactions_body` sends.
    pub language_codes: Vec<String>,
    pub word_count: usize,
    /// How long the transcription took (ms); 0 when none was sent.
    pub latency_ms: u64,
    /// Why the probe failed (never a key); `None` when `ok`. An API error
    /// carries the reply's 400-character excerpt (`g35t_client`).
    pub error: Option<String>,
    /// The keys refused (a 429, `rate_limited`, or a key refusal) before the
    /// one that decided the outcome, in order; every key when all were
    /// refused.
    pub refused_keys: Vec<KeyRefusal>,
    /// The clip sent; `None` when the probe stopped before picking one.
    pub clip: Option<ClipInfo>,
    /// The first [`SAMPLE_WORDS`] words heard, space-separated.
    pub sample: String,
}

impl G35tProbeReport {
    /// A probe that stopped before any request was sent.
    pub fn refused(error: String, clip: Option<ClipInfo>) -> Self {
        Self {
            ok: false,
            model: MODEL_SLUG.to_string(),
            key_index: None,
            language_codes: language_codes(),
            word_count: 0,
            latency_ms: 0,
            error: Some(error),
            refused_keys: Vec::new(),
            clip,
            sample: String::new(),
        }
    }
}

/// [`LANGUAGE_CODES`] as the answer carries it.
fn language_codes() -> Vec<String> {
    LANGUAGE_CODES.iter().map(|c| c.to_string()).collect()
}

/// `ms` as ffmpeg seconds with millisecond precision: 61_234 → `61.234`.
fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

/// ffmpeg's arguments for the clip: [`CLIP_MS`] of `input` from `start_ms`,
/// audio only, as a 16 kHz mono 32-bit float WAV at `out`: the format of the
/// worker's isolated vocal (`scripts/lyrics_worker.py` writes `FLOAT`).
/// `-ss` before `-i` seeks the input.
pub fn clip_args(input: &Path, start_ms: u64, out: &Path) -> Vec<OsString> {
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
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        "16000".into(),
        "-c:a".into(),
        "pcm_f32le".into(),
        "-y".into(),
        out.as_os_str().to_os_string(),
    ]
}

/// The probe's song (module doc): the lowest-id row with served lyrics whose
/// audio is on disk, the clip starting at its first served line. `Err` says
/// why there is none.
pub async fn pick_clip(pool: &SqlitePool, cache_dir: &Path) -> Result<ProbeClip, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT youtube_id, audio_file_path FROM videos \
         WHERE normalized = 1 AND has_lyrics = 1 AND audio_file_path IS NOT NULL \
         ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("reading the catalogue failed: {e}"))?;
    for (youtube_id, audio) in rows {
        let audio = PathBuf::from(audio);
        if !audio.is_file() {
            continue;
        }
        let Some(start_ms) = first_line_start(cache_dir, &youtube_id).await else {
            continue;
        };
        let isolated = crate::lyrics::aligner::isolated_vocal_path(cache_dir, &youtube_id);
        let stem = crate::stems::stem_paths(&audio).0;
        let (input, source) = if isolated.is_file() {
            (isolated, ClipSource::IsolatedVocal)
        } else if stem.is_file() {
            (stem, ClipSource::VocalStem)
        } else {
            (audio, ClipSource::Mix)
        };
        return Ok(ProbeClip {
            youtube_id,
            input,
            source,
            start_ms,
        });
    }
    Err("no song to probe: no row with served lyrics has its audio on disk".to_string())
}

/// The earliest line start (ms) of the song's served `{yt}_lyrics.json`;
/// `None` when the file is missing, unreadable or has no line.
async fn first_line_start(cache_dir: &Path, youtube_id: &str) -> Option<u64> {
    let path = cache_dir.join(format!("{youtube_id}_lyrics.json"));
    let text = tokio::fs::read_to_string(&path).await.ok()?;
    let track: sp_core::lyrics::LyricsTrack = serde_json::from_str(&text).ok()?;
    track.lines.iter().map(|l| l.start_ms).min()
}

/// Cut `clip` into the WAV `out` with `ffmpeg` ([`clip_args`]).
#[cfg_attr(test, mutants::skip)] // shells out to ffmpeg; its arguments are `clip_args`, tested
pub async fn cut_clip(ffmpeg: &Path, clip: &ProbeClip, out: &Path) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(clip_args(&clip.input, clip.start_ms, out))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::downloader::hide_console_window(&mut cmd);
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("ffmpeg did not start: {e}"))?;
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

/// Send `clip_wav` through the worker's call on `api_root` with `keys`,
/// bounded by `limit`, and report it. `ok` needs at least one word: a
/// completed answer with none is a failure too (a hint sent without its
/// `mode` answers exactly that, `.claude/rules/lyrics-eval-backends.md`).
/// A call cut by `limit` leaves its uploaded file to Gemini's own expiry.
pub async fn probe_clip(
    client: &reqwest::Client,
    api_root: &str,
    keys: &[String],
    clip_wav: &Path,
    clip: Option<ClipInfo>,
    limit: Duration,
) -> G35tProbeReport {
    let started = Instant::now();
    let call = g35t_client::transcribe_at(client, api_root, keys, clip_wav);
    let result = tokio::time::timeout(limit, call).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let (key_index, words, refused_keys, error) = match result {
        Ok(Ok(heard)) => (Some(heard.key_index), heard.words, heard.refused, None),
        Ok(Err(failure)) => {
            let error = format!("{:#}", failure.error);
            (failure.key_index, Vec::new(), failure.refused, Some(error))
        }
        Err(_) => {
            let error = format!("no answer within {} s", limit.as_secs_f64());
            (None, Vec::new(), Vec::new(), Some(error))
        }
    };
    let error = match error {
        Some(error) => Some(error),
        None if words.is_empty() => Some("Gemini answered the clip with no words".to_string()),
        None => None,
    };
    let sample = words
        .iter()
        .take(SAMPLE_WORDS)
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    G35tProbeReport {
        ok: error.is_none(),
        model: MODEL_SLUG.to_string(),
        key_index,
        language_codes: language_codes(),
        word_count: words.len(),
        latency_ms,
        error,
        refused_keys,
        clip,
        sample,
    }
}

/// The whole probe on `api_root`: the key list, the song, ffmpeg, the cut
/// (the pick and the cut under `cache::SONG_FILES`, the cut bounded by
/// [`CUT_TIMEOUT`]), then [`probe_clip`]. Each missing piece stops it with its
/// reason, before anything is sent. `ffmpeg` is the app's bundled ffmpeg
/// (`None` while the tools are still starting).
pub async fn run_probe(
    pool: &SqlitePool,
    cache_dir: &Path,
    ffmpeg: Option<PathBuf>,
    keys: &[String],
    client: &reqwest::Client,
    api_root: &str,
) -> G35tProbeReport {
    if keys.is_empty() {
        let error = "no Gemini API key configured (setting gemini_api_key is empty)";
        return G35tProbeReport::refused(error.to_string(), None);
    }
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            let error = format!("no temp dir for the clip: {e}");
            return G35tProbeReport::refused(error, None);
        }
    };
    let wav = dir.path().join("g35t_probe.wav");
    let info = {
        // A rename (the metadata repair, a title correction) holds this lock
        // from its read to its record: holding it from the pick through the
        // cut, the clip's input is never renamed in between.
        let _files = crate::downloader::cache::SONG_FILES.lock().await;
        let clip = match pick_clip(pool, cache_dir).await {
            Ok(clip) => clip,
            Err(error) => return G35tProbeReport::refused(error, None),
        };
        let info = clip.info();
        let Some(ffmpeg) = ffmpeg else {
            let error = "ffmpeg is not ready yet (the tools are still starting)";
            return G35tProbeReport::refused(error.to_string(), Some(info));
        };
        match tokio::time::timeout(CUT_TIMEOUT, cut_clip(&ffmpeg, &clip, &wav)).await {
            Ok(Ok(())) => info,
            Ok(Err(error)) => return G35tProbeReport::refused(error, Some(info)),
            Err(_) => {
                let secs = CUT_TIMEOUT.as_secs();
                let error = format!("ffmpeg did not cut the clip within {secs} s");
                return G35tProbeReport::refused(error, Some(info));
            }
        }
    };
    probe_clip(client, api_root, keys, &wav, Some(info), PROBE_TIMEOUT).await
}

#[cfg(test)]
#[path = "g35t_probe_tests.rs"]
mod tests;
