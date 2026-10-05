//! Gemini 3.5 Transcribe client — independent-ASR word-level verification
//! for forced alignment (issue #143, design settled on #130's 2026-09-12
//! comment). Used by the mtl alignment stage's reference gate
//! (`reference_gate.rs`): the forced-alignment line timings are only
//! stamped as a ★ reference song when they AGREE with these independently
//! transcribed words.
//!
//! Rust port of the verified Python prototype
//! `eval/lyrics/backends/gemini_3_5_transcribe.py` — see
//! `.claude/rules/lyrics-eval-backends.md` ("Gemini 3.5 Transcribe"
//! section) for the exact API shapes this was proven against live on
//! 2026-09-12. Port the proven shape; do not re-derive it.
//!
//! Three-step API, base `https://generativelanguage.googleapis.com`:
//!   1. `POST /upload/v1beta/files` (raw WAV bytes) → file resource
//!      (`name`, `uri`, `mimeType`, `state`)
//!   2. `GET  /v1beta/{name}` polled every 2s (max 60s) until
//!      `state != "PROCESSING"` (`FAILED` is an error)
//!   3. `POST /v1beta/interactions` → word-level transcript
//!      (`status` must be `"completed"`)
//!   4. best-effort `DELETE /v1beta/{name}` cleanup, always attempted once
//!      the file was created, regardless of step 2/3 outcome
//!
//! Auth is the `x-goog-api-key` header (never a Bearer token, never a
//! query param). `api_keys` is tried in order by the shared Gemini key-list
//! contract (`crate::gemini_api`, #136 — the metadata provider rotates by the
//! same rules): a 429 or a key refusal (a 403, or a 400 naming the API key)
//! moves to the NEXT key; a 5xx retries the SAME key with backoff (2/4/8/16s,
//! up to 4 retries). Any other failure aborts immediately. Never log a header
//! or a key value — only the key's INDEX; log word count + elapsed time at
//! info on success. A failure's text names the key as `key i of n` and is
//! redacted with every key (`gemini_api::redact_keys`; a refused body with
//! its request's key BEFORE the 400-char cut), so it may go to a log or to
//! the live probe (`g35t_probe`, #144) as is.
//!
//! [`transcribe_words`] is the worker's call on Google's API root;
//! [`transcribe_at`] is the same call on any root (a mock server in tests)
//! that also says which key answered — the post-deploy probe uses it.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::time::sleep;

use crate::gemini_api::{
    GEMINI_API_ROOT, KeyReply, KeyVerdict, RETRY_BACKOFFS, redact_keys, send_on_key,
};

/// The model every request names (the probe reports it, #144).
pub(crate) const MODEL_SLUG: &str = "gemini-3.5-transcribe";
const AUDIO_MIME_TYPE: &str = "audio/wav";

/// #144: the BCP-47 `transcription_config.language_codes` every request
/// carries. The API reference reads them as "hints about the languages
/// present in the audio" (omitted or empty = automatic detection), and the
/// transcribe guide says to pass them whenever the language is known.
///
/// The catalogue sings in English and Spanish, so both are hinted; the model
/// picks between them per song and follows a bilingual one (code-switching).
/// `es-419` (Latin America) is the Spanish code the official
/// supported-languages table lists; it has no `es-ES`. Auto-detection over
/// 85+ locales was rejected: on sung vocals it can land on a neighbouring
/// language and put that on the wall. Design record: issue #144 comment
/// 5995867005. The live probe reports this same constant (`g35t_probe`).
pub(crate) const LANGUAGE_CODES: &[&str] = &["en-US", "es-419"];

const FILE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const FILE_POLL_TIMEOUT: Duration = Duration::from_secs(60);

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const INTERACTIONS_TIMEOUT: Duration = Duration::from_secs(600);

/// One ASR word with millisecond timing. Deliberately separate from
/// `sp_core::lyrics::LyricsWord` / `crate::lyrics::backend::AlignedWord` —
/// this type only ever carries the INDEPENDENT verification source's real
/// words through the reference-gate matcher; production lyrics never
/// persist synthesized per-word timing (see `mod.rs` v18 history).
#[derive(Debug, Clone, PartialEq)]
pub struct AsrWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// A transcription [`transcribe_at`] got: the words, and the index (0-based,
/// in the `gemini_api_key` list, the `key_index` of the log line) of the key
/// that answered.
#[derive(Debug)]
pub(crate) struct Transcription {
    pub(crate) words: Vec<AsrWord>,
    pub(crate) key_index: usize,
}

