//! Tests for `phantom::drop_phantom_clusters`. Sibling-included from
//! description_merge.rs to keep description_merge_tests.rs under the
//! 1000-line file-size cap.

#![allow(unused_imports)]

use super::AsrWord;
use super::phantom;

fn aw(norm: &str, start_ms: u32, end_ms: u32, confidence: f32) -> AsrWord {
    AsrWord {
        norm: norm.to_string(),
        start_ms,
        end_ms,
        confidence,
    }
}

#[test]
fn phantom_cluster_id132_holy_forever_257_drops_cause_your_name() {
    // Universal pattern derived from id=132 wall-verify (2026-05-05):
    // sustained "holy" 2141 ms conf=0.962 ends at 175711 ms; gap 1121 ms;
    // phantom "Cause"(0.687)+"your"(0.483)+"name"(0.604) at 176832-179014;
    // gap 7491 ms to next real sung "Is" at 186505 ms. The cluster was
    // matching "Your name is the highest" prefix and switching the wall
    // mid-sustained-forever. Filter must drop all three phantom words.
    let mut words = vec![
        aw("holy", 173570, 175711, 0.962),
        aw("cause", 176832, 177573, 0.687),
        aw("your", 177713, 178153, 0.483),
        aw("name", 178173, 179014, 0.604),
        aw("is", 186505, 186665, 0.857),
        aw("the", 186705, 186845, 0.893),
    ];
    phantom::drop_phantom_clusters(&mut words);
    let kept: Vec<&str> = words.iter().map(|w| w.norm.as_str()).collect();
    assert_eq!(kept, vec!["holy", "is", "the"]);
}

#[test]
fn phantom_cluster_keeps_real_short_phrase_inside_phrase_pause() {
    // Real sung phrase "to the Lamb" with 800 ms pause before/after.
    // Avg confidence is high (>0.85) — must not be dropped even if
    // surrounded by pauses.
    let mut words = vec![
        aw("worship", 23368, 24249, 0.86),
        // 721 ms gap (above GAP_BEFORE_MIN)
        aw("to", 24970, 25110, 0.86),
        aw("the", 25190, 25750, 0.92),
        aw("lamb", 25850, 27311, 0.88),
        // 5400 ms gap (above GAP_AFTER_MIN)
        aw("and", 32711, 33000, 0.84),
    ];
    phantom::drop_phantom_clusters(&mut words);
    assert_eq!(words.len(), 5, "high-confidence phrase must survive");
}

#[test]
fn phantom_cluster_keeps_low_conf_word_when_gap_after_too_small() {
    // Low-confidence cluster but the next word follows tight (no big silence
    // after) → it's part of the real flow, not an LM artifact.
    let mut words = vec![
        aw("end", 1000, 2000, 0.95),
        // 800 ms gap
        aw("um", 2800, 3100, 0.40),
        aw("ah", 3110, 3300, 0.30),
        // only 200 ms gap (below GAP_AFTER_MIN_MS) → not phantom
        aw("next", 3500, 4000, 0.95),
    ];
    phantom::drop_phantom_clusters(&mut words);
    assert_eq!(words.len(), 4);
}

#[test]
fn phantom_cluster_keeps_singleton_low_conf_word() {
    // Single low-confidence word surrounded by silence — not a cluster.
    // Existing absorb_sustained_boundary handles per-word artifacts.
    let mut words = vec![
        aw("end", 1000, 2000, 0.95),
        aw("um", 3000, 3300, 0.40),
        aw("real", 8000, 8500, 0.95),
    ];
    phantom::drop_phantom_clusters(&mut words);
    assert_eq!(
        words.len(),
        3,
        "MIN_CLUSTER_LEN=2 — singletons stay (handled elsewhere)"
    );
}

#[test]
fn phantom_cluster_drops_at_song_start() {
    // Cluster at position 0 — no previous word. Treat song-start as silence.
    let mut words = vec![
        aw("foo", 0, 200, 0.30),
        aw("bar", 220, 400, 0.40),
        // 5 s gap to first real word
        aw("real", 5400, 5800, 0.95),
    ];
    phantom::drop_phantom_clusters(&mut words);
    let kept: Vec<&str> = words.iter().map(|w| w.norm.as_str()).collect();
    assert_eq!(kept, vec!["real"]);
}

