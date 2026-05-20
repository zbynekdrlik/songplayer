//! Integration tests for the asr_path orchestrator + the three
//! forbidden-behavior guards from the spec.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::aai_backend::AaiBackend;
use super::claude_merge::MergeChat;
use super::pick_untimed_candidate;
use super::{ASSEMBLYAI_API_KEY_SETTING, AsrOutput, SOURCE_FALLBACK, SOURCE_MERGED, run};
use crate::lyrics::tier1::CandidateText;

fn cand(source: &str, lines: Vec<&str>) -> CandidateText {
    CandidateText {
        source: source.to_string(),
        lines: lines.into_iter().map(String::from).collect(),
        line_timings: None,
        has_timing: false,
    }
}

struct ScriptedChat {
    responses: Mutex<Vec<String>>,
}

impl ScriptedChat {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
        }
    }
}

#[async_trait::async_trait]
impl MergeChat for ScriptedChat {
    async fn chat(&self, _system: &str, _user: &str) -> Result<String, String> {
        self.responses
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| "out of responses".to_string())
    }
}

async fn aai_server_with_two_words() -> (MockServer, PathBuf) {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/upload"))
        .and(header("authorization", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "upload_url": "https://cdn.example/a.wav"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/transcript"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "tid"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/transcript/tid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "text": "hello world",
            "words": [
                {"text": "hello", "start": 0, "end": 500, "confidence": 0.9},
                {"text": "world", "start": 600, "end": 1100, "confidence": 0.9}
            ]
        })))
        .mount(&server)
        .await;

    // Unique temp file PER CALL — these orchestrator tests are `#[tokio::test]`
    // and run concurrently in the SAME process, so a PID-only name races
    // (one test deletes the WAV while another is mid-`transcribe`, surfacing as
    // a spurious AAI read error). An atomic counter guarantees uniqueness.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "asr_path_orch_test_{}_{}.wav",
        std::process::id(),
        n
    ));
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"\x00").unwrap();
    drop(f);

    (server, tmp)
}

#[test]
fn settings_key_is_stable() {
    assert_eq!(ASSEMBLYAI_API_KEY_SETTING, "assemblyai_api_key");
}

#[test]
fn pick_untimed_candidate_prefers_genius_over_lrclib() {
    let cands = vec![
        cand("lrclib", vec!["short"]),
        cand("genius", vec!["hello", "world", "again"]),
    ];
    let picked = pick_untimed_candidate(&cands).expect("must pick");
    assert_eq!(picked.source, "genius");
}

#[test]
fn pick_untimed_candidate_returns_none_on_empty_lines() {
    let cands = vec![cand("genius", vec![])];
    assert!(pick_untimed_candidate(&cands).is_none());
}

#[test]
fn pick_untimed_candidate_full_ranking() {
    // ── Tier distinction tests ──────────────────────────────────────────────
    // (A) genius beats lrclib even when lrclib has MORE lines.
    //     Kills: rank() → constant 0 or 1 (all sources tied, tiebreaker wins).
    //     Kills: match guard "genius" → true (every source gets rank 0 →
    //            lrclib wins the line-count tiebreaker with 5 lines).
    let cands = vec![
        cand("lrclib", vec!["a", "b", "c", "d", "e"]),
        cand("genius", vec!["a", "b"]),
    ];
    let picked = pick_untimed_candidate(&cands).expect("must pick");
    assert_eq!(
        picked.source, "genius",
        "genius must beat lrclib regardless of line count"
    );

    // (B) lrclib beats spotify/other (rank 1 vs 2) even when spotify has MORE
    //     lines.
    //     Kills: match guard "lrclib" → true  (spotify also gets rank 1, wins
    //            on line-count tiebreaker with 5 vs 1 lines).
    //     Kills: match guard "lrclib" → false (lrclib falls to rank 2, ties
    //            with spotify; tiebreaker gives spotify the 5-line win).
    let cands = vec![
        cand("spotify", vec!["a", "b", "c", "d", "e"]),
        cand("lrclib", vec!["a"]),
    ];
    let picked = pick_untimed_candidate(&cands).expect("must pick");
    assert_eq!(
        picked.source, "lrclib",
        "lrclib must beat spotify by rank, regardless of line count"
    );

    // ── Within-tier tiebreaker: more lines wins ─────────────────────────────
    // (C) Within the genius tier, the candidate with more lines is picked.
    //     Source label differs ("genius_live" vs "genius") so both rank 0
    //     because "genius_live".contains("genius") is true.
    //     Kills rank() → constant (collapses tiers but can't win because
    //     tier-A and tier-B above already distinguish tier ranks).
    let cands = vec![
        cand("genius", vec!["a", "b"]),
        cand("genius_live", vec!["a", "b", "c", "d", "e"]),
    ];
    let picked = pick_untimed_candidate(&cands).expect("must pick");
    assert_eq!(
        picked.lines.len(),
        5,
        "within genius tier, the candidate with more lines must win"
    );

    // (D) All candidates have empty lines → None.
    let cands = vec![cand("genius", vec![]), cand("lrclib", vec![])];
    assert!(
        pick_untimed_candidate(&cands).is_none(),
        "all-empty candidates must yield None"
    );
}