/// A transcription [`transcribe_at`] did not get.
#[derive(Debug)]
pub(crate) struct TranscribeFailure {
    /// Index (0-based) of the key whose answer ended the call: the key a
    /// fatal answer came on, or the last key when every key was refused.
    /// `None` when no key was tried (none configured, or the audio could not
    /// be read).
    pub(crate) key_index: Option<usize>,
    /// What went wrong: names the key as `key i of n` (1-based) and carries
    /// the API's own answer, with every key redacted.
    pub(crate) error: anyhow::Error,
}

/// Outcome of a single HTTP step against one API key.
enum StepError {
    /// `KeyVerdict::NextKey` (a 429 or a key refusal) — the caller should try
    /// the NEXT key in `api_keys`.
    NextKey(anyhow::Error),
    /// Any other failure — abort the transcription entirely.
    Fatal(anyhow::Error),
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Issue one HTTP call on one key through the shared `gemini_api::send_on_key`
/// (a 5xx is retried on the same key after each `RETRY_BACKOFFS` pause) and
/// map its reply: a 429 or a key refusal is `StepError::NextKey` (the same
/// key would fail identically); anything else not answered is `Fatal`.
/// `build` is invoked fresh on every attempt because a
/// `reqwest::RequestBuilder` is consumed by `.send()`. A refused body is
/// redacted with `api_key` (the request's own key) before `step_error` cuts
/// it, so no key prefix survives at the edge.
// The loop itself is unit-tested in `gemini_api_tests.rs` against a mock
// server; this mapping only wraps it. The probe tests (`g35t_probe_tests.rs`)
// drive it end to end through `transcribe_at`.
#[cfg_attr(test, mutants::skip)]
async fn send_with_retry(
    what: &str,
    api_key: &str,
    timeout: Option<Duration>,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, StepError> {
    let reply = send_on_key(what, &RETRY_BACKOFFS, || match timeout {
        Some(t) => build().timeout(t),
        None => build(),
    })
    .await
    .map_err(|e| StepError::Fatal(anyhow!("g35t_client {what}: request failed: {e}")))?;
    match reply {
        KeyReply::Answered(resp) => Ok(resp),
        KeyReply::Refused {
            verdict,
            status,
            body,
        } => Err(step_error(
            what,
            verdict,
            status,
            &redact_keys(&body, &[api_key]),
        )),
    }
}

/// A refused `send_on_key` reply as this client's `StepError`: a 429 or a
/// key refusal → `NextKey` (try the next key); a 5xx after every pause or
/// any other status → `Fatal`.
fn step_error(what: &str, verdict: KeyVerdict, status: u16, body: &str) -> StepError {
    let body = truncate(body, 400);
    match verdict {
        KeyVerdict::NextKey { .. } => StepError::NextKey(anyhow!(
            "g35t_client {what}: key refused status={status} body={body}"
        )),
        KeyVerdict::RetrySameKey => StepError::Fatal(anyhow!(
            "g35t_client {what}: exhausted retries status={status} body={body}"
        )),
        KeyVerdict::Stop => StepError::Fatal(anyhow!(
            "g35t_client {what}: unexpected status={status} body={body}"
        )),
    }
}

#[cfg_attr(test, mutants::skip)]
async fn upload_audio(
    client: &reqwest::Client,
    root: &str,
    api_key: &str,
    audio_bytes: &[u8],
    mime_type: &str,
) -> Result<Value, StepError> {
    let resp = send_with_retry("upload", api_key, Some(UPLOAD_TIMEOUT), || {
        client
            .post(format!("{root}/upload/v1beta/files"))
            .header("x-goog-api-key", api_key)
            .header("X-Goog-Upload-Protocol", "raw")
            .header("X-Goog-Upload-Header-Content-Type", mime_type)
            .header("Content-Type", mime_type)
            .body(audio_bytes.to_vec())
    })
    .await?;

    let body_text = resp
        .text()
        .await
        .map_err(|e| StepError::Fatal(anyhow!("g35t_client upload: reading body: {e}")))?;
    let v: Value = serde_json::from_str(&body_text)
        .map_err(|e| StepError::Fatal(anyhow!("g35t_client upload: non-JSON body: {e}")))?;
    let file = v.get("file").cloned().ok_or_else(|| {
        StepError::Fatal(anyhow!("g35t_client upload: missing `file` in response"))
    })?;
    if file.get("name").and_then(|n| n.as_str()).is_none() {
        return Err(StepError::Fatal(anyhow!(
            "g35t_client upload: missing file.name in response"
        )));
    }
    Ok(file)
}

#[cfg_attr(test, mutants::skip)]
async fn poll_file_ready(
    client: &reqwest::Client,
    root: &str,
    api_key: &str,
    file_name: &str,
) -> Result<Value, StepError> {
    let started = std::time::Instant::now();
    loop {
        let resp = send_with_retry("file poll", api_key, None, || {
            client
                .get(format!("{root}/v1beta/{file_name}"))
                .header("x-goog-api-key", api_key)
        })
        .await?;

        let body_text = resp
            .text()
            .await
            .map_err(|e| StepError::Fatal(anyhow!("g35t_client file poll: reading body: {e}")))?;
        let info: Value = serde_json::from_str(&body_text)
            .map_err(|e| StepError::Fatal(anyhow!("g35t_client file poll: non-JSON body: {e}")))?;
        let state = info.get("state").and_then(|s| s.as_str()).unwrap_or("");

        if state == "FAILED" {
            return Err(StepError::Fatal(anyhow!(
                "g35t_client file poll: file {file_name} processing FAILED: {info}"
            )));
        }
        if state != "PROCESSING" {
            return Ok(info);
        }
        if started.elapsed() > FILE_POLL_TIMEOUT {
            return Err(StepError::Fatal(anyhow!(
                "g35t_client file poll: {file_name} still PROCESSING after {:?}",
                FILE_POLL_TIMEOUT
            )));
        }
        sleep(FILE_POLL_INTERVAL).await;
    }
}

/// The `POST /v1beta/interactions` body for one uploaded audio file: the
/// model, the audio input and the transcription config, which is the
/// [`LANGUAGE_CODES`] hint plus verbatim mode with word timestamps. The mode
/// must ride along: a `language_codes` sent without a `mode` comes back as a
/// completed interaction with no words. Pure — unit-tested.
fn interactions_body(file_uri: &str, mime_type: &str) -> Value {
    serde_json::json!({
        "model": MODEL_SLUG,
        "input": [{"type": "audio", "uri": file_uri, "mime_type": mime_type}],
        "generation_config": {
            "transcription_config": {
                "language_codes": LANGUAGE_CODES,
                "mode": {"type": "verbatim", "timestamp_granularities": ["word"]},
            }
        }
    })
}

#[cfg_attr(test, mutants::skip)]
async fn run_interactions(
    client: &reqwest::Client,
    root: &str,
    api_key: &str,
    file_uri: &str,
    mime_type: &str,
) -> Result<Value, StepError> {
    let body = interactions_body(file_uri, mime_type);

    let resp = send_with_retry("interactions", api_key, Some(INTERACTIONS_TIMEOUT), || {
        client
            .post(format!("{root}/v1beta/interactions"))
            .header("x-goog-api-key", api_key)
            .json(&body)
    })
    .await?;

    let body_text = resp
        .text()
        .await
        .map_err(|e| StepError::Fatal(anyhow!("g35t_client interactions: reading body: {e}")))?;
    let v: Value = serde_json::from_str(&body_text)
        .map_err(|e| StepError::Fatal(anyhow!("g35t_client interactions: non-JSON body: {e}")))?;
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    if status != "completed" {
        return Err(StepError::Fatal(anyhow!(
            "g35t_client interactions: did not complete: status={status:?} id={:?}",
            v.get("id")
        )));
    }
    Ok(v)
}

/// Best-effort cleanup — never fatal, mirrors the Python prototype's
/// `delete_file` (a cleanup failure must not fail the whole transcription).
#[cfg_attr(test, mutants::skip)]
async fn delete_file_best_effort(
    client: &reqwest::Client,
    root: &str,
    api_key: &str,
    file_name: &str,
) {
    match client
        .delete(format!("{root}/v1beta/{file_name}"))
        .header("x-goog-api-key", api_key)
        .send()
        .await
    {
        Ok(resp) => {
            tracing::debug!(
                file_name,
                status = resp.status().as_u16(),
                "g35t_client: cleanup delete"
            );
        }
        Err(e) => {
            tracing::warn!(
                file_name,
                error = %e,
                "g35t_client: cleanup delete failed (non-fatal)"
            );
        }
    }
}

/// The poll → interactions → parse leg of one key attempt, run AFTER a
/// successful upload. Split out from `transcribe_with_key` (rather than an
/// inline `async {}` block) so `delete_file_best_effort` unconditionally
/// runs afterward regardless of which branch here returned — the
/// finally-equivalent for an uploaded file.
#[cfg_attr(test, mutants::skip)]
async fn transcribe_after_upload(
    client: &reqwest::Client,
    root: &str,
    api_key: &str,
    file: &Value,
    file_name: &str,
) -> Result<Vec<AsrWord>, StepError> {
    let ready = poll_file_ready(client, root, api_key, file_name).await?;
    let file_uri = ready
        .get("uri")
        .and_then(|u| u.as_str())
        .or_else(|| file.get("uri").and_then(|u| u.as_str()))
        .ok_or_else(|| StepError::Fatal(anyhow!("g35t_client: file {file_name} has no uri")))?
        .to_string();
    let mime_type = ready
        .get("mimeType")
        .and_then(|m| m.as_str())
        .or_else(|| file.get("mimeType").and_then(|m| m.as_str()))
        .unwrap_or(AUDIO_MIME_TYPE)
        .to_string();

    let response = run_interactions(client, root, api_key, &file_uri, &mime_type).await?;
    Ok(words_from_response(&response))
}

#[cfg_attr(test, mutants::skip)]
async fn transcribe_with_key(
    client: &reqwest::Client,
    root: &str,
    api_key: &str,
    audio_bytes: &[u8],
    mime_type: &str,
) -> Result<Vec<AsrWord>, StepError> {
    let file = upload_audio(client, root, api_key, audio_bytes, mime_type).await?;
    let file_name = file
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| StepError::Fatal(anyhow!("g35t_client: upload response missing file.name")))?
        .to_string();

    let result = transcribe_after_upload(client, root, api_key, &file, &file_name).await;

    delete_file_best_effort(client, root, api_key, &file_name).await;
    result
}

/// The Gemini File-API upload MIME for `path`, inferred from its extension: a
/// `.flac` uploads as `audio/flac`, everything else (the isolated-vocal `.wav`)
/// as the default `audio/wav`. `run_interactions` re-reads the stored `mimeType`
/// the upload set, so getting the upload header right is all that is needed for
/// the full-mix FLAC path (#171). Pure — unit-tested.
fn audio_mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("flac") => "audio/flac",
        _ => AUDIO_MIME_TYPE,
    }
}

