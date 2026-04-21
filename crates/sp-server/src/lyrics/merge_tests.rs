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

// ---- Mutation-pinning tests for v10 code (2026-04-19 CI surface) ----

#[test]
fn pass_through_baseline_multiplies_base_by_0_7() {
    // Kills mutations: replace `*` with `+` or `/`, or return 0.0/1.0/-1.0.
    assert!((pass_through_baseline(1.0) - 0.7).abs() < 1e-6);
    assert!((pass_through_baseline(0.5) - 0.35).abs() < 1e-6);
    assert!((pass_through_baseline(0.0) - 0.0).abs() < 1e-6);
    // Non-identity multiplication distinct from addition/division.
    let computed = pass_through_baseline(0.9);
    assert!((computed - 0.63).abs() < 1e-6);
    assert!((computed - (0.9 + PASS_THROUGH_MULTIPLIER)).abs() > 0.1);
    assert!((computed - (0.9 / PASS_THROUGH_MULTIPLIER)).abs() > 0.1);
}

#[test]
fn nearest_within_strict_boundary_is_inclusive() {
    // Mutation-pin: `x < target` in partition_point vs `<=`. Target at an
    // exact element must be "within 0ms" of itself.
    assert!(nearest_within(1000, &[1000], 0));
    assert!(nearest_within(1000, &[1000, 2000], 0));
    // Differs from <=: a target 1000 against sorted [500, 1500] with
    // window 500 must still find 500 AND 1500 as equi-distant.
    assert!(nearest_within(1000, &[500, 1500], 500));
    // Boundary: 501 away = rejected when window = 500.
    assert!(!nearest_within(1000, &[499, 1501], 500));
}

#[test]
fn sanitize_no_overlap_preserves_boundary_timings() {
    // Word 0 ends exactly at word 1's start — already no overlap, no
    // clamp needed. `end_ms.min(next_start_effective)` with both equal
    // is a no-op.
    let no_overlap = vec![("a".to_string(), 1000, 1080), ("b".to_string(), 1080, 1160)];
    let out = sanitize_word_timings_from(&no_overlap, 0);
    assert_eq!(out[0].2, 1080, "no overlap → word 0 end preserved");
    assert_eq!(out[1].1, 1080);
}

#[test]
fn sanitize_overlap_clamps_word_end_down_preserves_min_duration() {
    // Word 0's raw end exceeds word 1's start — `end_ms.min(next_effective)`
    // clamps word 0 down. Because `next_effective = max(next_raw, start+MIN)`,
    // the clamped end is always >= `start + MIN_WORD_DURATION_MS`, so the
    // minimum-duration invariant holds without a separate restore branch.
    let overlap = vec![("a".to_string(), 1000, 5000), ("b".to_string(), 1200, 1300)];
    let out = sanitize_word_timings_from(&overlap, 0);
    assert!(
        out[0].2 <= out[1].1,
        "overlap case must clamp word 0 end down; got {} vs next start {}",
        out[0].2,
        out[1].1
    );
    assert!(
        out[0].2 >= out[0].1 + MIN_WORD_DURATION_MS,
        "clamped word must still have at least MIN_WORD_DURATION_MS of width"
    );
}

#[tokio::test]
async fn merge_provider_results_word_index_increments_by_one_per_word() {
    // Mutation-pin for merge.rs:108 `word_index += 1`. If the `+=` is
    // replaced with `*=`, word_index stays at 0 for every entry. Assert
    // the emitted `details[i].word_index == i` sequence directly from
    // `merge_provider_results`, which is where the counter lives.
    let providers = vec![make_provider(
        "qwen3",
        0.7,
        &[
            ("a", 1000, 1250, 0.7),
            ("b", 1250, 1500, 0.7),
            ("c", 1600, 1850, 0.7),
            ("d", 1850, 2100, 0.7),
        ],
    )];
    let client = dummy_client();
    let (_track, details) = merge_provider_results(&client, "a b c d", "test", &providers, 0)
        .await
        .unwrap();
    assert_eq!(details.len(), 4);
    for (i, d) in details.iter().enumerate() {
        assert_eq!(
            d.word_index, i,
            "word_index sequence must be 0..N; got details[{i}].word_index = {}",
            d.word_index
        );
    }
}

