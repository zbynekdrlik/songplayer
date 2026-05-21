//! Integration tests for the lean asr_path orchestrator (AAI → split).

use std::io::Write;
use std::path::PathBuf;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::aai_backend::AaiBackend;
use super::{ASSEMBLYAI_API_KEY_SETTING, AsrOutput, SOURCE_ASR, run};

async fn aai_server(words_json: serde_json::Value) -> (MockServer, PathBuf) {
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "tid" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/transcript/tid"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "text": "x",
            "words": words_json,
        })))
        .mount(&server)
        .await;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("asr_path_test_{}_{}.wav", std::process::id(), n));
    let mut f = std::fs::File::create(&tmp).unwrap();
    f.write_all(b"\x00").unwrap();
    drop(f);
    (server, tmp)
}

#[test]
fn settings_key_is_stable() {
    assert_eq!(ASSEMBLYAI_API_KEY_SETTING, "assemblyai_api_key");
}

#[tokio::test]
async fn run_transcribes_and_splits_into_lines() {
    let (server, audio) = aai_server(serde_json::json!([
        {"text": "There", "start": 0, "end": 400, "confidence": 0.9},
        {"text": "is", "start": 450, "end": 700, "confidence": 0.9},
        {"text": "a", "start": 750, "end": 850, "confidence": 0.9},
        {"text": "name", "start": 900, "end": 1300, "confidence": 0.9}
    ]))
    .await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let r = run(&aai, &audio, &[]).await.expect("ok");
    match r.output {
        AsrOutput::Lines { lines, source } => {
            assert_eq!(source, SOURCE_ASR);
            assert!(!lines.is_empty());
            assert!(
                lines.iter().all(|l| l.words.is_none()),
                "words must be None"
            );
            // ms come straight from AAI words.
            assert_eq!(lines[0].start_ms, 0);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
    assert_eq!(r.audit.outcome, "lines");
    assert_eq!(r.audit.source_label, Some(SOURCE_ASR));
    assert_eq!(r.audit.aai_word_count, 4);
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_quarantines_on_empty_transcript() {
    let (server, audio) = aai_server(serde_json::json!([])).await;
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let r = run(&aai, &audio, &[]).await.expect("ok");
    assert!(matches!(
        r.output,
        AsrOutput::Quarantine {
            reason: "empty_transcript"
        }
    ));
    assert_eq!(r.audit.outcome, "quarantine");
    let _ = std::fs::remove_file(&audio);
}

#[tokio::test]
async fn run_propagates_quota_exhausted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let tmp = std::env::temp_dir().join(format!("asr_path_q_{}.wav", std::process::id()));
    std::fs::write(&tmp, b"\x00").unwrap();
    let aai = AaiBackend::with_base_url("test-key", server.uri());
    let err = run(&aai, &tmp, &[]).await.expect_err("must err");
    assert!(
        matches!(err, super::AsrError::QuotaExhausted),
        "got {err:?}"
    );
    let _ = std::fs::remove_file(&tmp);
}
