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
//! query param). `api_keys` is tried in order: an HTTP 429, or a 400 whose
//! body contains "API key not valid", moves to the NEXT key; a 5xx retries
//! the SAME key with backoff (2/4/8/16s, up to 4 retries). Any other
//! failure aborts immediately. Never log a header or a key value — only
//! the key's INDEX; log word count + elapsed time at info on success.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::time::sleep;

const API_ROOT: &str = "https://generativelanguage.googleapis.com";
const MODEL_SLUG: &str = "gemini-3.5-transcribe";
const AUDIO_MIME_TYPE: &str = "audio/wav";

const FILE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const FILE_POLL_TIMEOUT: Duration = Duration::from_secs(60);

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const INTERACTIONS_TIMEOUT: Duration = Duration::from_secs(600);

/// Exponential backoff schedule for a 5xx retry on the SAME key: up to 4
/// retries (5 attempts total) at 2/4/8/16s.
const RETRY_BACKOFFS_S: [u64; 4] = [2, 4, 8, 16];

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
    /// 429, or a 400 whose body contains "API key not valid" — the caller
    /// should try the NEXT key in `api_keys`.
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

/// Issue one HTTP call, retrying on a 5xx with the fixed backoff schedule
/// (same key, same request). A 429 or a "API key not valid" 400 is
/// classified as `StepError::NextKey` immediately — no retry, since the
/// same key retrying would fail identically. `build` is invoked fresh on
/// every attempt because a `reqwest::RequestBuilder` is consumed by
/// `.send()`.
// Network-bound: every branch requires a live HTTP server; covered by the
// Python prototype's live verification (2026-09-12) referenced above, not
// a local unit test. See replicate_client.rs / metadata/gemini.rs for the
// same pattern.
#[cfg_attr(test, mutants::skip)]
async fn send_with_retry(
    what: &str,
    timeout: Option<Duration>,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, StepError> {
    let mut attempt: usize = 0;
    loop {
        let mut req = build();
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| StepError::Fatal(anyhow!("g35t_client {what}: request failed: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status.as_u16() == 429 {
            return Err(StepError::NextKey(anyhow!(
                "g35t_client {what}: HTTP 429 rate-limited"
            )));
        }
        if status.as_u16() == 400 {
            let body = resp.text().await.unwrap_or_default();
            if body.contains("API key not valid") {
                return Err(StepError::NextKey(anyhow!(
                    "g35t_client {what}: HTTP 400 API key not valid"
                )));
            }
            return Err(StepError::Fatal(anyhow!(
                "g35t_client {what}: HTTP 400: {}",
                truncate(&body, 400)
            )));
        }
        if status.is_server_error() {
            if attempt >= RETRY_BACKOFFS_S.len() {
                let body = resp.text().await.unwrap_or_default();
                return Err(StepError::Fatal(anyhow!(
                    "g35t_client {what}: exhausted retries status={status} body={}",
                    truncate(&body, 400)
                )));
            }
            let backoff = Duration::from_secs(RETRY_BACKOFFS_S[attempt]);
            tracing::warn!(
                what,
                attempt = attempt + 1,
                status = status.as_u16(),
                backoff_s = backoff.as_secs(),
                "g35t_client: 5xx — retrying same key"
            );
            sleep(backoff).await;
            attempt += 1;
            continue;
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(StepError::Fatal(anyhow!(
            "g35t_client {what}: unexpected status={status} body={}",
            truncate(&body, 400)
        )));
    }
}

#[cfg_attr(test, mutants::skip)]
async fn upload_audio(
    client: &reqwest::Client,
    api_key: &str,
    wav_bytes: &[u8],
) -> Result<Value, StepError> {
    let resp = send_with_retry("upload", Some(UPLOAD_TIMEOUT), || {
        client
            .post(format!("{API_ROOT}/upload/v1beta/files"))
            .header("x-goog-api-key", api_key)
            .header("X-Goog-Upload-Protocol", "raw")
            .header("X-Goog-Upload-Header-Content-Type", AUDIO_MIME_TYPE)
            .header("Content-Type", AUDIO_MIME_TYPE)
            .body(wav_bytes.to_vec())
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

#[cfg_attr(test, mutants::skip)]
async fn run_interactions(
    client: &reqwest::Client,
    api_key: &str,
    file_uri: &str,
    mime_type: &str,
    language_codes: &[String],
) -> Result<Value, StepError> {
    let body = serde_json::json!({
        "model": MODEL_SLUG,
        "input": [{"type": "audio", "uri": file_uri, "mime_type": mime_type}],
        "generation_config": {
            "transcription_config": {
                "language_codes": language_codes,
                "mode": {"type": "verbatim", "timestamp_granularities": ["word"]},
            }
        }
    });

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
    language_codes: &[String],
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

    let response = run_interactions(client, api_key, &file_uri, &mime_type, language_codes).await?;
    Ok(words_from_response(&response))
}

#[cfg_attr(test, mutants::skip)]
async fn transcribe_with_key(
    client: &reqwest::Client,
    api_key: &str,
    wav_bytes: &[u8],
    language_codes: &[String],
) -> Result<Vec<AsrWord>, StepError> {
    let file = upload_audio(client, api_key, wav_bytes).await?;
    let file_name = file
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| StepError::Fatal(anyhow!("g35t_client: upload response missing file.name")))?
        .to_string();

    let result = transcribe_after_upload(client, api_key, &file, &file_name, language_codes).await;

    delete_file_best_effort(client, api_key, &file_name).await;
    result
}

/// Transcribe a mono 16 kHz WAV with Gemini 3.5 Transcribe, returning
/// word-level timings. Tries `api_keys` in order (see module docs for the
/// key-rotation / retry contract).
#[cfg_attr(test, mutants::skip)]
pub async fn transcribe_words(
    client: &reqwest::Client,
    api_keys: &[String],
    wav_path: &Path,
    language_codes: &[String],
) -> Result<Vec<AsrWord>> {
    if api_keys.is_empty() {
        bail!("g35t_client: no Gemini API keys configured");
    }
    let wav_bytes = tokio::fs::read(wav_path)
        .await
        .with_context(|| format!("g35t_client: reading {}", wav_path.display()))?;

    let started = std::time::Instant::now();
    let mut last_err: Option<anyhow::Error> = None;
    for (key_idx, api_key) in api_keys.iter().enumerate() {
        match transcribe_with_key(client, api_key, &wav_bytes, language_codes).await {
            Ok(words) => {
                tracing::info!(
                    key_index = key_idx,
                    word_count = words.len(),
                    elapsed_s = started.elapsed().as_secs_f64(),
                    "g35t_client: transcription complete"
                );
                return Ok(words);
            }
            Err(StepError::NextKey(e)) => {
                tracing::warn!(
                    key_index = key_idx,
                    error = %e,
                    "g35t_client: key rejected (429/invalid) — trying next key"
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

/// Split a `gemini_api_key` DB setting (comma-separated) into a trimmed,
/// non-empty key list, in order.
pub fn gemini_keys_from_setting(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn gemini_keys_from_setting_splits_trims_and_drops_empties() {
        assert_eq!(
            gemini_keys_from_setting(" key1 , key2,, key3 "),
            vec!["key1".to_string(), "key2".to_string(), "key3".to_string()]
        );
        assert_eq!(gemini_keys_from_setting(""), Vec::<String>::new());
        assert_eq!(
            gemini_keys_from_setting("onlyone"),
            vec!["onlyone".to_string()]
        );
    }
}
