//! Tests for the deterministic merge layer. Sibling file per airuleset
//! 1000-line cap on source modules.

use super::*;

fn make_provider(name: &str, base_conf: f64, words: &[(&str, u64, u64, f32)]) -> ProviderResult {
    ProviderResult {
        provider_name: name.into(),
        lines: if words.is_empty() {
            vec![]
        } else {
            vec![LineTiming {
                text: words
                    .iter()
                    .map(|(t, _, _, _)| *t)
                    .collect::<Vec<_>>()
                    .join(" "),
                start_ms: words.first().map(|w| w.1).unwrap_or(0),
                end_ms: words.last().map(|w| w.2).unwrap_or(0),
                words: words
                    .iter()
                    .map(|(t, s, e, c)| WordTiming {
                        text: (*t).into(),
                        start_ms: *s,
                        end_ms: *e,
                        confidence: *c,
                    })
                    .collect(),
            }]
        },
        metadata: serde_json::json!({"base_confidence": base_conf}),
    }
}

fn dummy_client() -> AiClient {
    AiClient::new(crate::ai::AiSettings {
        api_url: "http://unused".into(),
        api_key: Some("unused".into()),
        model: "unused".into(),
        system_prompt_extra: None,
    })
}

#[test]
fn pick_best_provider_selects_highest_base_confidence() {
    let providers = vec![
        make_provider("autosub", 0.3, &[("a", 0, 100, 0.3)]),
        make_provider("qwen3", 0.7, &[("b", 0, 100, 0.7)]),
    ];
    let best = pick_best_provider_with_words(&providers).unwrap();
    assert_eq!(best.provider_name, "qwen3");
}

#[test]
fn pick_best_provider_skips_providers_without_words() {
    // Provider with higher base_confidence but NO words must be skipped.
    let providers = vec![
        make_provider("empty_high_conf", 0.9, &[]),
        make_provider("qwen3", 0.7, &[("b", 0, 100, 0.7)]),
    ];
    let best = pick_best_provider_with_words(&providers).unwrap();
    assert_eq!(
        best.provider_name, "qwen3",
        "must skip empty providers even if base_confidence is higher"
    );
}

#[test]
fn pick_best_provider_returns_none_when_no_provider_has_words() {
    let providers = vec![make_provider("empty", 0.9, &[])];
    assert!(pick_best_provider_with_words(&providers).is_none());
}

#[test]
fn base_confidence_of_reads_metadata_defaults_0_7() {
    let p = ProviderResult {
        provider_name: "x".into(),
        lines: vec![],
        metadata: serde_json::json!({}),
    };
    assert!((base_confidence_of(&p) - 0.7).abs() < 1e-6);

    let p2 = make_provider("y", 0.55, &[]);
    assert!((base_confidence_of(&p2) - 0.55).abs() < 1e-6);
}

#[test]
fn nearest_within_empty_slice_is_false() {
    assert!(!nearest_within(1000, &[], 500));
}

#[test]
fn nearest_within_finds_value_below() {
    // Target is 1000, sorted contains 700 (diff 300) → within 500ms window.
    assert!(nearest_within(1000, &[200, 700], 500));
}

#[test]
fn nearest_within_finds_value_above() {
    // Target 1000, sorted contains 1400 (diff 400) → within window.
    assert!(nearest_within(1000, &[1400, 2000], 500));
}

#[test]
fn nearest_within_rejects_far_values() {
    // All values are > 500ms from target.
    assert!(!nearest_within(1000, &[0, 200, 1800, 5000], 500));
}

#[test]
fn nearest_within_boundary_exactly_at_window_is_included() {
    // Diff of exactly 500ms must count as within (<=, not <).
    assert!(nearest_within(1000, &[500], 500));
    assert!(nearest_within(1000, &[1500], 500));
}

#[test]
fn word_confidence_pass_through_without_agreement() {
    // primary_base = 0.7, no peers within 500ms → 0.7 * 0.7 = 0.49
    let c = word_confidence(1000, 0.7, &[5000, 10000]);
    assert!((c - 0.49).abs() < 1e-5, "expected 0.49, got {c}");
}

#[test]
fn word_confidence_boost_on_agreement() {
    // primary_base = 0.7, peer at 1100 (100ms diff) → 0.7 * 1.2 = 0.84
    let c = word_confidence(1000, 0.7, &[1100]);
    assert!((c - 0.84).abs() < 1e-5, "expected 0.84, got {c}");
}

#[test]
fn word_confidence_boost_caps_at_1_0() {
    // primary_base = 0.9, peer within window → 0.9 * 1.2 = 1.08, must cap at 1.0.
    let c = word_confidence(1000, 0.9, &[1000]);
    assert!((c - 1.0).abs() < 1e-5, "expected 1.0 cap, got {c}");
}

#[test]
fn word_confidence_no_peers_is_pass_through() {
    // Empty peer list → pass-through multiplier.
    let c = word_confidence(1000, 0.7, &[]);
    assert!((c - 0.49).abs() < 1e-5);
}