/// Transcribe an audio file with Gemini 3.5 Transcribe on Google's API root,
/// returning word-level timings: the worker's call, [`transcribe_at`] on
/// [`GEMINI_API_ROOT`]. The upload MIME is inferred from the extension
/// (`.wav` → audio/wav, `.flac` → audio/flac, #171). Tries `api_keys` in
/// order (see module docs for the key-rotation / retry contract). Every
/// request hints the catalogue's languages ([`LANGUAGE_CODES`], #144); no
/// caller picks them.
#[cfg_attr(test, mutants::skip)] // Google's root; `transcribe_at` is tested on a mock
pub async fn transcribe_words(
    client: &reqwest::Client,
    api_keys: &[String],
    wav_path: &Path,
) -> Result<Vec<AsrWord>> {
    transcribe_at(client, GEMINI_API_ROOT, api_keys, wav_path)
        .await
        .map(|t| t.words)
        .map_err(|f| f.error)
}

/// [`transcribe_words`] on any API root (`api_root` without a trailing
/// slash: Google's in production, a mock server in tests), telling which key
/// answered, or on a failure which key's answer ended the call. The same
/// upload, poll, request body ([`interactions_body`]) and key rotation as
/// the worker's call, so the live probe (`g35t_probe`, #144) sends exactly
/// what a song does.
pub(crate) async fn transcribe_at(
    client: &reqwest::Client,
    api_root: &str,
    api_keys: &[String],
    audio_path: &Path,
) -> Result<Transcription, TranscribeFailure> {
    let fail = |key_index: Option<usize>, text: String| TranscribeFailure {
        key_index,
        error: anyhow!(redact_keys(&text, api_keys)),
    };
    if api_keys.is_empty() {
        let text = "g35t_client: no Gemini API keys configured".to_string();
        return Err(fail(None, text));
    }
    let audio_bytes = tokio::fs::read(audio_path).await.map_err(|e| {
        let text = format!("g35t_client: reading {}: {e}", audio_path.display());
        fail(None, text)
    })?;
    let mime_type = audio_mime_for_path(audio_path);

    let total = api_keys.len();
    let started = std::time::Instant::now();
    let mut last_refusal: Option<anyhow::Error> = None;
    for (key_index, api_key) in api_keys.iter().enumerate() {
        match transcribe_with_key(client, api_root, api_key, &audio_bytes, mime_type).await {
            Ok(words) => {
                tracing::info!(
                    key_index,
                    word_count = words.len(),
                    elapsed_s = started.elapsed().as_secs_f64(),
                    language_codes = ?LANGUAGE_CODES,
                    "g35t_client: transcription complete"
                );
                return Ok(Transcription { words, key_index });
            }
            Err(StepError::NextKey(e)) => {
                tracing::warn!(
                    key_index,
                    error = %e,
                    "g35t_client: key refused (429 or a key refusal) — trying next key"
                );
                last_refusal = Some(e);
            }
            Err(StepError::Fatal(e)) => {
                return Err(fail(Some(key_index), on_key(key_index, total, &e)));
            }
        }
    }
    // Every key was refused: the loop ran (the list is not empty) and set it.
    let last = last_refusal.unwrap_or_else(|| anyhow!("no key answered"));
    let text = format!(
        "g35t_client: all {total} keys refused; {}",
        on_key(total - 1, total, &last)
    );
    Err(fail(Some(total - 1), text))
}

