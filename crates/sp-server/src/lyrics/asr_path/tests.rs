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

    let tmp = std::env::temp_dir().join(format!(
        "asr_path_orch_test_{}.wav",
        std::process::id()
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

#[tokio::test]
async fn run_happy_path_returns_merged() {
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "Hello world", "start_word_idx": 0, "end_word_idx": 1}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello", "world"])];

    let out = run(&aai, &chat, &audio, &cands, Some("en")).await.expect("ok");
    match out {
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

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    match out {
        AsrOutput::Fallback { lines, source } => {
            assert_eq!(source, SOURCE_FALLBACK);
            assert!(!lines.is_empty(), "fallback should produce lines from AAI");
        }
        other => panic!("expected Fallback, got {other:?}"),
    }
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_falls_back_on_claude_ms_field() {
    // Guard: Claude emits ms fields → parser rejects → orchestrator falls back.
    let (server, audio) = aai_server_with_two_words().await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": false, "notes": "", "lines": [{"text": "x", "start_word_idx": 0, "end_word_idx": 1, "start_ms": 0}]}"#,
    ]);
    let cands = vec![cand("genius", vec!["hello"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    assert!(matches!(out, AsrOutput::Fallback { .. }), "got {out:?}");
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

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Merged { lines, .. } = out {
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
    let chat = ScriptedChat::new(vec![
        r#"{"disagreement": true, "notes": "", "lines": []}"#,
    ]);
    let cands = vec![cand("genius", vec!["x"])];

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Fallback { lines, .. } = out {
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

    let out = run(&aai, &chat, &audio, &cands, None).await.expect("ok");
    if let AsrOutput::Merged { lines, .. } = out {
        // AAI words were (hello: 0..500, world: 600..1100). The merged line
        // MUST equal those exact ms values — no interpolation, no rounding.
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 1100);
    } else {
        panic!("expected Merged");
    }
    let _ = std::fs::remove_file(&audio);
}
