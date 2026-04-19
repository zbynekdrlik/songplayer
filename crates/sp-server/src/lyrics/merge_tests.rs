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

// ---- sanitize_word_timings: 2026-04-19 event regression guards ----

#[test]
fn sanitize_clamps_zero_duration_word_to_minimum() {
    let input = vec![("Hallelujah".to_string(), 21760, 21760)];
    let out = sanitize_word_timings_from(&input, 0);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1, 21760, "start preserved");
    assert!(
        out[0].2 >= 21760 + MIN_WORD_DURATION_MS,
        "zero-duration word must be widened to at least MIN_WORD_DURATION_MS; got end={}",
        out[0].2
    );
}

#[test]
fn sanitize_fixes_backward_start_ms() {
    // Bug shape from SO BE IT: word "is" at 63715 followed by "it" at 63960,
    // then "is" at 63715 again (backward). Sanitize must make starts monotone.
    let input = vec![
        ("it".to_string(), 63960, 64200),
        ("is".to_string(), 63715, 64355),
        ("done".to_string(), 64355, 64915),
    ];
    let out = sanitize_word_timings_from(&input, 0);
    assert_eq!(out[0].1, 63960);
    assert!(
        out[1].1 >= out[0].1,
        "word 1 must not start before word 0; got {} vs {}",
        out[1].1,
        out[0].1
    );
    assert!(
        out[2].1 >= out[1].1,
        "word 2 must not start before word 1; got {} vs {}",
        out[2].1,
        out[1].1
    );
}

#[test]
fn sanitize_prevents_overlap_with_next_word() {
    // Previous word runs past the next word's start — clamp its end.
    let input = vec![
        ("Hello".to_string(), 1000, 5000),
        ("world".to_string(), 1500, 2000),
    ];
    let out = sanitize_word_timings_from(&input, 0);
    assert!(
        out[0].2 <= out[1].1,
        "word 0 end ({}) must not exceed word 1 start ({})",
        out[0].2,
        out[1].1
    );
}

#[test]
fn sanitize_preserves_valid_timings_unchanged() {
    let input = vec![
        ("Hello".to_string(), 1000, 1500),
        ("world".to_string(), 1500, 2000),
    ];
    let out = sanitize_word_timings_from(&input, 0);
    assert_eq!(out[0], ("Hello".to_string(), 1000, 1500));
    assert_eq!(out[1], ("world".to_string(), 1500, 2000));
}

#[test]
fn sanitize_handles_empty_input() {
    let out = sanitize_word_timings_from(&[], 0);
    assert!(out.is_empty());
}

#[test]
fn sanitize_from_seeds_floor_for_cross_line_boundary() {
    // Regression: when sanitizing line-by-line, the first word of line 2
    // must start AFTER line 1's last word ended, otherwise the global
    // `compute_duplicate_start_pct` (which sorts all word starts across
    // the track) reports false duplicates. v10 fix: sanitize_word_timings_from
    // takes a floor and propagates across line boundaries.
    let line2 = vec![
        ("first".to_string(), 5000, 5000),
        ("of".to_string(), 5000, 5100),
    ];
    // Line 1 ended at 5080 — line 2's first word's raw start 5000 is
    // BEHIND that. Sanitize-with-floor must push line 2's starts up.
    let out = sanitize_word_timings_from(&line2, 5080);
    assert!(
        out[0].1 >= 5080,
        "first word of line 2 must start at >= floor (5080); got {}",
        out[0].1
    );
    assert!(
        out[1].1 > out[0].1,
        "second word must still be strictly increasing"
    );
}

#[test]
fn sanitize_duplicate_start_cluster_becomes_sequential() {
    // Qwen3 emits 3-4 words all at the same start_ms. The sanitizer
    // MUST break them into a sequence with STRICTLY increasing starts
    // so the karaoke cursor can identify which word is active — just
    // "non-decreasing" (ties allowed) is not enough. This is the E2E
    // regression that caught the first version of the sanitizer:
    // duplicate_start_pct stayed at 20-37% on ensemble:qwen3 songs.
    let input = vec![
        ("The".to_string(), 5000, 5000),
        ("Lamb".to_string(), 5000, 5000),
        ("of".to_string(), 5000, 5000),
        ("God".to_string(), 5000, 5000),
    ];
    let out = sanitize_word_timings_from(&input, 0);
    assert_eq!(out.len(), 4);
    // First word preserves its raw start.
    assert_eq!(out[0].1, 5000);
    // Each word has at least minimum duration.
    for w in &out {
        assert!(
            w.2 >= w.1 + MIN_WORD_DURATION_MS,
            "word {:?} duration below minimum: {} -> {}",
            w.0,
            w.1,
            w.2
        );
    }
    // Starts must be STRICTLY increasing — no ties. This is the
    // property `duplicate_start_pct` actually measures, and the
    // E2E floor requires ≥60% of songs to land under 15% duplicates.
    for i in 1..out.len() {
        assert!(
            out[i].1 > out[i - 1].1,
            "word starts must be STRICTLY increasing; got {} then {}",
            out[i - 1].1,
            out[i].1
        );
    }
}

#[test]
fn sanitize_backward_jump_with_overlap_becomes_sequential() {
    // Composite real-world shape: a word starts BEFORE the prior word's
    // raw end. Sanitizer handles this by clamping `first`'s end DOWN to
    // `second`'s raw start (the no-overlap rule), then `second` picks
    // up at the clamped end. Each word keeps the minimum duration.
    let input = vec![
        ("first".to_string(), 1000, 1500),
        ("second".to_string(), 1200, 1250), // starts inside `first`, zero-ish duration
    ];
    let out = sanitize_word_timings_from(&input, 0);
    // second.start must be after first.start (strict).
    assert!(
        out[1].1 > out[0].1,
        "second.start ({}) must be strictly greater than first.start ({})",
        out[1].1,
        out[0].1
    );
    // second.start must be at or after first.end (no overlap).
    assert!(
        out[1].1 >= out[0].2,
        "second.start ({}) must be >= first.end ({})",
        out[1].1,
        out[0].2
    );
    // Each word keeps the minimum duration.
    assert!(out[0].2 >= out[0].1 + MIN_WORD_DURATION_MS);
    assert!(out[1].2 >= out[1].1 + MIN_WORD_DURATION_MS);
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
