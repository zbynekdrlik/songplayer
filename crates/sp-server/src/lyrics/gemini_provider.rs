//! Gemini-based AlignmentProvider. Slices the Demucs-dereverbed vocal WAV
//! into 60 s chunks with 10 s overlap, calls Gemini 3.x Pro per chunk via the
//! `GeminiClient`, parses responses with `parse_timed_lines`, and merges per
//! `merge_overlap`. Produces line-level timings only (word vectors empty for
//! MVP — word-level work is deferred to a later PR).
//!
//! **v14 multi-key rotation:** `clients: Vec<GeminiClient>` holds one direct-API
//! client per configured `gemini_api_key`. `transcribe_rotating` tries the
//! last-successful key first; on HTTP 429 it advances to the next key and
//! retries. On non-quota errors it fails immediately. An `AtomicUsize` tracks
//! the last-successful key so subsequent chunks skip already-exhausted keys
//! within the same song. Restart starts from key 0 again (one extra 429 burst
//! on cold start — acceptable).

use crate::lyrics::gemini_chunks::{merge_overlap, plan_chunks};
use crate::lyrics::gemini_client::GeminiClient;
use crate::lyrics::gemini_parse::parse_timed_lines;
use crate::lyrics::gemini_prompt::build_prompt;
use crate::lyrics::provider::{
    AlignmentProvider, CandidateText, LineTiming, ProviderResult, SongContext, WordTiming,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, warn};

pub struct GeminiProvider {
    /// One client per configured API key. Rotation picks the next available
    /// key on HTTP 429. An empty list means "Gemini disabled"; the provider's
    /// `can_provide` still returns true if a vocal is present, and `align`
    /// fails explicitly so the orchestrator records the attempt as a miss
    /// rather than silently no-oping.
    pub clients: Vec<GeminiClient>,
    /// Sticky index — points to the last-successful key so the next chunk
    /// starts from that key instead of always re-trying key 0 first.
    pub current_key_idx: Arc<AtomicUsize>,
    pub ffmpeg_path: PathBuf,
    pub cache_dir: PathBuf,
}