#[test]
fn phantom_cluster_keeps_high_avg_low_min_conf() {
    // Cluster of 3 words: two high-conf and one low. Avg above threshold.
    // Should survive — avg, not min, is the gate so a single low-conf
    // hiccup inside a real phrase doesn't trigger dropping the phrase.
    let mut words = vec![
        aw("a", 0, 100, 0.95),
        aw("hello", 1000, 1500, 0.95),
        aw("um", 1520, 1700, 0.10),
        aw("world", 1720, 2200, 0.90),
        aw("b", 6000, 6200, 0.95),
    ];
    phantom::drop_phantom_clusters(&mut words);
    let kept: Vec<&str> = words.iter().map(|w| w.norm.as_str()).collect();
    assert_eq!(kept, vec!["a", "hello", "um", "world", "b"]);
}

// ── boundary tests (kill mutation survivors) ─────────────────────────────────

#[test]
fn phantom_cluster_returns_zero_counts_when_input_below_min_len() {
    // words.len() < MIN_CLUSTER_LEN (=2): early-return with (0, 0).
    // Kills `replace < with ==` and `replace < with <=` at line 46.
    let mut words = vec![aw("only", 0, 100, 0.30)];
    let (clusters, dropped) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!((clusters, dropped), (0, 0));
    assert_eq!(words.len(), 1, "below-min-len input must not be mutated");
}

#[test]
fn phantom_cluster_returns_count_two_for_two_clusters() {
    // Two distinct phantom clusters in one input. Verifies the
    // `clusters_dropped += 1` accumulator: with `*= 1` the count would
    // stay at 0; with the correct `+= 1` it reaches 2. Same idea for
    // `words_dropped += len` (4 words across two clusters of 2 each).
    let mut words = vec![
        aw("real1", 0, 200, 0.95),
        aw("p1a", 1000, 1100, 0.30),
        aw("p1b", 1110, 1200, 0.40),
        aw("real2", 6200, 6400, 0.95),
        aw("p2a", 8000, 8100, 0.30),
        aw("p2b", 8110, 8200, 0.40),
        aw("real3", 13200, 13400, 0.95),
    ];
    let (clusters, dropped) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(clusters, 2, "two distinct clusters expected");
    assert_eq!(dropped, 4, "four phantom words dropped");
}

#[test]
fn phantom_cluster_gap_before_at_threshold_drops() {
    // gap_before exactly == GAP_BEFORE_MIN_MS (700) is NOT enough — the
    // check is `gap_before < 700 → skip`, so a gap of exactly 700 does
    // not skip and the cluster proceeds.
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        // gap_before = 1000 - 300 = 700 (exactly threshold)
        aw("p1", 1000, 1100, 0.30),
        aw("p2", 1110, 1200, 0.40),
        aw("real2", 6200, 6400, 0.95),
    ];
    let (clusters, _) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(
        clusters, 1,
        "gap_before == GAP_BEFORE_MIN_MS must enter cluster"
    );
}

#[test]
fn phantom_cluster_gap_before_below_threshold_does_not_start_cluster() {
    // gap_before = 699 (one below threshold) — cluster does NOT start.
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        aw("p1", 999, 1100, 0.30), // gap = 699
        aw("p2", 1110, 1200, 0.40),
        aw("real2", 6200, 6400, 0.95),
    ];
    let (clusters, _) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(clusters, 0, "gap_before < GAP_BEFORE_MIN_MS must skip");
    assert_eq!(words.len(), 4, "no words dropped");
}