#[tokio::test]
async fn merge_with_single_provider_passes_through_at_0_7x_base() {
    let providers = vec![make_provider(
        "qwen3",
        0.7,
        &[("Hello", 1000, 1500, 0.7), ("world", 1500, 2000, 0.7)],
    )];
    let client = dummy_client();
    let (track, details) = merge_provider_results(&client, "Hello world", "lrclib", &providers)
        .await
        .unwrap();
    assert_eq!(track.source, "ensemble:qwen3");
    assert_eq!(track.lines.len(), 1);
    assert_eq!(track.lines[0].words.as_ref().unwrap().len(), 2);
    assert_eq!(details.len(), 2);
    // No peers → pass-through: 0.7 * 0.7 = 0.49
    for d in &details {
        assert!(
            (d.merged_confidence - 0.49).abs() < 1e-5,
            "single provider → pass-through 0.49, got {}",
            d.merged_confidence
        );
    }
}

#[tokio::test]
async fn merge_boosts_confidence_when_providers_agree_on_timing() {
    // qwen3 at 1000/1500, autosub within 100ms on each word → agreement.
    let providers = vec![
        make_provider(
            "autosub",
            0.3,
            &[("hello", 1080, 1600, 0.3), ("world", 1520, 2020, 0.3)],
        ),
        make_provider(
            "qwen3",
            0.7,
            &[("Hello", 1000, 1500, 0.7), ("world", 1500, 2000, 0.7)],
        ),
    ];
    let client = dummy_client();
    let (track, details) = merge_provider_results(&client, "Hello world", "lrclib", &providers)
        .await
        .unwrap();
    assert_eq!(
        track.source, "ensemble:autosub+qwen3",
        "source must list ALL participating providers, not just primary"
    );
    // Both words should get boost: 0.7 * 1.2 = 0.84
    for d in &details {
        assert!(
            (d.merged_confidence - 0.84).abs() < 1e-5,
            "agreement must boost to 0.84, got {}",
            d.merged_confidence
        );
    }
}

#[tokio::test]
async fn merge_no_agreement_stays_at_pass_through() {
    // qwen3 words at 1000 and 1500, autosub words far away (5000, 10000).
    let providers = vec![
        make_provider("autosub", 0.3, &[("x", 5000, 5500, 0.3)]),
        make_provider(
            "qwen3",
            0.7,
            &[("Hello", 1000, 1500, 0.7), ("world", 1500, 2000, 0.7)],
        ),
    ];
    let client = dummy_client();
    let (_, details) = merge_provider_results(&client, "Hello world", "lrclib", &providers)
        .await
        .unwrap();
    for d in &details {
        assert!(
            (d.merged_confidence - 0.49).abs() < 1e-5,
            "no agreement → pass-through 0.49, got {}",
            d.merged_confidence
        );
    }
}

#[tokio::test]
async fn merge_uses_qwen3_line_structure_even_with_autosub_present() {
    // The primary (qwen3) line text must flow through to the output track.
    // Autosub's different tokenization must NOT corrupt the reference lines.
    let providers = vec![
        make_provider(
            "autosub",
            0.3,
            &[("hel", 1000, 1200, 0.3), ("lo", 1200, 1400, 0.3)],
        ),
        make_provider("qwen3", 0.7, &[("Hello", 1000, 1500, 0.7)]),
    ];
    let client = dummy_client();
    let (track, _) = merge_provider_results(&client, "Hello", "lrclib", &providers)
        .await
        .unwrap();
    assert_eq!(
        track.lines.len(),
        1,
        "one line from qwen3 (primary), not two from autosub's ASR split"
    );
    assert_eq!(track.lines[0].en, "Hello");
    assert_eq!(track.lines[0].words.as_ref().unwrap().len(), 1);
    assert_eq!(track.lines[0].words.as_ref().unwrap()[0].text, "Hello");
}

#[tokio::test]
async fn merge_bails_when_no_provider_has_words() {
    // No usable data → error (not silent zero-word output).
    let providers = vec![make_provider("empty", 0.7, &[])];
    let client = dummy_client();
    let err = merge_provider_results(&client, "anything", "lrclib", &providers).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn merge_does_not_call_ai_client() {
    // Regression: the deterministic merge must not hit the AI endpoint.
    // Construct a client pointed at an unroutable URL; if the code calls
    // chat(), the test fails with a connection error. If it skips the call
    // (as required by v7), it completes normally.
    let client = AiClient::new(crate::ai::AiSettings {
        api_url: "http://127.0.0.1:1/v1".into(), // port 1 is reserved + unroutable
        api_key: Some("test".into()),
        model: "m".into(),
        system_prompt_extra: None,
    });
    let providers = vec![make_provider("qwen3", 0.7, &[("word", 100, 200, 0.7)])];
    let result = merge_provider_results(&client, "word", "lrclib", &providers).await;
    assert!(
        result.is_ok(),
        "deterministic merge must not touch the AI client; got err: {:?}",
        result.err()
    );
}