#[async_trait]
impl AlignmentProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn base_confidence(&self) -> f32 {
        // Treated as the sole line-timing source while qwen3 is disabled.
        0.9
    }

    async fn can_provide(&self, ctx: &SongContext) -> bool {
        ctx.clean_vocal_path.as_ref().is_some_and(|p| p.exists())
    }

    // I/O-heavy orchestrator — per-chunk call+parse+merge pipeline. Individual
    // pieces (prompt builder, parser, chunk planner, merger, HTTP client, key
    // rotation) are exhaustively unit-tested. The body here is glue + ffmpeg
    // subprocess.
    #[cfg_attr(test, mutants::skip)]
    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> {
        let vocal = ctx
            .clean_vocal_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gemini: clean_vocal_path missing"))?;
        if self.clients.is_empty() {
            anyhow::bail!("gemini: no API keys configured");
        }
        let reference = gather_reference_text(&ctx.candidate_texts);
        let plans = plan_chunks(ctx.duration_ms);
        if plans.is_empty() {
            anyhow::bail!("gemini: duration_ms is 0, nothing to transcribe");
        }

        // v20: retry-from-cache. Load the song's previous `_gemini_chunks.json`
        // if any. For each planned chunk, if the cached raw is non-empty we
        // reuse it (no Gemini call); if the cached raw is empty or missing,
        // we call Gemini fresh. This saves quota + time on songs where only
        // 1 or 2 chunks previously failed. Matches the Python prototype's
        // `--retry-from-cache` behavior.
        let cached = load_cached_chunks(&self.cache_dir, &ctx.video_id, &plans).await;

        let tmp = tempfile::tempdir().context("create chunk tmp dir")?;
        let mut per_chunk: Vec<Vec<crate::lyrics::gemini_parse::ParsedLine>> =
            Vec::with_capacity(plans.len());
        let mut raw_cache_entries: Vec<RawChunk> = Vec::with_capacity(plans.len());

        for (chunk_idx, plan) in plans.iter().enumerate() {
            // If the cache already has this chunk, reuse it and skip the
            // Gemini call entirely.
            if let Some(raw) = cached.get(chunk_idx).and_then(|s| s.as_ref())
                && !raw.is_empty()
            {
                let parsed = parse_timed_lines(raw);
                debug!(
                    chunk = plan.idx,
                    parsed = parsed.len(),
                    "gemini: chunk reused from cache"
                );
                per_chunk.push(parsed);
                raw_cache_entries.push(RawChunk {
                    start_ms: plan.start_ms,
                    end_ms: plan.end_ms,
                    raw: raw.clone(),
                });
                continue;
            }

            if chunk_idx > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
            let chunk_wav = tmp.path().join(format!("chunk_{:02}.wav", plan.idx));
            if let Err(e) = slice_chunk(
                &self.ffmpeg_path,
                vocal,
                plan.start_ms,
                plan.end_ms,
                &chunk_wav,
            )
            .await
            {
                warn!("gemini: chunk {} slice failed: {e}", plan.idx);
                per_chunk.push(Vec::new());
                raw_cache_entries.push(RawChunk {
                    start_ms: plan.start_ms,
                    end_ms: plan.end_ms,
                    raw: String::new(),
                });
                continue;
            }
            let prompt = build_prompt(&reference, plan.start_ms, plan.end_ms, ctx.duration_ms);
            debug!(chunk = plan.idx, "gemini: calling Gemini for chunk");
            // v19: one retry per chunk on failure. A single transient 5-minute
            // timeout (observed on Not Guilty chunk 2) was dropping the whole
            // chunk → 40 s gap in the rendered lyrics. Retrying once usually
            // succeeds because timeouts are not correlated.
            let mut attempt = 0;
            let outcome = loop {
                attempt += 1;
                match transcribe_rotating(
                    &self.clients,
                    self.current_key_idx.as_ref(),
                    &prompt,
                    &chunk_wav,
                )
                .await
                {
                    Ok(raw) => break Ok(raw),
                    Err(e) if attempt < 2 => {
                        warn!(
                            chunk = plan.idx,
                            attempt, "gemini: chunk failed, retrying once: {e}"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    Err(e) => break Err(e),
                }
            };
            match outcome {
                Ok(raw) => {
                    let parsed = parse_timed_lines(&raw);
                    debug!(
                        chunk = plan.idx,
                        parsed = parsed.len(),
                        "gemini: chunk parsed"
                    );
                    per_chunk.push(parsed);
                    raw_cache_entries.push(RawChunk {
                        start_ms: plan.start_ms,
                        end_ms: plan.end_ms,
                        raw,
                    });
                }
                Err(e) => {
                    warn!("gemini: chunk {} call failed after retry: {e}", plan.idx);
                    per_chunk.push(Vec::new());
                    raw_cache_entries.push(RawChunk {
                        start_ms: plan.start_ms,
                        end_ms: plan.end_ms,
                        raw: String::new(),
                    });
                }
            }
        }

        // v20: fail the whole alignment if any chunk is still empty after
        // retry. Persisting partial output creates visible gaps in the
        // rendered lyrics on Resolume ("lyrics totally left and again
        // appear"), which looks broken to the audience. Failing here keeps
        // the song `has_lyrics = 0` so the worker retries it later from
        // cache (empty chunks will be re-called; good ones reused).
        let empty_chunks: Vec<usize> = raw_cache_entries
            .iter()
            .enumerate()
            .filter(|(_, c)| c.raw.is_empty())
            .map(|(i, _)| i)
            .collect();
        if !empty_chunks.is_empty() {
            // Still write the raw cache so the NEXT attempt can reuse the
            // chunks that did succeed this time around.
            if let Err(e) =
                write_raw_cache(&self.cache_dir, &ctx.video_id, &raw_cache_entries).await
            {
                warn!("gemini: raw cache write failed: {e}");
            }
            anyhow::bail!(
                "gemini: {} of {} chunks failed after retry (indices {:?}); not persisting",
                empty_chunks.len(),
                plans.len(),
                empty_chunks
            );
        }

        if let Err(e) = write_raw_cache(&self.cache_dir, &ctx.video_id, &raw_cache_entries).await {
            warn!("gemini: raw cache write failed: {e}");
        }

        let merged = merge_overlap(&plans, &per_chunk);
        if merged.is_empty() {
            anyhow::bail!("gemini: no lines produced from any chunk");
        }

        let lines: Vec<LineTiming> = merged
            .into_iter()
            .map(|g| LineTiming {
                text: g.text,
                start_ms: g.start_ms,
                end_ms: g.end_ms,
                words: Vec::<WordTiming>::new(),
            })
            .collect();

        Ok(ProviderResult {
            provider_name: self.name().into(),
            lines,
            metadata: serde_json::json!({
                "base_confidence": self.base_confidence(),
                "chunks": plans.len(),
                "keys_configured": self.clients.len(),
            }),
        })
    }
}

/// Try to transcribe a chunk, rotating across `clients` on HTTP 429.
///
/// Starts at `start_idx` (wrapping). On 429 from a key, moves to the next
/// key and retries. On success, updates `start_idx` to the successful key
/// so the next call starts from there. On non-quota errors, bails
/// immediately — server-side issues won't be fixed by trying another key.
///
/// Returns an error iff every key returned 429 (or the list is empty).
pub async fn transcribe_rotating(
    clients: &[GeminiClient],
    start_idx: &AtomicUsize,
    prompt: &str,
    audio: &Path,
) -> Result<String> {
    if clients.is_empty() {
        anyhow::bail!("transcribe_rotating: no clients");
    }
    let start = start_idx.load(Ordering::Relaxed) % clients.len();
    let mut last_err: Option<anyhow::Error> = None;
    for offset in 0..clients.len() {
        let idx = (start + offset) % clients.len();
        match clients[idx].transcribe_chunk(prompt, audio).await {
            Ok(s) => {
                start_idx.store(idx, Ordering::Relaxed);
                return Ok(s);
            }
            Err(e) => {
                // Only rotate on 429 (per-key quota). Every other error is
                // server-side or programming — more keys won't help.
                if is_quota_429(&e) {
                    warn!(
                        key_idx = idx,
                        total = clients.len(),
                        "gemini: key {} exhausted (429), rotating",
                        idx
                    );
                    last_err = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("all {} gemini keys exhausted", clients.len())))
}

/// True if the error message contains a 429 signature (either the raw status
/// line from `GeminiClient` or a RESOURCE_EXHAUSTED body).
pub fn is_quota_429(e: &anyhow::Error) -> bool {
    let s = format!("{e}");
    s.contains("HTTP 429") || s.contains("RESOURCE_EXHAUSTED")
}

/// Pick the best reference text from candidate_texts. Prefers `source="description"`
/// (clean text from YouTube descriptions); falls back to whichever candidate has
/// the most lines. Returns a placeholder if no candidates are available.
pub fn gather_reference_text(candidates: &[CandidateText]) -> String {
    let pick = candidates
        .iter()
        .find(|c| c.source == "description")
        .or_else(|| candidates.iter().max_by_key(|c| c.lines.len()));
    match pick {
        Some(c) if !c.lines.is_empty() => c.lines.join("\n"),
        _ => "(no reference lyrics available for this song)".to_string(),
    }
}

// Pure ffmpeg subprocess wrapper — covered by manual verification + the ffmpeg
// binary's own tests. Mutation skip is for the I/O body.
#[cfg_attr(test, mutants::skip)]
async fn slice_chunk(
    ffmpeg: &Path,
    input: &Path,
    start_ms: u64,
    end_ms: u64,
    out: &Path,
) -> Result<()> {
    let dur_s = (end_ms - start_ms) as f64 / 1000.0;
    let ss_s = start_ms as f64 / 1000.0;
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-ss",
        &format!("{ss_s}"),
        "-t",
        &format!("{dur_s}"),
        "-i",
    ])
    .arg(input)
    .args(["-c:a", "pcm_s16le"])
    .arg(out)
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().await.context("run ffmpeg for chunk slice")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg failed: {err}");
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct RawChunk {
    start_ms: u64,
    end_ms: u64,
    raw: String,
}

#[cfg_attr(test, mutants::skip)]
async fn write_raw_cache(cache_dir: &Path, video_id: &str, chunks: &[RawChunk]) -> Result<()> {
    let path = cache_dir.join(format!("{video_id}_gemini_chunks.json"));
    let body = serde_json::json!({"chunks": chunks});
    tokio::fs::write(&path, serde_json::to_string_pretty(&body)?).await?;
    Ok(())
}

/// Load previously-cached per-chunk raw responses for this song.
///
/// Returns `Vec<Option<String>>` aligned 1:1 with `plans` — `Some(raw)` means
/// the cache had a response for that chunk boundary (empty string = previous
/// run failed and needs retry; non-empty = reuse). `None` means the cache did
/// not contain a matching chunk plan (e.g. first run, or a different chunking
/// scheme was used before). On any read/parse error, returns a vec of `None`
/// so the caller falls through to fresh Gemini calls.
#[cfg_attr(test, mutants::skip)]
async fn load_cached_chunks(
    cache_dir: &Path,
    video_id: &str,
    plans: &[crate::lyrics::gemini_chunks::ChunkPlan],
) -> Vec<Option<String>> {
    let path = cache_dir.join(format!("{video_id}_gemini_chunks.json"));
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return vec![None; plans.len()];
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return vec![None; plans.len()];
    };
    let Some(cached) = doc.get("chunks").and_then(|c| c.as_array()) else {
        return vec![None; plans.len()];
    };
    plans
        .iter()
        .map(|p| {
            cached.iter().find_map(|c| {
                let start = c.get("start_ms")?.as_u64()?;
                let end = c.get("end_ms")?.as_u64()?;
                if start != p.start_ms || end != p.end_ms {
                    return None;
                }
                Some(c.get("raw")?.as_str()?.to_string())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::provider::CandidateText;

    fn ctext(source: &str, lines: Vec<&str>) -> CandidateText {
        CandidateText {
            source: source.into(),
            lines: lines.into_iter().map(String::from).collect(),
            has_timing: false,
            line_timings: None,
        }
    }

    #[test]
    fn gather_reference_text_prefers_description() {
        let cands = vec![
            ctext("autosub", vec!["a"]),
            ctext("description", vec!["b", "c"]),
        ];
        assert_eq!(gather_reference_text(&cands), "b\nc");
    }

    #[test]
    fn gather_reference_text_falls_back_to_longest() {
        let cands = vec![ctext("autosub", vec!["a", "b"]), ctext("lrclib", vec!["c"])];
        assert_eq!(gather_reference_text(&cands), "a\nb");
    }

    #[test]
    fn gather_reference_text_empty_returns_placeholder() {
        assert!(gather_reference_text(&[]).contains("no reference"));
    }

    #[test]
    fn gather_reference_text_all_empty_returns_placeholder() {
        let cands = vec![ctext("description", vec![]), ctext("autosub", vec![])];
        assert!(gather_reference_text(&cands).contains("no reference"));
    }

    #[test]
    fn is_quota_429_matches_common_shapes() {
        assert!(is_quota_429(&anyhow::anyhow!("HTTP 429: quota exhausted")));
        assert!(is_quota_429(&anyhow::anyhow!("status: RESOURCE_EXHAUSTED")));
        assert!(!is_quota_429(&anyhow::anyhow!("HTTP 500 server error")));
        assert!(!is_quota_429(&anyhow::anyhow!("connection refused")));
    }

    /// First key returns 429, second key returns 200 — rotation must try both
    /// and return the second key's successful response.
    #[tokio::test]
    async fn rotation_advances_on_429_and_returns_next_key_success() {
        use tempfile::NamedTempFile;
        use wiremock::matchers::{header, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Key "BAD" → 429
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:generateContent"))
            .and(header("x-goog-api-key", "BAD"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}
            })))
            .mount(&server)
            .await;
        // Key "GOOD" → 200 with a parseable response
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:generateContent"))
            .and(header("x-goog-api-key", "GOOD"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": {"parts": [{"text": "(00:00 --> 00:05) hello"}]}
                }]
            })))
            .mount(&server)
            .await;

        // Two clients sharing the same mock URL, different keys. max_attempts=1
        // so 429 fails fast and the provider-level rotation takes over.
        let mk = |key: &str| GeminiClient {
            base_url: server.uri(),
            model: "gemini-3.1-pro-preview".to_string(),
            api_key: Some(key.to_string()),
            timeout_s: 10,
            base_retry_ms: 1,
            max_attempts: 1,
        };
        let clients = vec![mk("BAD"), mk("GOOD")];
        let idx = AtomicUsize::new(0);

        // Tiny WAV placeholder — GeminiClient only reads bytes + base64s.
        let wav = NamedTempFile::new().unwrap();
        std::fs::write(wav.path(), b"fake-wav").unwrap();

        let result = transcribe_rotating(&clients, &idx, "hi", wav.path()).await;
        assert!(result.is_ok(), "rotation must succeed, got {result:?}");
        assert!(result.unwrap().contains("hello"));
        // Sticky index now points to the successful key.
        assert_eq!(idx.load(Ordering::Relaxed), 1);
    }

    /// All keys return 429 — rotation must try each once, then give up.
    #[tokio::test]
    async fn rotation_fails_when_every_key_429s() {
        use tempfile::NamedTempFile;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:generateContent"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}
            })))
            .expect(3) // 3 keys, each tried exactly once
            .mount(&server)
            .await;

        let mk = |key: &str| GeminiClient {
            base_url: server.uri(),
            model: "m".to_string(),
            api_key: Some(key.to_string()),
            timeout_s: 10,
            base_retry_ms: 1,
            max_attempts: 1,
        };
        let clients = vec![mk("K1"), mk("K2"), mk("K3")];
        let idx = AtomicUsize::new(0);
        let wav = NamedTempFile::new().unwrap();
        std::fs::write(wav.path(), b"fake-wav").unwrap();

        let result = transcribe_rotating(&clients, &idx, "hi", wav.path()).await;
        assert!(result.is_err(), "all-429 must fail");
    }

    /// Non-429 error (e.g. 500) must NOT rotate — server-side issue, more
    /// keys won't help. Fail fast on the first key's response.
    #[tokio::test]
    async fn rotation_does_not_advance_on_server_error() {
        use tempfile::NamedTempFile;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:generateContent"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1) // only the first key should be tried
            .mount(&server)
            .await;

        let mk = |key: &str| GeminiClient {
            base_url: server.uri(),
            model: "m".to_string(),
            api_key: Some(key.to_string()),
            timeout_s: 10,
            base_retry_ms: 1,
            max_attempts: 1,
        };
        let clients = vec![mk("K1"), mk("K2")];
        let idx = AtomicUsize::new(0);
        let wav = NamedTempFile::new().unwrap();
        std::fs::write(wav.path(), b"fake-wav").unwrap();

        let result = transcribe_rotating(&clients, &idx, "hi", wav.path()).await;
        assert!(result.is_err(), "500 must propagate");
        assert!(
            !is_quota_429(&result.unwrap_err()),
            "error must not be tagged as quota"
        );
    }

    /// After a successful call, the sticky index stays on the working key so
    /// the next chunk starts there instead of re-hitting an exhausted key.
    #[tokio::test]
    async fn rotation_sticky_index_reuses_working_key() {
        use tempfile::NamedTempFile;
        use wiremock::matchers::{header, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // K1 always 429
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:generateContent"))
            .and(header("x-goog-api-key", "K1"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1) // only hit on the first call; second call skips K1
            .mount(&server)
            .await;
        // K2 always 200
        Mock::given(method("POST"))
            .and(path_regex(r"/v1beta/models/.*:generateContent"))
            .and(header("x-goog-api-key", "K2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}]
            })))
            .expect(2) // hit on both calls
            .mount(&server)
            .await;

        let mk = |key: &str| GeminiClient {
            base_url: server.uri(),
            model: "m".to_string(),
            api_key: Some(key.to_string()),
            timeout_s: 10,
            base_retry_ms: 1,
            max_attempts: 1,
        };
        let clients = vec![mk("K1"), mk("K2")];
        let idx = AtomicUsize::new(0);
        let wav = NamedTempFile::new().unwrap();
        std::fs::write(wav.path(), b"fake-wav").unwrap();

        // First chunk: 0→429 on K1, rotate → K2 → 200. idx sticks to 1.
        let r1 = transcribe_rotating(&clients, &idx, "c1", wav.path()).await;
        assert!(r1.is_ok());
        assert_eq!(idx.load(Ordering::Relaxed), 1);

        // Second chunk: starts at idx=1 → K2 → 200 directly. K1 not retried.
        let r2 = transcribe_rotating(&clients, &idx, "c2", wav.path()).await;
        assert!(r2.is_ok());
        assert_eq!(idx.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn rotation_rejects_empty_client_list() {
        use tempfile::NamedTempFile;
        let idx = AtomicUsize::new(0);
        let wav = NamedTempFile::new().unwrap();
        let result = transcribe_rotating(&[], &idx, "x", wav.path()).await;
        assert!(result.is_err());
    }
}