// ---- sanitize_word_timings: 2026-04-19 event regression guards ----

#[test]
fn sanitize_track_emits_wordless_lines_with_line_level_timing() {
    // Regression guard for the v14 data-loss bug: pre-v15 `sanitize_track`
    // silently dropped every wordless `LineTiming`, which meant every
    // Gemini song (Gemini is line-level-only) persisted with `lines: []`.
    // 17 of 31 v11-v14 Gemini-marked rows on production had empty JSON
    // because of this. v15 emits wordless lines with their line-level
    // timing, clamped to `floor_start_ms` so the cross-line
    // strict-increasing invariant still holds.
    let provider_lines = vec![
        LineTiming {
            text: "line one".into(),
            start_ms: 1000,
            end_ms: 2000,
            words: vec![
                WordTiming {
                    text: "line".into(),
                    start_ms: 1000,
                    end_ms: 1500,
                    confidence: 0.7,
                },
                WordTiming {
                    text: "one".into(),
                    start_ms: 1500,
                    end_ms: 2000,
                    confidence: 0.7,
                },
            ],
        },
        LineTiming {
            // Wordless (line-level only) — must be emitted with its own
            // line timing, NOT dropped.
            text: "[instrumental]".into(),
            start_ms: 2500,
            end_ms: 3500,
            words: vec![],
        },
        LineTiming {
            text: "line three".into(),
            start_ms: 4000,
            end_ms: 5000,
            words: vec![
                WordTiming {
                    text: "line".into(),
                    start_ms: 4000,
                    end_ms: 4500,
                    confidence: 0.7,
                },
                WordTiming {
                    text: "three".into(),
                    start_ms: 4500,
                    end_ms: 5000,
                    confidence: 0.7,
                },
            ],
        },
    ];
    let out = sanitize_track(&provider_lines, 10_000);
    assert_eq!(
        out.len(),
        3,
        "all three lines (including the wordless one) must be emitted, got {}",
        out.len()
    );
    assert_eq!(out[0].en, "line one");
    assert_eq!(out[1].en, "[instrumental]");
    assert_eq!(out[2].en, "line three");

    // v22: wordless line end_ms is Gemini's value verbatim (no clipping
    // to next_start-50, no extending into gaps). Input end_ms=3500.
    assert_eq!(out[1].start_ms, 2500);
    assert_eq!(
        out[1].end_ms, 3500,
        "end_ms is Gemini's verbatim value; got {}",
        out[1].end_ms
    );
    assert!(out[1].words.is_none());
}

#[test]
fn sanitize_track_all_wordless_lines_all_emitted() {
    // Gemini-only case: every LineTiming is wordless. Pre-v15 this
    // returned an empty Vec and shipped empty lyrics JSON. v15 must emit
    // every line with its provider timing.
    let provider_lines = vec![
        LineTiming {
            text: "first line".into(),
            start_ms: 1000,
            end_ms: 3000,
            words: vec![],
        },
        LineTiming {
            text: "second line".into(),
            start_ms: 3500,
            end_ms: 5500,
            words: vec![],
        },
        LineTiming {
            text: "third line".into(),
            start_ms: 6000,
            end_ms: 8000,
            words: vec![],
        },
    ];
    let out = sanitize_track(&provider_lines, 10_000);
    assert_eq!(
        out.len(),
        3,
        "Gemini-style all-wordless track must emit every line"
    );
    assert_eq!(out[0].en, "first line");
    assert_eq!(out[0].start_ms, 1000);
    // v22: Gemini's end_ms values pass through verbatim (no clip, no extend).
    assert_eq!(out[0].end_ms, 3000);
    assert_eq!(out[1].end_ms, 5500);
    assert_eq!(out[2].end_ms, 8000);
    assert!(out[0].words.is_none());
    assert!(out[1].words.is_none());
    assert!(out[2].words.is_none());
    assert_eq!(out[1].start_ms, 3500);
    assert_eq!(out[2].start_ms, 6000);
    assert!(out[0].start_ms < out[1].start_ms);
    assert!(out[1].start_ms < out[2].start_ms);
}