/// `key i of n: <error>`: a key named by its 1-based place in the list
/// (`key_index` + 1 of `total`), never by its value.
fn on_key(key_index: usize, total: usize, error: &anyhow::Error) -> String {
    format!("key {} of {total}: {error:#}", key_index + 1)
}

/// Parse a Gemini offset string like `"5.200s"` or `"9s"` into whole
/// milliseconds. Returns `None` for a missing/malformed/negative offset.
pub fn parse_offset_ms(s: &str) -> Option<u64> {
    let numeric = s.strip_suffix('s')?;
    let seconds: f64 = numeric.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1000.0).round() as u64)
}

/// Walk `steps[*].content[*].annotations[*]` in order, collecting every
/// `type: "word_info"` annotation with usable text + timing. Anything else
/// (wrong type, empty text, unparsable offset) is skipped and logged, in
/// line with the Python prototype's `word_infos_to_words`.
pub fn words_from_response(v: &Value) -> Vec<AsrWord> {
    let mut words = Vec::new();
    let Some(steps) = v.get("steps").and_then(|s| s.as_array()) else {
        return words;
    };
    for step in steps {
        let Some(contents) = step.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for content in contents {
            let Some(annotations) = content.get("annotations").and_then(|a| a.as_array()) else {
                continue;
            };
            for ann in annotations {
                if ann.get("type").and_then(|t| t.as_str()) != Some("word_info") {
                    continue;
                }
                let text = ann
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .trim();
                let start_ms = ann
                    .get("start_offset")
                    .and_then(|o| o.as_str())
                    .and_then(parse_offset_ms);
                let end_ms = ann
                    .get("end_offset")
                    .and_then(|o| o.as_str())
                    .and_then(parse_offset_ms);
                match (start_ms, end_ms) {
                    (Some(start_ms), Some(end_ms)) if !text.is_empty() => {
                        words.push(AsrWord {
                            text: text.to_string(),
                            start_ms,
                            end_ms,
                        });
                    }
                    _ => {
                        tracing::warn!(annotation = %ann, "g35t_client: skipping malformed word_info");
                    }
                }
            }
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #136: the shared key verdict maps onto this client's two outcomes.
    #[test]
    fn a_refused_reply_maps_to_next_key_or_fatal() {
        let text = |e: StepError| match e {
            StepError::NextKey(e) => format!("next: {e}"),
            StepError::Fatal(e) => format!("fatal: {e}"),
        };
        let long = "x".repeat(500);
        assert_eq!(
            text(step_error(
                "upload",
                KeyVerdict::NextKey { rate_limited: true },
                429,
                "quota"
            )),
            "next: g35t_client upload: key refused status=429 body=quota"
        );
        assert_eq!(
            text(step_error("poll", KeyVerdict::RetrySameKey, 503, "down")),
            "fatal: g35t_client poll: exhausted retries status=503 body=down"
        );
        assert_eq!(
            text(step_error("interactions", KeyVerdict::Stop, 404, &long)),
            format!(
                "fatal: g35t_client interactions: unexpected status=404 body={}",
                "x".repeat(400)
            )
        );
    }

    /// #144: every request hints the catalogue's English AND Spanish, so a
    /// Spanish song is no longer transcribed under an English-only hint. The
    /// whole body is pinned: the verbatim word-timestamp mode must ride along
    /// with the hint (a hint without a `mode` returns no words).
    #[test]
    fn interactions_body_hints_the_catalogue_s_english_and_spanish() {
        let body = interactions_body("https://files.example/abc", "audio/flac");
        assert_eq!(
            body,
            serde_json::json!({
                "model": "gemini-3.5-transcribe",
                "input": [{
                    "type": "audio",
                    "uri": "https://files.example/abc",
                    "mime_type": "audio/flac"
                }],
                "generation_config": {
                    "transcription_config": {
                        "language_codes": ["en-US", "es-419"],
                        "mode": {"type": "verbatim", "timestamp_granularities": ["word"]}
                    }
                }
            })
        );
    }

    #[test]
    fn parse_offset_ms_parses_standard_and_bare_seconds() {
        assert_eq!(parse_offset_ms("5.200s"), Some(5200));
        assert_eq!(parse_offset_ms("9s"), Some(9000));
        assert_eq!(parse_offset_ms("0.05s"), Some(50));
    }

    #[test]
    fn parse_offset_ms_rejects_malformed_or_missing_suffix() {
        assert_eq!(parse_offset_ms("x"), None);
        assert_eq!(parse_offset_ms(""), None);
        assert_eq!(parse_offset_ms("5.2"), None);
        assert_eq!(parse_offset_ms("-1s"), None);
    }

    #[test]
    fn words_from_response_skips_malformed_and_non_word_annotations() {
        let response: Value = serde_json::json!({
            "steps": [
                {
                    "content": [
                        {
                            "annotations": [
                                {"type": "word_info", "text": "Nothing", "start_offset": "5.200s", "end_offset": "9s"},
                                {"type": "other", "text": "ignored", "start_offset": "1s", "end_offset": "2s"}
                            ]
                        }
                    ]
                },
                {
                    "content": [
                        {
                            "annotations": [
                                {"type": "word_info", "text": "compares", "start_offset": "9s", "end_offset": "9.6s"},
                                {"type": "word_info", "text": "", "start_offset": "9.6s", "end_offset": "10s"}
                            ]
                        }
                    ]
                }
            ]
        });

        let words = words_from_response(&response);
        assert_eq!(
            words.len(),
            2,
            "the non-word_info and empty-text entries must be skipped"
        );
        assert_eq!(words[0].text, "Nothing");
        assert_eq!(words[0].start_ms, 5200);
        assert_eq!(words[0].end_ms, 9000);
        assert_eq!(words[1].text, "compares");
        assert_eq!(words[1].start_ms, 9000);
        assert_eq!(words[1].end_ms, 9600);
    }

    #[test]
    fn words_from_response_empty_on_missing_steps() {
        let response: Value = serde_json::json!({"status": "completed"});
        assert!(words_from_response(&response).is_empty());
    }
}

#[cfg(test)]
#[path = "g35t_client_tests_mutants.rs"]
mod g35t_client_tests_mutants;
