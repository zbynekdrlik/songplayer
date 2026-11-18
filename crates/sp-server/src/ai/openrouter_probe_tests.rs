use super::{ProbeReport, SAMPLE_CHARS, report};
use crate::ai::openrouter::Completion;

#[test]
fn an_answer_is_ok_with_its_model_and_a_trimmed_sample() {
    let long = format!("  {}  ", "o".repeat(200));
    let r = report(
        "google/gemini-4-hydrogen".into(),
        Ok(Completion {
            text: long,
            model: Some("google/gemini-4-hydrogen-1112".into()),
        }),
        42,
    );
    assert_eq!(
        r,
        ProbeReport {
            ok: true,
            model: "google/gemini-4-hydrogen".into(),
            answered_model: Some("google/gemini-4-hydrogen-1112".into()),
            latency_ms: 42,
            error: None,
            sample: "o".repeat(SAMPLE_CHARS),
        }
    );
}

#[test]
fn a_failure_is_not_ok_and_names_every_cause() {
    let err = anyhow::anyhow!("HTTP 401").context("probe");
    let r = report("m".into(), Err(err), 7);
    assert!(!r.ok);
    assert_eq!(r.error.as_deref(), Some("probe: HTTP 401"));
    assert_eq!(
        (r.answered_model, r.sample.as_str(), r.latency_ms),
        (None, "", 7)
    );
}