#[test]
fn phantom_cluster_gap_after_at_threshold_drops() {
    // gap_after exactly == GAP_AFTER_MIN_MS (3000) DROPS: code is
    // `if gap_after < 3000 → skip`. 3000 fails the strict-less guard →
    // drops. gap=2999 SKIPS. Verifies the `<` boundary on gap_after.
    let mut words_at_threshold = vec![
        aw("real", 0, 300, 0.95),
        aw("p1", 1000, 1100, 0.30),
        aw("p2", 1110, 1200, 0.40),
        // gap_after = 4200 - 1200 = 3000 exactly
        aw("real2", 4200, 4400, 0.95),
    ];
    let (c1, _) = phantom::drop_phantom_clusters(&mut words_at_threshold);
    assert_eq!(
        c1, 1,
        "gap_after == GAP_AFTER_MIN_MS drops (boundary inclusive)"
    );

    let mut words_just_under = vec![
        aw("real", 0, 300, 0.95),
        aw("p1", 1000, 1100, 0.30),
        aw("p2", 1110, 1200, 0.40),
        // gap_after = 4199 - 1200 = 2999 (one under)
        aw("real2", 4199, 4400, 0.95),
    ];
    let (c2, _) = phantom::drop_phantom_clusters(&mut words_just_under);
    assert_eq!(c2, 0, "gap_after = 2999 must NOT drop");
}

#[test]
fn phantom_cluster_avg_conf_at_threshold_does_not_drop() {
    // avg conf exactly == 0.70: `avg >= 0.70 → skip`. So avg of exactly
    // 0.70 does NOT drop. Verifies the >= guard isn't relaxed to >.
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        aw("p1", 1000, 1100, 0.70),
        aw("p2", 1110, 1200, 0.70),
        aw("real2", 6200, 6400, 0.95),
    ];
    let (clusters, _) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(clusters, 0, "avg conf == 0.70 must NOT drop");
}

#[test]
fn phantom_cluster_high_conf_word_breaks_cluster_growth() {
    // First word low-conf after silence → cluster starts. Second word
    // high-conf → cluster ENDS (not absorbed into cluster). Cluster len
    // = 1, below MIN_CLUSTER_LEN → no drop.
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        aw("low", 1100, 1200, 0.30),
        aw("HIGH", 1210, 1400, 0.95),
        aw("real2", 7400, 7600, 0.95),
    ];
    let (clusters, _) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(clusters, 0, "high-conf word must end cluster growth");
    assert_eq!(words.len(), 4, "no words dropped");
}

#[test]
fn phantom_cluster_inner_gap_at_threshold_breaks_cluster() {
    // Inner gap exactly == GAP_BEFORE_MIN_MS (700): `inner_gap >= 700 → break`.
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        aw("p1", 1100, 1200, 0.30),
        aw("p2", 1900, 2000, 0.40), // inner_gap = 700
        aw("real2", 7000, 7200, 0.95),
    ];
    let (clusters, _) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(
        clusters, 0,
        "inner gap >= GAP_BEFORE_MIN_MS must break cluster"
    );
}

#[test]
fn phantom_cluster_at_song_end_treats_end_as_silence() {
    // Cluster as the LAST words: gap_after = u32::MAX (no next word).
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        aw("p1", 1100, 1200, 0.30),
        aw("p2", 1210, 1300, 0.40),
    ];
    let (clusters, dropped) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(clusters, 1);
    assert_eq!(dropped, 2);
    let kept: Vec<&str> = words.iter().map(|w| w.norm.as_str()).collect();
    assert_eq!(kept, vec!["real"]);
}

#[test]
fn phantom_cluster_singleton_low_conf_advances_past_word() {
    // Verifies `i = j.max(i + 1)` advances correctly when len < MIN_CLUSTER_LEN.
    let mut words = vec![
        aw("real", 0, 300, 0.95),
        aw("low1", 1100, 1200, 0.30),
        aw("hi", 1210, 1500, 0.95),
        aw("low2", 2300, 2400, 0.30),
        aw("low3", 2410, 2500, 0.40),
        aw("real2", 7500, 7700, 0.95),
    ];
    let (clusters, dropped) = phantom::drop_phantom_clusters(&mut words);
    assert_eq!(clusters, 1, "second cluster (low2+low3) drops");
    assert_eq!(dropped, 2);
}