#[tokio::test]
async fn run_happy_path_returns_merged() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello", "world"])];

    let r = run(&aai, &chat, &audio, &cands, Some("en"))
        .await
        .expect("ok");
    match r.output {
        AsrOutput::Merged { lines, source } => {
            assert_eq!(source, SOURCE_MERGED);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].en, "Hello world");
            assert!(lines[0].words.is_none());
        }
        other => panic!("expected Merged, got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_falls_back_on_claude_disagreement() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": true, "notes": "wrong song", "lines": []}"#,
    ]);
    let cands = vec![cand("genius", vec!["different", "lyrics"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    match r.output {
        AsrOutput::Fallback { lines, source } => {
            assert_eq!(source, SOURCE_FALLBACK);
            assert!(!lines.is_empty(), "fallback should produce lines from AAI");
        }
        other => panic!("expected Fallback, got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_merges_ignoring_claude_ms_fields() {
    // Guard: Claude habitually echoes ms fields. The parser IGNORES them (the
    // resolver only reads word indices + AAI ms), so the merge SUCCEEDS rather
    // than falling back. The v15 guarantee holds structurally — Claude's ms are
    // never read. Output ms come from the AAI words (hello 0..500, world
    // 600..1100).
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1, "start_time_ms": 999999, "end_time_ms": 999999}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    match r.output {
        AsrOutput::Merged { lines, .. } => {
            assert_eq!(lines.len(), 1);
            // ms come from AAI words, NOT Claude's bogus 999999.
            assert_eq!(lines[0].start_ms, 0);
            assert_eq!(lines[0].end_ms, 1100);
        }
        other => panic!("expected Merged (ms ignored), got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

// ─── L131 guard: disagreement=true must trigger fallback even with non-empty lines ───

#[tokio::test]
async fn run_falls_back_when_disagreement_true_even_with_nonempty_lines() {
    // Kills mod.rs L131 mutation `||` → `&&`.
    // Under `||`: EITHER condition (disagreement=true OR lines.is_empty())
    //             triggers fallback.
    // Under `&&`: BOTH must be true. A response with disagreement=true but
    //             non-empty lines would slip through to the resolver under the
    //             mutant — the resolver would then produce Merged output instead
    //             of Fallback, which is wrong.
    //
    // Production rationale: if Claude says disagreement=true we trust that
    // signal even if it incoherently also emitted some lines. The schema does
    // not forbid the combination, so we must defend against it.
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    // Claude response: disagreement=true but also has a non-empty lines array.
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": true, "notes": "wrong song", "lines": [{"text": "stray", "start_word_idx": 0, "end_word_idx": 0}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    match r.output {
        AsrOutput::Fallback { .. } => {}
        other => panic!("expected Fallback (disagreement overrides any lines), got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

// ─── Three forbidden-behavior guards locked in tests ───

#[tokio::test]
async fn guard_never_emits_word_timings_on_merged() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "x", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Merged { lines, .. } = r.output {
        assert!(lines.iter().all(|l| l.words.is_none()));
    } else {
        panic!("expected Merged");
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn guard_never_emits_word_timings_on_fallback() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![r#"{"disagreement": true, "notes": "", "lines": []}"#]);
    let cands = vec![cand("genius", vec!["x"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Fallback { lines, .. } = r.output {
        assert!(lines.iter().all(|l| l.words.is_none()));
    } else {
        panic!("expected Fallback");
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn guard_never_synthesizes_ms_from_thin_air() {
    // Every line.start_ms / end_ms MUST come from an AAI word's start / end.
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Merged { lines, .. } = r.output {
        // AAI words were (hello: 0..500, world: 600..1100). The merged line
        // MUST equal those exact ms values — no interpolation, no rounding.
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
    } else {
        panic!("expected Merged");
    }
    let _ = std::fs::remove_file(&audio);
}

// ─── Audit-shape tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn run_emits_audit_for_merged_path() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "matched", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello", "world"])];

    let r = run(&aai, &chat, &audio, &cands, Some("en"))
        .await
        .expect("ok");
    assert_eq!(r.audit.outcome, "merged");
    assert_eq!(r.audit.source_label, Some(SOURCE_MERGED));
    assert_eq!(r.audit.aai_word_count, 2);
    assert_eq!(r.audit.claude_disagreement, Some(false));
    assert_eq!(r.audit.claude_line_count, Some(1));
    assert!(r.audit.fallback_reason.is_none());
    assert!(r.audit.quarantine_reason.is_none());
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_emits_audit_for_disagreement_fallback() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": true, "notes": "wrong version", "lines": []}"#,
    ]);
    let cands = vec![cand("genius", vec!["different"])];

    let r = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    assert_eq!(r.audit.outcome, "fallback");
    assert_eq!(r.audit.source_label, Some(SOURCE_FALLBACK));
    assert_eq!(r.audit.fallback_reason, Some("disagreement_or_empty"));
    assert_eq!(r.audit.claude_disagreement, Some(true));
    let _ = std::fs::remove_file(&audio);
}