#[test]
fn sanitize_track_wordless_line_uses_gemini_end_verbatim_even_if_overlap() {
    // v22: the sanitizer no longer clips end_ms against next_start. Per
    // user direction, line should stay visible until it's sung (Gemini's
    // end_ms) — gaps between lines are fine; overlap with the next line
    // is also acceptable (rare in practice; common when two vocalists
    // trade off). Trust Gemini's timing.
    let provider_lines = vec![
        LineTiming {
            text: "first".into(),
            start_ms: 1000,
            end_ms: 4000,
            words: vec![],
        },
        LineTiming {
            text: "second".into(),
            start_ms: 3800,
            end_ms: 5000,
            words: vec![],
        },
    ];
    let out = sanitize_track(&provider_lines, 10_000);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].end_ms, 4000, "Gemini's end_ms verbatim");
    assert_eq!(out[1].start_ms, 3800, "Gemini's start_ms verbatim");
    assert_eq!(out[1].end_ms, 5000);
}

#[test]
fn sanitize_track_wordless_input_never_emits_synthetic_words() {
    // v18 regression guard: wordless Gemini input must NEVER produce
    // `words: Some(...)`. Per-word synthesis was removed because even-
    // distribution across the line duration caused the karaoke
    // highlighter to fire at wrong moments on the wall.
    let provider_lines = vec![LineTiming {
        text: "amazing grace how sweet".into(),
        start_ms: 10_000,
        end_ms: 14_000,
        words: vec![],
    }];
    let out = sanitize_track(&provider_lines, 20_000);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].en, "amazing grace how sweet");
    assert_eq!(out[0].start_ms, 10_000);
    // v22: end_ms is Gemini's value verbatim (no extend).
    assert_eq!(out[0].end_ms, 14_000);
    assert!(out[0].words.is_none());
}

#[test]
fn sanitize_track_last_wordless_line_caps_at_song_duration() {
    // v22: end_ms passes through verbatim EXCEPT when it would extend past
    // the song duration — that's a pure sanity cap (prevents invalid
    // timestamps in the persisted JSON), not a gap-control heuristic.
    let provider_lines = vec![LineTiming {
        text: "final word".into(),
        start_ms: 100_000,
        end_ms: 200_000, // way past song end
        words: vec![],
    }];
    let out = sanitize_track(&provider_lines, 120_000);
    assert_eq!(
        out[0].end_ms, 120_000,
        "end must cap at song duration, not extend past it; got {}",
        out[0].end_ms
    );
}

#[test]
fn sanitize_track_wordless_line_clamps_start_to_floor() {
    // If a wordless line's start_ms collides with the previous line's
    // end, clamp to the floor so the strict-increasing invariant holds.
    let provider_lines = vec![
        LineTiming {
            text: "first".into(),
            start_ms: 1000,
            end_ms: 2000,
            words: vec![WordTiming {
                text: "first".into(),
                start_ms: 1000,
                end_ms: 2000,
                confidence: 0.7,
            }],
        },
        LineTiming {
            // Provider claims start=1500 which is BEFORE the previous
            // line's end — the sanitizer must clamp to 2000.
            text: "overlapping".into(),
            start_ms: 1500,
            end_ms: 3000,
            words: vec![],
        },
    ];
    let out = sanitize_track(&provider_lines, 10_000);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[1].start_ms, 2000,
        "wordless line must clamp start to prior line's end (2000), got {}",
        out[1].start_ms
    );
    assert!(out[1].end_ms >= out[1].start_ms + 80);
}

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
    let (track, details) = merge_provider_results(&client, "Hello world", "lrclib", &providers, 0)
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
    let (track, details) = merge_provider_results(&client, "Hello world", "lrclib", &providers, 0)
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
    let (_, details) = merge_provider_results(&client, "Hello world", "lrclib", &providers, 0)
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
    let (track, _) = merge_provider_results(&client, "Hello", "lrclib", &providers, 0)
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
    let err = merge_provider_results(&client, "anything", "lrclib", &providers, 0).await;
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
    let result = merge_provider_results(&client, "word", "lrclib", &providers, 0).await;
    assert!(
        result.is_ok(),
        "deterministic merge must not touch the AI client; got err: {:?}",
        result.err()
    );
}
