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
//! info on success.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::time::sleep;

use crate::gemini_api::{
    GEMINI_API_ROOT as API_ROOT, KeyReply, KeyVerdict, RETRY_BACKOFFS, send_on_key,
};

const MODEL_SLUG: &str = "gemini-3.5-transcribe";
const AUDIO_MIME_TYPE: &str = "audio/wav";

/// #144: the BCP-47 `transcription_config.language_codes` every request
/// carries. The API reference reads them as "hints about the languages
/// present in the audio" (omitted or empty = automatic detection), and the
/// transcribe guide says to pass them whenever the language is known.
const LANGUAGE_CODES: &[&str] = &["en-US"];

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

/// Outcome of a single HTTP step against one API key.
enum StepError {
    /// `KeyVerdict::NextKey` (a 429 or a key refusal) — the caller should try
    /// the NEXT key in `api_keys`.
    NextKey(anyhow::Error),
    /// Any other failure — abort `transcribe_words` entirely.
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
/// `reqwest::RequestBuilder` is consumed by `.send()`.
// The loop itself is unit-tested in `gemini_api_tests.rs` against a mock
// server; this mapping only wraps it (the API root here is a constant).
#[cfg_attr(test, mutants::skip)]
async fn send_with_retry(
    what: &str,
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
        } => Err(step_error(what, verdict, status, &body)),
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
    api_key: &str,
    audio_bytes: &[u8],
    mime_type: &str,
) -> Result<Value, StepError> {
    let resp = send_with_retry("upload", Some(UPLOAD_TIMEOUT), || {
        client
            .post(format!("{API_ROOT}/upload/v1beta/files"))
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
    api_key: &str,
    file_name: &str,
) -> Result<Value, StepError> {
    let started = std::time::Instant::now();
    loop {
        let resp = send_with_retry("file poll", None, || {
            client
                .get(format!("{API_ROOT}/v1beta/{file_name}"))
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
    api_key: &str,
    file_uri: &str,
    mime_type: &str,
) -> Result<Value, StepError> {
    let body = interactions_body(file_uri, mime_type);

    let resp = send_with_retry("interactions", Some(INTERACTIONS_TIMEOUT), || {
        client
            .post(format!("{API_ROOT}/v1beta/interactions"))
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
async fn delete_file_best_effort(client: &reqwest::Client, api_key: &str, file_name: &str) {
    match client
        .delete(format!("{API_ROOT}/v1beta/{file_name}"))
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
    api_key: &str,
    file: &Value,
    file_name: &str,
) -> Result<Vec<AsrWord>, StepError> {
    let ready = poll_file_ready(client, api_key, file_name).await?;
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

    let response = run_interactions(client, api_key, &file_uri, &mime_type).await?;
    Ok(words_from_response(&response))
}

#[cfg_attr(test, mutants::skip)]
async fn transcribe_with_key(
    client: &reqwest::Client,
    api_key: &str,
    audio_bytes: &[u8],
    mime_type: &str,
) -> Result<Vec<AsrWord>, StepError> {
    let file = upload_audio(client, api_key, audio_bytes, mime_type).await?;
    let file_name = file
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| StepError::Fatal(anyhow!("g35t_client: upload response missing file.name")))?
        .to_string();

    let result = transcribe_after_upload(client, api_key, &file, &file_name).await;

    delete_file_best_effort(client, api_key, &file_name).await;
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

/// Transcribe an audio file with Gemini 3.5 Transcribe, returning word-level
/// timings. The upload MIME is inferred from the extension (`.wav` → audio/wav,
/// `.flac` → audio/flac, #171). Tries `api_keys` in order (see module docs for
/// the key-rotation / retry contract). Every request hints the catalogue's
/// languages ([`LANGUAGE_CODES`], #144); no caller picks them.
#[cfg_attr(test, mutants::skip)]
pub async fn transcribe_words(
    client: &reqwest::Client,
    api_keys: &[String],
    wav_path: &Path,
) -> Result<Vec<AsrWord>> {
    if api_keys.is_empty() {
        bail!("g35t_client: no Gemini API keys configured");
    }
    let audio_bytes = tokio::fs::read(wav_path)
        .await
        .with_context(|| format!("g35t_client: reading {}", wav_path.display()))?;
    let mime_type = audio_mime_for_path(wav_path);

    let started = std::time::Instant::now();
    let mut last_err: Option<anyhow::Error> = None;
    for (key_idx, api_key) in api_keys.iter().enumerate() {
        match transcribe_with_key(client, api_key, &audio_bytes, mime_type).await {
            Ok(words) => {
                tracing::info!(
                    key_index = key_idx,
                    word_count = words.len(),
                    elapsed_s = started.elapsed().as_secs_f64(),
                    language_codes = ?LANGUAGE_CODES,
                    "g35t_client: transcription complete"
                );
                return Ok(words);
            }
            Err(StepError::NextKey(e)) => {
                tracing::warn!(
                    key_index = key_idx,
                    error = %e,
                    "g35t_client: key refused (429 or a key refusal) — trying next key"
                );
                last_err = Some(e);
                continue;
            }
            Err(StepError::Fatal(e)) => {
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("g35t_client: all API keys exhausted")))
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
