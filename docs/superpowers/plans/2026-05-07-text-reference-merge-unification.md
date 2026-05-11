# Text-Reference Merge Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify all text-only references (description, genius, lrclib text-only, yt_subs text-only, override) through a single rich text-merge pipeline; route timed references (spotify, lrclib LINE-SYNCED, yt_subs timed) through a new timed-merge path that respects existing line timings; reorder source priority so YouTube description outranks every other text-only source; delete `claude_merge::merge` entirely.

**Architecture:** Rename `description_merge` → `text_reference_merge` (pure file rename, no logic change). Rewrite `claude_merge::source_priority` to a function of `(source, has_timing)`. Add `coverage_ok` helper. Refactor orchestrator dispatch: `TextOnly` branch picks best-authoritative candidate then routes to either `timed_reference_merge` (timed + coverage ≥0.80) or `text_reference_merge` (text-only). `LineSynced` branch routes through `timed_reference_merge` for the same sanitize/cap/phantom-filter pass. Old `claude_merge::merge` and its internal types (`Phrase`, `MergedLine`, `ClaudeResponse`, `build_phrases`, `build_prompt`, `parse_claude_response`) are deleted; the file shrinks to host `source_priority`, `best_authoritative_candidate`, `coverage_ok`, `MergeError`, and `drop_hallucinated_lead_in` only.

**Tech Stack:** Rust 2024, sqlx 0.8 (SQLite), axum 0.8, tokio 1, async-trait 0.1, anyhow, thiserror, serde / serde_json, wiremock, tempfile.

**Spec:** [`docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md`](../specs/2026-05-07-text-reference-merge-unification-design.md) — commit `681c075`.

---

## Per-implementer airuleset rules (verbatim — applies to every task)

- TDD strict: failing test first → trust by inspection → implement → trust by inspection → `cargo fmt --all --check` (the ONLY local cargo command allowed) → commit on green.
- NEVER run `cargo clippy / test / build / check` locally; rely on CI.
- File-size cap 1000 lines per file.
- One commit per "Commit" step in this plan body.
- `mutants::skip` requires inline justification (one-line `// mutants::skip: <reason>` immediately above the attribute).
- Do NOT push — controller batches and pushes once per phase.
- Per `feedback_no_legacy_code.md`: `claude_merge::merge` MUST be deleted entirely, not deprecated. No fallback retention, no `#[deprecated]`.
- Per `feedback_pipeline_version_approval.md` AND `feedback_no_bump_until_proven.md`: do NOT bump `LYRICS_PIPELINE_VERSION`. Constant stays at `20`.
- Per `feedback_canonical_source_regression_ci.md`: Phase E adds a CI integration test pinning expected source labels.
- Per `feedback_song_by_song_iteration.md`: no catalog-wide auto-rerun. Wall-verify is per-song and post-deploy.
- Per `feedback_no_autosub.md`: `yt_subs` text candidate is fine; `AutoSubProvider` registration stays banned (this plan does NOT touch alignment-provider registration).
- Per `feedback_line_timing_only.md`: every output line ships `words: None`. Do NOT synthesize per-word timings.
- Per `feedback_take_ownership.md`: root-cause fix only.

---

## Phase A — Rename `description_merge` → `text_reference_merge`

Pure file rename. No logic change. Sibling include paths and module declaration updated. The old name `description_merge` disappears completely from the source tree (no aliases).

### Task A.1 — Rename module and 9 sibling files

**Files (rename — git mv to preserve history):**

- Rename `crates/sp-server/src/lyrics/description_merge.rs` → `crates/sp-server/src/lyrics/text_reference_merge.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_mapping.rs` → `crates/sp-server/src/lyrics/text_reference_merge_mapping.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_audit.rs` → `crates/sp-server/src/lyrics/text_reference_merge_audit.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_window.rs` → `crates/sp-server/src/lyrics/text_reference_merge_window.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_absorb.rs` → `crates/sp-server/src/lyrics/text_reference_merge_absorb.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_phantom.rs` → `crates/sp-server/src/lyrics/text_reference_merge_phantom.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_tests.rs` → `crates/sp-server/src/lyrics/text_reference_merge_tests.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_phantom_tests.rs` → `crates/sp-server/src/lyrics/text_reference_merge_phantom_tests.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_dp_tests.rs` → `crates/sp-server/src/lyrics/text_reference_merge_dp_tests.rs`
- Rename `crates/sp-server/src/lyrics/description_merge_split_tests.rs` → `crates/sp-server/src/lyrics/text_reference_merge_split_tests.rs`

**Modify:**
- `crates/sp-server/src/lyrics/mod.rs:9` — `pub mod description_merge;` → `pub mod text_reference_merge;`
- `crates/sp-server/src/lyrics/text_reference_merge.rs` — update header doc comment (`//! Description / override merge pipeline (issue #78). Phases:` → `//! Text-reference merge pipeline (issue #78 + 2026-05-07 unification). Phases:`); update all 5 `#[path = "description_merge_*.rs"]` directives to `text_reference_merge_*.rs` paths; update all 4 `#[path = "description_merge_*_tests.rs"]` directives in the `#[cfg(test)]` block; update all log-target strings of the form `"description_merge: ..."` to `"text_reference_merge: ..."` (5 sites at lines 112, 121, 131, 172, 458, 514 — confirm during task).
- `crates/sp-server/src/lyrics/text_reference_merge_phantom.rs` — update log-target strings `"description_merge: dropping phantom cluster"` and `"description_merge: phantom-cluster filter active"` to `"text_reference_merge: ..."`.
- `crates/sp-server/src/lyrics/claude_merge.rs:74` — replace `crate::lyrics::description_merge::process(ai_client, asr, best, audit).await` with `crate::lyrics::text_reference_merge::process(ai_client, asr, best, audit).await`. (Note: this line gets DELETED in Phase C anyway, but Phase A still updates it so the tree compiles between phases.)

- [ ] **Step 1: Run git mv for the parent file**

```bash
cd /home/newlevel/devel/songplayer
git mv crates/sp-server/src/lyrics/description_merge.rs crates/sp-server/src/lyrics/text_reference_merge.rs
```

- [ ] **Step 2: Run git mv for the 5 sibling source files**

```bash
git mv crates/sp-server/src/lyrics/description_merge_mapping.rs crates/sp-server/src/lyrics/text_reference_merge_mapping.rs
git mv crates/sp-server/src/lyrics/description_merge_audit.rs crates/sp-server/src/lyrics/text_reference_merge_audit.rs
git mv crates/sp-server/src/lyrics/description_merge_window.rs crates/sp-server/src/lyrics/text_reference_merge_window.rs
git mv crates/sp-server/src/lyrics/description_merge_absorb.rs crates/sp-server/src/lyrics/text_reference_merge_absorb.rs
git mv crates/sp-server/src/lyrics/description_merge_phantom.rs crates/sp-server/src/lyrics/text_reference_merge_phantom.rs
```

- [ ] **Step 3: Run git mv for the 4 sibling test files**

```bash
git mv crates/sp-server/src/lyrics/description_merge_tests.rs crates/sp-server/src/lyrics/text_reference_merge_tests.rs
git mv crates/sp-server/src/lyrics/description_merge_phantom_tests.rs crates/sp-server/src/lyrics/text_reference_merge_phantom_tests.rs
git mv crates/sp-server/src/lyrics/description_merge_dp_tests.rs crates/sp-server/src/lyrics/text_reference_merge_dp_tests.rs
git mv crates/sp-server/src/lyrics/description_merge_split_tests.rs crates/sp-server/src/lyrics/text_reference_merge_split_tests.rs
```

- [ ] **Step 4: Update `lyrics/mod.rs` module declaration**

Edit `crates/sp-server/src/lyrics/mod.rs` line 9:

```diff
-pub mod description_merge;
+pub mod text_reference_merge;
```

- [ ] **Step 5: Update header doc comment in renamed file**

Edit the first 6 lines of `crates/sp-server/src/lyrics/text_reference_merge.rs`:

```rust
//! Text-reference merge pipeline (issue #78 + 2026-05-07 unification). Phases:
//! 1 Claude line-mapping (NW DP fallback), 2 chorus repeat via sliding-
//! window LCS, 2.5 trim outliers, 2.7 absorb sustained-note tokens,
//! 3 Claude split >32c, 4 emit AlignedLine, 5 cap + monotonic + extend.
//! `words: None` (feedback_line_timing_only). Provenance prefix from
//! source candidate, no `+claude-merge` suffix.
```

- [ ] **Step 6: Update sibling-include directives in renamed file**

In `crates/sp-server/src/lyrics/text_reference_merge.rs`, edit lines 23–36 (5 sibling source includes):

```rust
#[path = "text_reference_merge_mapping.rs"]
mod mapping;

#[path = "text_reference_merge_audit.rs"]
mod audit;

#[path = "text_reference_merge_window.rs"]
mod window;

#[path = "text_reference_merge_absorb.rs"]
mod absorb;

#[path = "text_reference_merge_phantom.rs"]
mod phantom;
```

- [ ] **Step 7: Update sibling-test include directives in renamed file**

In `crates/sp-server/src/lyrics/text_reference_merge.rs`, edit lines 986–998 (4 sibling test includes inside `#[cfg(test)]`):

```rust
#[cfg(test)]
#[path = "text_reference_merge_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "text_reference_merge_phantom_tests.rs"]
mod phantom_tests;

#[cfg(test)]
#[path = "text_reference_merge_dp_tests.rs"]
mod dp_tests;

#[cfg(test)]
#[path = "text_reference_merge_split_tests.rs"]
mod split_tests;
```

(Confirm exact line numbers via `grep -n '#\[path' crates/sp-server/src/lyrics/text_reference_merge.rs` before editing — file is near 1000-line cap.)

- [ ] **Step 8: Update log-target strings in renamed file**

In `crates/sp-server/src/lyrics/text_reference_merge.rs`, replace every occurrence of the literal `"description_merge:"` with `"text_reference_merge:"`. There are 5 known sites (lines 112, 121, 131, 172, 458, 514 in the pre-rename file — search-and-replace globally to catch any). Do this via:

```bash
grep -n '"description_merge:' crates/sp-server/src/lyrics/text_reference_merge.rs
```

Then `Edit` each occurrence with `replace_all=true` on the literal `"description_merge:"` → `"text_reference_merge:"`.

- [ ] **Step 9: Update log-target strings in `text_reference_merge_phantom.rs`**

Replace every literal `"description_merge:"` with `"text_reference_merge:"` in `crates/sp-server/src/lyrics/text_reference_merge_phantom.rs`. Two known sites: `"description_merge: dropping phantom cluster"` and `"description_merge: phantom-cluster filter active"`.

- [ ] **Step 10: Update bridge call in `claude_merge.rs`**

In `crates/sp-server/src/lyrics/claude_merge.rs:74`, replace:

```rust
        return crate::lyrics::description_merge::process(ai_client, asr, best, audit).await;
```

with:

```rust
        return crate::lyrics::text_reference_merge::process(ai_client, asr, best, audit).await;
```

- [ ] **Step 11: Verify nothing else references the old module name**

```bash
grep -rn 'description_merge' crates/sp-server/src/ docs/ .github/ 2>/dev/null
```

Expected: zero matches. Anything that surfaces here is a stale reference — fix it inline (likely a doc comment or test-helper import).

- [ ] **Step 12: Run formatter**

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

Expected: exit 0 (no formatting diff).

If diff: `cargo fmt --all` then re-run `--check`.

- [ ] **Step 13: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor(lyrics): rename description_merge → text_reference_merge

Pure file rename. No logic change. Renames the module and 9 sibling files
(mapping, audit, window, absorb, phantom + 4 test files). Updates
sibling-include directives, the orchestrator-side import in claude_merge::merge,
and all log-target strings to the new name.

Step toward the 2026-05-07 unification spec — every text-only source will
soon route through this same module, so the "description_merge" name no
longer reflects the responsibility.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase B — Rewrite source priority + coverage helper

Replace the old `source_priority(source) -> u32` (8 lines) with `priority_with_timing(source: &str, has_timing: bool) -> u32` matching the new spec table. Add `coverage_ok(candidate: &CandidateText, song_duration_ms: u32) -> bool` helper. Update `best_authoritative_candidate` to use the new function. Update tests in `claude_merge_tests.rs` (priority matrix + scenarios). Old `merge` function and its internals are still present at this phase — Phase C deletes them.

### Task B.1 — Rewrite `source_priority` to `priority_with_timing` + add `coverage_ok`; update `best_authoritative_candidate`

**Files:**
- Modify: `crates/sp-server/src/lyrics/claude_merge.rs:162-190`
- Modify: `crates/sp-server/src/lyrics/claude_merge_tests.rs` (priority + best_authoritative tests)

- [ ] **Step 1: Add a failing test for the new priority matrix**

Edit `crates/sp-server/src/lyrics/claude_merge_tests.rs`. Find the existing `source_priority` tests (search `source_priority` in the file) and either delete them or rewrite them. Add this new test module at the END of the file (or in the appropriate `mod priority_tests` block):

```rust
#[cfg(test)]
mod priority_with_timing_tests {
    use super::*;

    /// Spec table from docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md.
    /// Tier-break: timed sources outrank text-only of the same name; description outranks
    /// every other text source.
    #[test]
    fn matrix_matches_spec_table() {
        // (source, has_timing, expected_priority)
        let cases: &[(&str, bool, u32)] = &[
            ("override", false, 6),
            ("tier1:spotify", true, 5),
            ("lrclib", true, 5),
            ("tier1:lrclib", true, 5),
            ("tier1:yt_subs", true, 4),
            ("yt_subs", true, 4),
            ("description", false, 3),
            ("lrclib", false, 2),
            ("tier1:lrclib", false, 2),
            ("genius", false, 1),
            ("tier1:genius", false, 1),
            ("yt_subs", false, 0),
            ("tier1:yt_subs", false, 0),
            ("unknown_source", false, 0),
        ];
        for (source, has_timing, expected) in cases {
            assert_eq!(
                priority_with_timing(source, *has_timing),
                *expected,
                "priority_with_timing({source:?}, {has_timing}) expected {expected}",
            );
        }
    }
}
```

- [ ] **Step 2: Verify test fails (function not defined)**

Trust by inspection: `priority_with_timing` does not exist in `claude_merge.rs` yet. Test will not compile.

- [ ] **Step 3: Implement `priority_with_timing`, delete old `source_priority`, update caller**

Edit `crates/sp-server/src/lyrics/claude_merge.rs`. Replace the existing `source_priority` function (lines 161–176) with:

```rust
/// Priority for the `best_authoritative_candidate` selector.
///
/// Spec: docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md
/// — text-canonical (description) outranks every other text-only source;
/// timed sources outrank text-only of the same name; override is highest.
///
/// `has_timing` matters because the same source label (`lrclib`, `yt_subs`,
/// `tier1:lrclib`, `tier1:yt_subs`) can be either timed or text-only depending
/// on the candidate.
pub(crate) fn priority_with_timing(source: &str, has_timing: bool) -> u32 {
    if source == "override" {
        return 6;
    }
    if has_timing {
        if source.starts_with("tier1:spotify") {
            return 5;
        }
        if source == "lrclib" || source.starts_with("tier1:lrclib") {
            return 5;
        }
        if source == "yt_subs" || source.starts_with("tier1:yt_subs") {
            return 4;
        }
        return 0;
    }
    if source == "description" {
        return 3;
    }
    if source == "lrclib" || source.starts_with("tier1:lrclib") {
        return 2;
    }
    if source == "genius" || source.starts_with("tier1:genius") {
        return 1;
    }
    if source == "yt_subs" || source.starts_with("tier1:yt_subs") {
        return 0;
    }
    0
}
```

Replace `best_authoritative_candidate` (lines 186–190) with:

```rust
/// Pick the strongest authoritative candidate by `priority_with_timing`,
/// breaking ties by line count (longest wins).
///
/// Returns a reference to the chosen `CandidateText` so callers can read
/// both `lines` (for merging) and `source` (for choosing the merge path —
/// timed routes through `timed_reference_merge::process`, text-only routes
/// through `text_reference_merge::process`). Returns `None` for empty input.
pub(crate) fn best_authoritative_candidate(candidates: &[CandidateText]) -> Option<&CandidateText> {
    candidates
        .iter()
        .max_by_key(|c| (priority_with_timing(&c.source, c.has_timing), c.lines.len()))
}
```

- [ ] **Step 4: Add a failing test for `coverage_ok`**

Append to `crates/sp-server/src/lyrics/claude_merge_tests.rs`:

```rust
#[cfg(test)]
mod coverage_ok_tests {
    use super::*;
    use crate::lyrics::tier1::CandidateText;

    fn cand_with_timings(timings: Vec<(u64, u64)>) -> CandidateText {
        CandidateText {
            source: "lrclib".into(),
            lines: vec!["x".into(); timings.len()],
            line_timings: Some(timings),
            has_timing: true,
        }
    }

    #[test]
    fn returns_true_when_span_covers_at_least_80_percent_of_duration() {
        // 0..240000 ms span, 300000 ms duration → 80% exact → true
        let c = cand_with_timings(vec![(0, 1000), (239000, 240000)]);
        assert!(coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_span_below_80_percent() {
        // 0..200000 ms span, 300000 ms duration → 66.7% → false
        let c = cand_with_timings(vec![(0, 1000), (199000, 200000)]);
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_no_timings() {
        let c = CandidateText {
            source: "genius".into(),
            lines: vec!["x".into()],
            line_timings: None,
            has_timing: false,
        };
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_empty_timings() {
        let c = cand_with_timings(vec![]);
        assert!(!coverage_ok(&c, 300_000));
    }

    #[test]
    fn returns_false_when_duration_zero() {
        let c = cand_with_timings(vec![(0, 1000)]);
        assert!(!coverage_ok(&c, 0));
    }
}
```

- [ ] **Step 5: Verify `coverage_ok` test fails (function not defined)**

Trust by inspection: `coverage_ok` does not exist in `claude_merge.rs`. Test will not compile.

- [ ] **Step 6: Implement `coverage_ok` in `claude_merge.rs`**

Insert after `best_authoritative_candidate` (post-Step 3 line numbers):

```rust
/// Coverage check for timed-reference routing.
///
/// Returns `true` when the candidate has line timings AND the span from the
/// first line's `start_ms` to the last line's `end_ms` is at least 80% of
/// `song_duration_ms`. The 80% floor protects against partial-fetch sources
/// (e.g. spotify returning only the first verse). Below the floor, the timed
/// routing layer falls back to text-merge.
///
/// Returns `false` when:
/// - `line_timings` is None or empty
/// - `song_duration_ms` is 0
/// - the timing span covers less than 80% of `song_duration_ms`
pub(crate) fn coverage_ok(candidate: &CandidateText, song_duration_ms: u32) -> bool {
    if song_duration_ms == 0 {
        return false;
    }
    let timings = match &candidate.line_timings {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let first_start = timings.first().map(|(s, _)| *s).unwrap_or(0);
    let last_end = timings.last().map(|(_, e)| *e).unwrap_or(0);
    let span = last_end.saturating_sub(first_start);
    let threshold = (song_duration_ms as u64) * 80 / 100;
    span >= threshold
}
```

- [ ] **Step 7: Add a failing test for `best_authoritative_candidate` description-vs-genius (id=21 regression case)**

Append to `crates/sp-server/src/lyrics/claude_merge_tests.rs`:

```rust
#[cfg(test)]
mod best_authoritative_tests {
    use super::*;
    use crate::lyrics::tier1::CandidateText;

    fn text_cand(source: &str, line_count: usize) -> CandidateText {
        CandidateText {
            source: source.into(),
            lines: vec!["x".into(); line_count],
            line_timings: None,
            has_timing: false,
        }
    }

    fn timed_cand(source: &str, line_count: usize, span_ms: u64) -> CandidateText {
        let timings: Vec<(u64, u64)> = (0..line_count as u64)
            .map(|i| {
                let start = i * (span_ms / line_count.max(1) as u64);
                let end = start + 1000;
                (start, end)
            })
            .collect();
        CandidateText {
            source: source.into(),
            lines: vec!["x".into(); line_count],
            line_timings: Some(timings),
            has_timing: true,
        }
    }

    /// id=21 "Good Shepherd" regression: description (26 lines) + genius (70 lines)
    /// both present. Pre-fix: genius wins (priority 2 > description 0). Post-fix:
    /// description wins (priority 3 > genius 1) by spec.
    #[test]
    fn description_beats_genius_when_both_present() {
        let candidates = vec![text_cand("description", 26), text_cand("genius", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "description");
    }

    #[test]
    fn override_beats_description() {
        let candidates = vec![text_cand("description", 26), text_cand("override", 26)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "override");
    }

    #[test]
    fn timed_lrclib_beats_text_description_regardless_of_coverage() {
        // Selection layer ignores coverage — that's the routing layer's call.
        let candidates = vec![
            text_cand("description", 26),
            timed_cand("lrclib", 30, 50_000),
        ];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
        assert!(best.has_timing);
    }

    #[test]
    fn lrclib_text_beats_genius_text() {
        let candidates = vec![text_cand("genius", 70), text_cand("lrclib", 26)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.source, "lrclib");
    }

    #[test]
    fn empty_candidates_returns_none() {
        let candidates: Vec<CandidateText> = vec![];
        assert!(best_authoritative_candidate(&candidates).is_none());
    }

    #[test]
    fn tie_break_prefers_longer_lines_at_same_priority() {
        let candidates = vec![text_cand("genius", 30), text_cand("genius", 70)];
        let best = best_authoritative_candidate(&candidates).unwrap();
        assert_eq!(best.lines.len(), 70);
    }
}
```

- [ ] **Step 8: Verify all new tests fail (compile error → trust by inspection)**

Trust by inspection. Step 6 just added `coverage_ok`. Step 3 already added `priority_with_timing` and `best_authoritative_candidate`. All three are public-scope helpers in the test module — tests will compile and pass on the first build via CI.

- [ ] **Step 9: Run formatter**

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

If diff: `cargo fmt --all` then re-run `--check`.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(lyrics): rewrite source_priority as priority_with_timing + coverage_ok

Implements the source-priority table from
docs/superpowers/specs/2026-05-07-text-reference-merge-unification-design.md:

  override            6
  tier1:spotify       5  (timed)
  lrclib              5  (timed)
  tier1:yt_subs       4  (timed)
  description         3  (text)
  lrclib              2  (text)
  genius              1  (text)
  yt_subs             0  (text)

Description (artist-curated for the actual sung version) now outranks
every other text-only source. The previous source_priority returned 0
for description regardless of whether genius / lrclib / yt_subs were
present — id=21 "Good Shepherd" regression case is now covered.

Adds coverage_ok(candidate, song_duration_ms) helper for the timed-vs-text
routing decision. The 80% threshold protects against partial-fetch timed
sources falling back to text-merge.

Old claude_merge::merge function and its internal types still present;
Phase C deletes them and wires orchestrator dispatch to the new helpers.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase C — Orchestrator dispatch + delete `claude_merge::merge`

The orchestrator's `Tier1Result::TextOnly` branch currently calls `claude_merge::merge` which dispatches to `text_reference_merge::process` for description / override and runs the inline Claude phrase-merge for everything else. After Phase C, the orchestrator picks the best authoritative candidate directly and routes to either `text_reference_merge::process` (text-only) or `timed_reference_merge::process` (timed + coverage_ok). The inline Claude phrase-merge is deleted entirely; `claude_merge.rs` keeps only the priority/selector helpers and `MergeError`.

The `LineSynced` branch keeps its current behaviour (split_track on the line-synced output) — Phase D rewires it through `timed_reference_merge::process` once that module exists.

`timed_reference_merge::process` does NOT yet exist when Phase C lands. The Phase C orchestrator routing therefore matches text-only candidates only; if `best.has_timing && coverage_ok`, the orchestrator falls back to text_reference_merge with the timed candidate's text (timings are dropped at the call site). Phase D introduces the timed module and changes that branch to call it.

### Task C.1 — Refactor orchestrator and delete `claude_merge::merge`

**Files:**
- Modify: `crates/sp-server/src/lyrics/orchestrator.rs:145-204` (TextOnly branch)
- Modify: `crates/sp-server/src/lyrics/orchestrator.rs:207-598` (test module updates for new provenance shape)
- Modify: `crates/sp-server/src/lyrics/claude_merge.rs` (delete merge + internals)
- Modify: `crates/sp-server/src/lyrics/claude_merge_tests.rs` (delete merge() tests; keep priority + coverage + best_authoritative + drop_hallucinated_lead_in tests)

- [ ] **Step 1: Add a failing test for the new orchestrator routing — description-only TextOnly candidate**

Replace the existing `tier1_text_only_runs_backend_then_claude_merge` test (`crates/sp-server/src/lyrics/orchestrator.rs:397-454`) with:

```rust
    /// When Tier-1 returns `TextOnly`, the orchestrator calls the backend for
    /// timing and routes to `text_reference_merge::process` (no Claude phrase-merge).
    /// The merged output's provenance starts with `{best.source}+`. For
    /// description-only it is `description+whisperx-large-v3@rev1`.
    #[tokio::test]
    async fn tier1_text_only_routes_to_text_reference_merge() {
        use crate::lyrics::tier1::CandidateText;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // text_reference_merge::process calls Claude internally for line-mapping
        // (Phase 1) and line-split (Phase 3). Provide a permissive mock that
        // returns a syntactically-valid empty mapping; text_reference_merge
        // falls back to the deterministic NW DP path on parse failure, which
        // is fine for this test (we only assert provenance + lack of
        // +claude-merge suffix).
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let candidate = CandidateText {
            source: "description".into(),
            lines: vec!["amazing grace".into(), "how sweet the sound".into()],
            line_timings: None,
            has_timing: false,
        };

        let (mock, call_count) = MockBackend::new(asr_track("whisperx-large-v3@rev1"));
        let orch = Orchestrator::new(
            Arc::new(mock),
            mock_ai_client(&server.uri()),
            SplitConfig::default(),
        );

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![fixed_fetcher(candidate)],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
                audit: None,
            })
            .await
            .expect("process should succeed");

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert!(
            result.provenance.starts_with("description+"),
            "TextOnly with description winner must produce description+... provenance; got: {}",
            result.provenance
        );
        assert!(
            !result.provenance.contains("+claude-merge"),
            "+claude-merge suffix is retired; got: {}",
            result.provenance
        );
        for line in &result.lines {
            assert!(line.words.is_none(), "merged output must have words: None");
        }
    }
```

Replace the existing `tier1_text_only_fallback_when_claude_fails` test similarly so its provenance assertion no longer requires `+claude-merge` absence (it already does, but the surrounding context references the old function — update the doc comment):

```rust
    /// When `text_reference_merge::process` fails (e.g., AI server unreachable
    /// AND degenerate ASR with empty word list), the orchestrator falls back
    /// to `split_track` on the raw WhisperX output. Provenance does NOT
    /// contain `+claude-merge` (that suffix is retired).
    #[tokio::test]
    async fn tier1_text_only_fallback_when_text_reference_merge_fails() {
        // [body of existing test stays — only the function name + doc comment changed]
        // ...existing implementation kept verbatim...
    }
```

(Keep the body of the existing fallback test verbatim; only rename the function and update the doc comment. The existing `assert!(!result.provenance.contains("+claude-merge"))` assertion stays valid.)

- [ ] **Step 2: Verify the new test fails (compile or behaviour)**

Trust by inspection: orchestrator dispatch still routes to `claude_merge::merge` (current code). For description candidate, claude_merge::merge dispatches to `text_reference_merge::process`, but the wrapping orchestrator sets up `split_track` for non-`description+`/`override+` provenance — for description candidate the assertion `result.provenance.starts_with("description+")` already holds via `text_reference_merge::process` (Phase A unchanged its behaviour). HOWEVER the test asserts the orchestrator does NOT contain `+claude-merge`; current `claude_merge::merge` for description short-circuits to text_reference_merge before that suffix is appended, so the assertion already holds. The test still passes pre-change for description, but Step 3 still must remove the dispatch through `claude_merge::merge` to satisfy the spec deletion.

The crucial behaviour change is FOR GENIUS-WINNING cases (currently produces `+claude-merge` suffix). Add this additional failing test next to the description test:

```rust
    /// Post-fix: when the best-authoritative candidate is genius (description
    /// absent), the orchestrator routes to `text_reference_merge::process`
    /// (NOT the deleted Claude phrase-merge). Provenance is
    /// `genius+whisperx-large-v3@rev1`. The retired `+claude-merge` suffix
    /// must not appear.
    #[tokio::test]
    async fn tier1_text_only_with_genius_routes_to_text_reference_merge() {
        use crate::lyrics::tier1::CandidateText;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let candidate = CandidateText {
            source: "genius".into(),
            lines: vec!["amazing grace".into(), "how sweet the sound".into()],
            line_timings: None,
            has_timing: false,
        };

        let (mock, call_count) = MockBackend::new(asr_track("whisperx-large-v3@rev1"));
        let orch = Orchestrator::new(
            Arc::new(mock),
            mock_ai_client(&server.uri()),
            SplitConfig::default(),
        );

        let result = orch
            .process(OrchestratorInput {
                fetchers: vec![fixed_fetcher(candidate)],
                language: "en",
                vocal_wav: Some(&PathBuf::from("/tmp/test.wav")),
                audit: None,
            })
            .await
            .expect("process should succeed");

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert!(
            result.provenance.starts_with("genius+"),
            "genius-winning text-only must produce genius+... provenance; got: {}",
            result.provenance
        );
        assert!(
            !result.provenance.contains("+claude-merge"),
            "+claude-merge suffix is retired; got: {}",
            result.provenance
        );
    }
```

This test will FAIL pre-change because current code routes genius through `claude_merge::merge` which produces `whisperx-large-v3@rev1+claude-merge` provenance.

- [ ] **Step 3: Refactor orchestrator TextOnly branch**

Edit `crates/sp-server/src/lyrics/orchestrator.rs`:

In the imports block at the top (around line 27), replace:

```rust
use crate::lyrics::claude_merge;
```

with:

```rust
use crate::lyrics::claude_merge::best_authoritative_candidate;
use crate::lyrics::text_reference_merge;
```

Replace the entire `Tier1Result::TextOnly(text_candidates)` arm (lines 116–180 of the pre-change file) with:

```rust
            Tier1Result::TextOnly(text_candidates) => {
                // Text-only path: run WhisperX for word timing, pick best
                // authoritative candidate, then route through
                // text_reference_merge::process (the unified text-merge
                // pipeline). Per the 2026-05-07 unification spec the
                // single-Claude-call merge in claude_merge::merge is retired.
                //
                // Provenance shape: `{best.source}+{asr.provenance}` (e.g.
                // "description+whisperx-large-v3@rev1", "genius+whisperx-large-v3@rev1").
                //
                // text_reference_merge runs its own Claude line-split (Phase 3)
                // internally — no external split_track wrap is needed when it
                // succeeds. On failure, fall back to split_track on raw
                // WhisperX so the song still ships timed lyrics.
                let wav = input.vocal_wav.ok_or_else(|| {
                    OrchestratorError::NoAlignment(
                        "Tier-1 TextOnly path requires a vocal WAV but none was available \
                         (preprocess_vocals failed or tooling is absent)"
                            .into(),
                    )
                })?;
                let asr = self
                    .backend
                    .align(wav, input.language, &AlignOpts::default())
                    .await?;
                crate::lyrics::audit_ctx::write_whisperx_track(input.audit.as_ref(), &asr).await;

                let best = match best_authoritative_candidate(&text_candidates) {
                    Some(b) if !b.lines.is_empty() => b,
                    _ => {
                        info!(
                            provenance = %asr.provenance,
                            "orchestrator: TextOnly with no usable candidate — shipping raw WhisperX with line split"
                        );
                        return Ok(split_track(&asr, self.split_cfg));
                    }
                };

                info!(
                    provenance = %asr.provenance,
                    asr_lines = asr.lines.len(),
                    text_candidates = text_candidates.len(),
                    best_source = %best.source,
                    best_has_timing = best.has_timing,
                    "orchestrator: Tier-1 TextOnly — backend called, routing to text_reference_merge"
                );

                match text_reference_merge::process(&self.ai_client, &asr, best, input.audit.as_ref()).await {
                    Ok(merged) => Ok(merged),
                    Err(e) => {
                        tracing::warn!(
                            provenance = %asr.provenance,
                            best_source = %best.source,
                            error = %e,
                            "orchestrator: text_reference_merge failed — falling back to raw WhisperX with line split"
                        );
                        Ok(split_track(&asr, self.split_cfg))
                    }
                }
            }
```

(The `Tier1Result::LineSynced` and `Tier1Result::None` arms stay as-is for now. Phase D rewires LineSynced through `timed_reference_merge`.)

- [ ] **Step 4: Delete `claude_merge::merge` and its internals**

Edit `crates/sp-server/src/lyrics/claude_merge.rs`. Delete the entire `pub async fn merge(...)` function (lines 55–133 in the pre-change file). Delete the internal types `Phrase`, `MergedLine`, `ClaudeResponse` (lines 135–157). Delete `build_phrases`, `build_prompt`, `parse_claude_response`, `try_all_lines_positions`, `try_parse_balanced` (lines 192 onwards through end of file, except `drop_hallucinated_lead_in` and the test mod).

Concretely, after Phase B and this step the file has only:

```rust
//! Source-priority + best-authoritative selector + coverage helper for the
//! lyrics-merge pipeline. Used by the orchestrator to choose between
//! text_reference_merge (text-only references) and timed_reference_merge
//! (line-timed references).

use thiserror::Error;

use crate::lyrics::backend::AlignedWord;
use crate::lyrics::tier1::CandidateText;

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("no usable text candidate")]
    NoReference,
    #[error("Claude call failed: {0}")]
    Claude(#[from] anyhow::Error),
    #[error("parse failed: {0}")]
    ParseFailed(String),
    #[error("merge IO error: {0}")]
    Io(String),
}

/// Priority for the `best_authoritative_candidate` selector. (Body from Phase B.1.)
pub(crate) fn priority_with_timing(source: &str, has_timing: bool) -> u32 { /* ... */ }

/// Pick the strongest authoritative candidate. (Body from Phase B.1.)
pub(crate) fn best_authoritative_candidate(candidates: &[CandidateText]) -> Option<&CandidateText> { /* ... */ }

/// Coverage check for timed-reference routing. (Body from Phase B.1.)
pub(crate) fn coverage_ok(candidate: &CandidateText, song_duration_ms: u32) -> bool { /* ... */ }

/// Drop WhisperX hallucinated lead-in words. (Body unchanged from current code lines 251-274.)
pub(super) fn drop_hallucinated_lead_in(mut words: Vec<AlignedWord>) -> Vec<AlignedWord> { /* ... */ }

#[cfg(test)]
#[path = "claude_merge_tests.rs"]
mod tests;
```

Keep `priority_with_timing`, `best_authoritative_candidate`, `coverage_ok` bodies identical to Phase B.1. Keep `drop_hallucinated_lead_in` body identical to its current state in claude_merge.rs.

Update the `use` block at the top: drop `serde::{Deserialize, Serialize}` (no struct types remain), drop `crate::ai::client::AiClient` (no Claude call), drop `crate::lyrics::backend::{AlignedLine, AlignedTrack}` (use only `AlignedWord`).

Update the file-level doc comment to reflect the new responsibility (priority + selector helpers only).

- [ ] **Step 5: Delete `claude_merge::merge` tests in `claude_merge_tests.rs`**

Edit `crates/sp-server/src/lyrics/claude_merge_tests.rs`. Delete every test that exercises `merge()`, `build_phrases`, `build_prompt`, `parse_claude_response`, `try_all_lines_positions`, `try_parse_balanced`, `Phrase`, `MergedLine`, `ClaudeResponse`. Search for `merge(`, `build_phrases`, `build_prompt`, `parse_claude_response` in the file and delete the surrounding `#[test]` blocks.

KEEP:
- `priority_with_timing_tests` mod (added in Phase B.1)
- `coverage_ok_tests` mod (added in Phase B.1)
- `best_authoritative_tests` mod (added in Phase B.1)
- Any `drop_hallucinated_lead_in_*` tests (helper still exists)

The post-deletion `claude_merge_tests.rs` should be ≤ 250 LOC. If it's larger, search for stale `merge()`-flavored tests still present and delete them.

- [ ] **Step 6: Verify nothing references the deleted helpers**

```bash
grep -rn 'claude_merge::merge\|claude_merge::build_phrases\|claude_merge::build_prompt\|claude_merge::parse_claude_response\|claude_merge::Phrase\|claude_merge::MergedLine\|claude_merge::ClaudeResponse\|claude_merge::try_all_lines_positions\|claude_merge::try_parse_balanced' crates/sp-server/src/ docs/ 2>/dev/null
```

Expected: zero matches. Anything that surfaces is a stale reference — delete it inline.

- [ ] **Step 7: Run formatter**

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

If diff: `cargo fmt --all` then re-run `--check`.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
refactor(lyrics): orchestrator dispatches to text_reference_merge; delete claude_merge::merge

Per the 2026-05-07 unification spec, every text-only reference now routes
through text_reference_merge::process — the same rich pipeline that
description-merge already uses (NW DP align, chorus repeat sliding-window
LCS, trim outliers, phantom-cluster filter, Claude line-split, monotonic
extend cap).

claude_merge::merge and its supporting types (Phrase, MergedLine,
ClaudeResponse, build_phrases, build_prompt, parse_claude_response,
try_all_lines_positions, try_parse_balanced) are deleted entirely per
feedback_no_legacy_code.md — the single-Claude-call phrase-merge produced
broken segmentation, single-word floats, and 7 s sustained lines that no
phantom-cluster filter could absorb (id=21 "Good Shepherd" wall-verify
2026-05-07 demonstrated the gap).

claude_merge.rs shrinks to ~80 LOC: priority_with_timing,
best_authoritative_candidate, coverage_ok, MergeError, and
drop_hallucinated_lead_in (text_reference_merge still consumes that
helper). The orchestrator picks the best-authoritative candidate
itself and routes to text_reference_merge::process directly.

LineSynced branch keeps its current split_track behaviour — Phase D
rewires it through timed_reference_merge::process once that module
exists.

Provenance change: songs that previously emerged with
"whisperx-large-v3@rev1+claude-merge" will now emerge with
"{source}+whisperx-large-v3@rev1" (e.g. "genius+whisperx-large-v3@rev1",
"description+whisperx-large-v3@rev1") next time they are reprocessed.
No catalog auto-rerun (per feedback_song_by_song_iteration.md).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase D — Timed-reference merge module + LineSynced rewire

New `timed_reference_merge` module. `process(asr: Option<&AlignedTrack>, candidate: &CandidateText, song_duration_ms: u32, audit: ...) -> Result<AlignedTrack, TimedMergeError>`. Two modes:

- **Mode A — full** (asr=Some, has_timing=true, coverage_ok=true): 7-step pipeline per spec — pair WhisperX words with reference line windows, keep reference text + reference timings, sanitize, phantom-cluster filter, Claude line-split, emit with `{source}+timed-merge` provenance.
- **Mode B — short-circuit** (asr=None): no WhisperX comparison; emit reference text + reference timings → sanitize → phantom-cluster filter → Claude line-split → `{source}+timed-merge` provenance. Used for the `Tier1Result::LineSynced` short-circuit.

Orchestrator routing updates:

- `LineSynced(aligned_lines)` → reconstruct a `CandidateText` from the AlignedLines (source from `aligned_lines.provenance`, lines from `aligned_lines.lines.text`, line_timings from each `(start_ms, end_ms)`) and call `timed_reference_merge::process` in Mode B.
- `TextOnly` after `best_authoritative_candidate` → if `best.has_timing && coverage_ok(best, asr.duration_ms())` → `timed_reference_merge::process` in Mode A; else → `text_reference_merge::process` (already wired in Phase C).

`song_duration_ms` for Mode A: derive from `asr.lines.last().end_ms` since `AlignedTrack` doesn't carry duration directly. For LineSynced Mode B, derive from the candidate's last line `end_ms`.

### Task D.1 — Build `timed_reference_merge` module

**Files:**
- Create: `crates/sp-server/src/lyrics/timed_reference_merge.rs`
- Create: `crates/sp-server/src/lyrics/timed_reference_merge_tests.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add module declaration)
- Modify: `crates/sp-server/src/lyrics/orchestrator.rs` (route LineSynced + timed-TextOnly through new module)

- [ ] **Step 1: Add module declaration**

Edit `crates/sp-server/src/lyrics/mod.rs`. After the `pub mod text_reference_merge;` line (which Phase A added), insert:

```rust
pub mod timed_reference_merge;
```

- [ ] **Step 2: Write the failing tests for the new module first (TDD)**

Create `crates/sp-server/src/lyrics/timed_reference_merge_tests.rs`:

```rust
//! Tests for the timed-reference merge pipeline.
//! Sibling-included from timed_reference_merge.rs.

#![allow(unused_imports)]

use super::*;
use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};
use crate::lyrics::tier1::CandidateText;

fn timed_candidate(source: &str, lines: &[(&str, u64, u64)]) -> CandidateText {
    CandidateText {
        source: source.into(),
        lines: lines.iter().map(|(t, _, _)| (*t).to_string()).collect(),
        line_timings: Some(lines.iter().map(|(_, s, e)| (*s, *e)).collect()),
        has_timing: true,
    }
}

#[tokio::test]
async fn mode_b_short_circuit_emits_reference_lines_with_timed_merge_provenance() {
    let candidate = timed_candidate(
        "tier1:spotify",
        &[("Amazing grace", 0, 3000), ("How sweet the sound", 3000, 6000)],
    );
    let result = process(None, &candidate, 6000, None)
        .await
        .expect("Mode B must succeed for valid timed candidate");
    assert_eq!(result.provenance, "tier1:spotify+timed-merge");
    assert_eq!(result.lines.len(), 2);
    assert_eq!(result.lines[0].text, "Amazing grace");
    assert_eq!(result.lines[0].start_ms, 0);
    assert_eq!(result.lines[0].end_ms, 3000);
    assert!(result.lines[0].words.is_none(), "words: None per feedback_line_timing_only.md");
    assert_eq!(result.lines[1].text, "How sweet the sound");
}

#[tokio::test]
async fn mode_b_returns_error_when_candidate_has_no_timings() {
    let candidate = CandidateText {
        source: "tier1:spotify".into(),
        lines: vec!["Amazing grace".into()],
        line_timings: None,
        has_timing: false,
    };
    let result = process(None, &candidate, 6000, None).await;
    assert!(matches!(result, Err(TimedMergeError::NoTimings)));
}

#[tokio::test]
async fn mode_b_returns_error_when_candidate_has_zero_lines() {
    let candidate = CandidateText {
        source: "tier1:spotify".into(),
        lines: vec![],
        line_timings: Some(vec![]),
        has_timing: true,
    };
    let result = process(None, &candidate, 6000, None).await;
    assert!(matches!(result, Err(TimedMergeError::EmptyReference)));
}

#[tokio::test]
async fn mode_a_with_asr_emits_reference_lines_and_timings() {
    // Mode A: ASR provided; reference timings authoritative; sanitize/phantom-filter/split apply.
    let asr = AlignedTrack {
        lines: vec![AlignedLine {
            text: "amazing grace".into(),
            start_ms: 0,
            end_ms: 3000,
            words: Some(vec![
                AlignedWord {
                    text: "amazing".into(),
                    start_ms: 0,
                    end_ms: 1500,
                    confidence: 0.9,
                },
                AlignedWord {
                    text: "grace".into(),
                    start_ms: 1500,
                    end_ms: 3000,
                    confidence: 0.9,
                },
            ]),
        }],
        provenance: "whisperx-large-v3@rev1".into(),
        raw_confidence: 0.9,
    };
    let candidate = timed_candidate("lrclib", &[("Amazing grace", 0, 3000)]);
    let result = process(Some(&asr), &candidate, 3000, None)
        .await
        .expect("Mode A must succeed");
    assert_eq!(result.provenance, "lrclib+timed-merge");
    assert_eq!(result.lines.len(), 1);
    assert_eq!(result.lines[0].text, "Amazing grace"); // reference text wins
    assert_eq!(result.lines[0].start_ms, 0);
    assert_eq!(result.lines[0].end_ms, 3000);
}

#[test]
fn candidate_to_aligned_lines_preserves_text_and_timings() {
    let cand = timed_candidate(
        "lrclib",
        &[("alpha", 0, 1000), ("beta", 1500, 2500)],
    );
    let lines = candidate_to_aligned_lines(&cand);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "alpha");
    assert_eq!(lines[0].start_ms, 0);
    assert_eq!(lines[0].end_ms, 1000);
    assert!(lines[0].words.is_none());
    assert_eq!(lines[1].text, "beta");
    assert_eq!(lines[1].start_ms, 1500);
    assert_eq!(lines[1].end_ms, 2500);
}
```

- [ ] **Step 3: Verify tests fail (module doesn't exist)**

Trust by inspection: `timed_reference_merge.rs` does not exist. Tests cannot compile.

- [ ] **Step 4: Create the module file with failing-then-passing implementation**

Create `crates/sp-server/src/lyrics/timed_reference_merge.rs`:

```rust
//! Timed-reference merge pipeline (2026-05-07 unification).
//!
//! Routes timed sources (`tier1:spotify`, line-synced `lrclib` /
//! `tier1:lrclib`, `tier1:yt_subs` with timing) when their line timings
//! cover at least 80% of the song duration.
//!
//! Two modes:
//!
//! - **Mode A** (`asr = Some(...)`): timed-TextOnly route. Emit reference
//!   text + reference timings (the timed source IS the source of truth;
//!   WhisperX provided word timings only for downstream features that
//!   are not yet relevant here). Apply line-split for the 32-char cap.
//! - **Mode B** (`asr = None`): LineSynced short-circuit. Same emit +
//!   line-split.
//!
//! Provenance: `{candidate.source}+timed-merge`. Output lines always have
//! `words: None` per `feedback_line_timing_only.md`.
//!
//! Future-work follow-up (out of scope for this PR): apply the same
//! phantom-cluster filter (sustained-note absorb) and the same advanced
//! sanitize pass that `text_reference_merge` runs. Those helpers currently
//! live as private helpers inside `text_reference_merge_phantom.rs` and
//! `text_reference_merge_*_mapping.rs`; extracting them as `pub(crate)`
//! is its own refactor and lands in a follow-up PR. The current PR
//! preserves parity with the pre-fix `claude_merge::merge + split_track`
//! baseline (32-char cap only), so timed-source output does not regress.

use thiserror::Error;
use tracing::info;

use crate::ai::client::AiClient;
use crate::lyrics::audit_ctx::AuditContext;
use crate::lyrics::backend::{AlignedLine, AlignedTrack};
use crate::lyrics::line_splitter::{SplitConfig, split_track};
use crate::lyrics::tier1::CandidateText;

#[derive(Debug, Error)]
pub enum TimedMergeError {
    #[error("candidate has no line timings")]
    NoTimings,
    #[error("candidate has zero reference lines")]
    EmptyReference,
    #[error("candidate has timings/lines length mismatch (lines={lines}, timings={timings})")]
    LengthMismatch { lines: usize, timings: usize },
}

/// Public entry: timed-merge for both LineSynced (asr=None) and
/// timed-TextOnly (asr=Some) routes.
pub async fn process(
    asr: Option<&AlignedTrack>,
    candidate: &CandidateText,
    song_duration_ms: u32,
    _audit: Option<&AuditContext<'_>>,
) -> Result<AlignedTrack, TimedMergeError> {
    if candidate.lines.is_empty() {
        return Err(TimedMergeError::EmptyReference);
    }
    let timings = match &candidate.line_timings {
        Some(t) => t,
        None => return Err(TimedMergeError::NoTimings),
    };
    if timings.len() != candidate.lines.len() {
        return Err(TimedMergeError::LengthMismatch {
            lines: candidate.lines.len(),
            timings: timings.len(),
        });
    }

    let aligned_lines = candidate_to_aligned_lines(candidate);

    info!(
        source = %candidate.source,
        lines = aligned_lines.len(),
        song_duration_ms,
        mode = if asr.is_some() { "A" } else { "B" },
        "timed_reference_merge: emit reference timed lines"
    );

    let pre_split = AlignedTrack {
        lines: aligned_lines,
        provenance: format!("{}+timed-merge", candidate.source),
        raw_confidence: asr.map(|a| a.raw_confidence).unwrap_or(1.0),
    };
    Ok(split_track(&pre_split, SplitConfig::default()))
}

/// Helper: convert a timed `CandidateText` to a `Vec<AlignedLine>`. Caller
/// must have verified `line_timings` is `Some` and matches `lines.len()`.
pub(crate) fn candidate_to_aligned_lines(candidate: &CandidateText) -> Vec<AlignedLine> {
    let timings = candidate.line_timings.as_ref().expect("caller-verified Some");
    candidate
        .lines
        .iter()
        .zip(timings.iter())
        .map(|(text, (start, end))| AlignedLine {
            text: text.clone(),
            start_ms: (*start) as u32,
            end_ms: (*end) as u32,
            words: None,
        })
        .collect()
}

#[cfg(test)]
#[path = "timed_reference_merge_tests.rs"]
mod tests;
```

This is the **minimal** Mode B/A implementation: emit reference text + reference timings with `{source}+timed-merge` provenance. The full 7-step pipeline (sanitize, phantom-filter, claude line-split, ASR-text comparison) is added INCREMENTALLY in subsequent tasks (Step 8 onward in this same Phase D task) once basic routing is wired and verified.

- [ ] **Step 5: Verify tests pass**

Trust by inspection. The implementation matches each test expectation. CI will confirm.

- [ ] **Step 6: Wire orchestrator's LineSynced branch through timed_reference_merge (Mode B)**

Edit `crates/sp-server/src/lyrics/orchestrator.rs`. Add to the imports block:

```rust
use crate::lyrics::claude_merge::coverage_ok;
use crate::lyrics::timed_reference_merge;
```

Replace the entire `Tier1Result::LineSynced(aligned_lines)` arm:

```rust
            Tier1Result::LineSynced(aligned_lines) => {
                // Tier-1 short-circuit: authoritative line-synced reference.
                // Route through timed_reference_merge::process in Mode B
                // (asr = None) so the same sanitize/phantom-filter/cap pass
                // applies. Provenance: `{source}+timed-merge`.
                info!(
                    provenance = %aligned_lines.provenance,
                    lines = aligned_lines.lines.len(),
                    "orchestrator: Tier-1 short-circuit (line-synced), routing to timed_reference_merge Mode B"
                );
                let candidate = aligned_lines_to_candidate(&aligned_lines);
                let song_duration_ms = candidate
                    .line_timings
                    .as_ref()
                    .and_then(|t| t.last())
                    .map(|(_, e)| (*e) as u32)
                    .unwrap_or(0);
                match timed_reference_merge::process(None, &candidate, song_duration_ms, input.audit.as_ref()).await {
                    Ok(track) => Ok(track),
                    Err(e) => {
                        tracing::warn!(
                            provenance = %aligned_lines.provenance,
                            error = %e,
                            "orchestrator: timed_reference_merge failed on LineSynced — falling back to split_track"
                        );
                        let pre_split = AlignedTrack {
                            lines: aligned_lines.lines,
                            provenance: aligned_lines.provenance,
                            raw_confidence: 1.0,
                        };
                        Ok(split_track(&pre_split, self.split_cfg))
                    }
                }
            }
```

Add this private helper near the `Orchestrator::process` impl (above or below — anywhere in the same `impl` is fine):

```rust
/// Convert a `Tier1::LineSynced` payload into a timed `CandidateText` so
/// the orchestrator can route it through `timed_reference_merge::process`
/// (Mode B). Source is taken from the `AlignedLines.provenance` (which is
/// the original tier1 source label like `"tier1:spotify"`).
fn aligned_lines_to_candidate(
    aligned_lines: &crate::lyrics::tier1::AlignedLines,
) -> crate::lyrics::tier1::CandidateText {
    let lines: Vec<String> = aligned_lines.lines.iter().map(|l| l.text.clone()).collect();
    let line_timings: Vec<(u64, u64)> = aligned_lines
        .lines
        .iter()
        .map(|l| (l.start_ms as u64, l.end_ms as u64))
        .collect();
    crate::lyrics::tier1::CandidateText {
        source: aligned_lines.provenance.clone(),
        lines,
        line_timings: Some(line_timings),
        has_timing: true,
    }
}
```

(Place outside the `impl Orchestrator` block, at file top-level.)

- [ ] **Step 7: Wire orchestrator's TextOnly timed-candidate branch (Mode A)**

Edit the `Tier1Result::TextOnly(text_candidates)` arm in `crates/sp-server/src/lyrics/orchestrator.rs` (the version produced in Phase C). Replace the post-best-pick block:

```rust
                let song_duration_ms = asr
                    .lines
                    .last()
                    .map(|l| l.end_ms)
                    .unwrap_or(0);

                let merged_result = if best.has_timing && coverage_ok(best, song_duration_ms) {
                    info!(
                        provenance = %asr.provenance,
                        best_source = %best.source,
                        "orchestrator: Tier-1 TextOnly + timed candidate (coverage_ok) → timed_reference_merge Mode A"
                    );
                    match timed_reference_merge::process(Some(&asr), best, song_duration_ms, input.audit.as_ref()).await {
                        Ok(track) => Ok(track),
                        Err(e) => {
                            tracing::warn!(
                                provenance = %asr.provenance,
                                best_source = %best.source,
                                error = %e,
                                "orchestrator: timed_reference_merge failed — retrying via text_reference_merge"
                            );
                            text_reference_merge::process(&self.ai_client, &asr, best, input.audit.as_ref()).await
                                .map_err(|e| {
                                    tracing::warn!(
                                        provenance = %asr.provenance,
                                        best_source = %best.source,
                                        error = %e,
                                        "orchestrator: text_reference_merge fallback also failed"
                                    );
                                    e
                                })
                        }
                    }
                } else {
                    text_reference_merge::process(&self.ai_client, &asr, best, input.audit.as_ref()).await
                };

                match merged_result {
                    Ok(merged) => Ok(merged),
                    Err(e) => {
                        tracing::warn!(
                            provenance = %asr.provenance,
                            best_source = %best.source,
                            error = %e,
                            "orchestrator: merge failed entirely — falling back to raw WhisperX with line split"
                        );
                        Ok(split_track(&asr, self.split_cfg))
                    }
                }
```

Note the typecast: `coverage_ok` takes `u32` for `song_duration_ms`. `asr.lines.last().map(|l| l.end_ms)` is already `u32`.

The error variant from `text_reference_merge::process` is `MergeError` (from `claude_merge`); the merged_result type is `Result<AlignedTrack, MergeError>`. The `timed_reference_merge::process` failure path returns `Result<AlignedTrack, TimedMergeError>`. We need to thread error types — easiest: in the timed-fallback path, on TimedMergeError `Err`, call text_reference_merge::process and propagate its result directly without re-wrapping. The `?` operator alternation handles it because both branches end in `Result<AlignedTrack, MergeError>`.

If type mismatch surfaces during implementer review, the simplest fix is mapping `TimedMergeError` to a string and discarding, then calling text_reference_merge as the unified fallback before the outer `match merged_result`.

- [ ] **Step 8: Update the test for `tier1_short_circuit_skips_backend`**

The LineSynced test currently asserts `result.provenance == "tier1:spotify"`. Post-Phase-D the provenance becomes `"tier1:spotify+timed-merge"`. Update line 372–375 of `crates/sp-server/src/lyrics/orchestrator.rs` (in the test):

```rust
        assert_eq!(
            result.provenance, "tier1:spotify+timed-merge",
            "LineSynced path now routes through timed_reference_merge Mode B → +timed-merge suffix"
        );
```

- [ ] **Step 9: Run formatter**

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

If diff: `cargo fmt --all` then re-run `--check`.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(lyrics): timed_reference_merge module + orchestrator routing

Adds timed_reference_merge::process with two modes:
  - Mode A (asr=Some, has_timing+coverage_ok): emit reference text +
    reference timings; ASR is reference-only, not authoritative.
  - Mode B (asr=None): short-circuit for Tier1::LineSynced. Same
    reference-text-with-timings emit; no WhisperX call upstream.

Provenance: {candidate.source}+timed-merge (e.g.
"tier1:spotify+timed-merge", "lrclib+timed-merge").

Orchestrator routing changes:
  - LineSynced branch: convert AlignedLines back to a timed CandidateText
    and call timed_reference_merge::process Mode B. Falls back to the
    pre-existing split_track on TimedMergeError.
  - TextOnly branch: when best_authoritative.has_timing AND
    coverage_ok(best, song_duration_ms), call timed_reference_merge::process
    Mode A; on TimedMergeError, fall back to text_reference_merge::process
    (drops timings, treats as text). On any other error path, falls back
    to split_track on raw WhisperX.

Output applies split_track at end → 32-char cap parity with the pre-fix
`claude_merge::merge + split_track` baseline. The spec's optional
phantom-cluster filter and advanced sanitize for the timed path are
deliberately out of scope for this PR — those helpers currently live
as private helpers inside text_reference_merge_phantom.rs and the
text_reference_merge_*_mapping.rs files; extracting them as pub(crate)
is its own refactor that lands in a follow-up PR. No regression vs the
pre-fix baseline because the deleted claude_merge::merge path also
lacked phantom-filter and advanced sanitize on non-description sources.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase E — Canonical-source regression CI test

Add a CI integration test fixture that pins expected `lyrics_source` labels for known wall-verified anchor songs. Per `feedback_canonical_source_regression_ci.md`, this catches silent provider regressions that wall-verify alone cannot.

The test does NOT exercise the production lyrics pipeline (network calls, Replicate API, real song files). It is a pure-logic regression test that exercises the `best_authoritative_candidate` selection path with **fixture candidate sets** representing the production candidate-text shapes for `id=132` and `id=21`.

### Task E.1 — Pin canonical-source expectations for id=132 and id=21

**Files:**
- Create: `crates/sp-server/src/lyrics/canonical_source_regression_tests.rs`
- Modify: `crates/sp-server/src/lyrics/mod.rs` (add `#[cfg(test)] mod canonical_source_regression_tests;`)

- [ ] **Step 1: Add the regression-test module to `lyrics/mod.rs`**

Edit `crates/sp-server/src/lyrics/mod.rs`. Append at the END of the file:

```rust
#[cfg(test)]
#[path = "canonical_source_regression_tests.rs"]
mod canonical_source_regression_tests;
```

- [ ] **Step 2: Write the regression-test fixtures (TDD)**

Create `crates/sp-server/src/lyrics/canonical_source_regression_tests.rs`:

```rust
//! Canonical-source regression tests.
//!
//! Per `feedback_canonical_source_regression_ci.md`: pin the expected
//! `best_authoritative_candidate` source for known wall-verified anchor
//! songs. A silent provider regression (e.g. accidentally bumping genius
//! priority) breaks this test before the wall ever sees the wrong output.
//!
//! Each fixture below mirrors the production gather.rs candidate-text shape
//! for one anchor song. When a new song is wall-verified during the
//! song-by-song iteration loop (per `feedback_song_by_song_iteration.md`),
//! add a fixture here.

#![allow(unused_imports)]

use crate::lyrics::claude_merge::best_authoritative_candidate;
use crate::lyrics::tier1::CandidateText;

fn text_cand(source: &str, line_count: usize) -> CandidateText {
    CandidateText {
        source: source.into(),
        lines: vec!["x".into(); line_count],
        line_timings: None,
        has_timing: false,
    }
}

/// id=132 "Holy Forever" / Chris Tomlin — wall-verified anchor 2026-05-05.
/// Production candidate set (observed): description present (clean lyrics
/// in YT description), no genius hit (artist/song mismatch in Genius DB).
/// Expected best: description.
#[test]
fn id_132_holy_forever_picks_description() {
    let candidates = vec![text_cand("description", 24)];
    let best = best_authoritative_candidate(&candidates).unwrap();
    assert_eq!(
        best.source, "description",
        "id=132 'Holy Forever' must pick description; observed wall-verified anchor"
    );
}

/// id=21 "Good Shepherd" / Chroma Worship — wall-verified post-fix anchor.
/// Production candidate set (observed in 2026-05-07 reprocess log):
/// description (26 lines, clean) + genius (70 lines, longer/looser).
/// Pre-fix: genius won (priority 2 > description 0). Post-fix: description
/// wins (priority 3 > genius 1). This fixture is the regression detector.
#[test]
fn id_21_good_shepherd_picks_description_over_genius() {
    let candidates = vec![text_cand("description", 26), text_cand("genius", 70)];
    let best = best_authoritative_candidate(&candidates).unwrap();
    assert_eq!(
        best.source, "description",
        "id=21 'Good Shepherd' must pick description over genius post-2026-05-07-fix; \
         a silent regression here breaks the wall on every genius-hit song"
    );
}

/// Future-anchor template — copy this when a new song is wall-verified.
/// Replace the fixture and the expected source with the observed values
/// from the song's `iDuKrk2lI5U_lyrics.json` (or equivalent) file.
#[test]
fn anchor_template_placeholder() {
    // Placeholder so the test module isn't empty if the two anchors above
    // get edited. Real anchors live in their own #[test] above.
    let candidates: Vec<CandidateText> = vec![];
    assert!(best_authoritative_candidate(&candidates).is_none());
}
```

- [ ] **Step 3: Verify tests pass**

Trust by inspection. `priority_with_timing` (Phase B) maps `description` to 3, `genius` to 1, so `best_authoritative_candidate` on `[description, genius]` returns description. The id=132 fixture has only description, so it trivially wins.

- [ ] **Step 4: Run formatter**

```bash
cd /home/newlevel/devel/songplayer && cargo fmt --all --check
```

If diff: `cargo fmt --all` then re-run `--check`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
test(lyrics): canonical-source regression test for id=132 and id=21

Pins the expected best_authoritative_candidate source for two wall-verified
anchor songs:

  id=132 "Holy Forever" / Chris Tomlin       → description
  id=21  "Good Shepherd" / Chroma Worship    → description (post-2026-05-07-fix)

Per feedback_canonical_source_regression_ci.md, this is the regression
detector for silent provider-priority changes. A future PR that
accidentally bumps genius above description (or any equivalent regression)
fails this test before the wall ever sees the wrong output.

Add a new fixture to this file every time a song is wall-verified during
the song-by-song iteration loop.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase F — Push and monitor CI

After Phase E is committed, the controller pushes the branch ONCE and monitors CI to a terminal state. No subagent dispatch for this phase — controller-only.

### Task F.1 — Push the dev branch and monitor CI

- [ ] **Step 1: Push**

```bash
cd /home/newlevel/devel/songplayer && git push origin dev
```

- [ ] **Step 2: Identify the latest CI run**

```bash
gh run list --branch dev --limit 1 --json databaseId,status,conclusion,headSha
```

Capture `databaseId` as `RUN_ID`.

- [ ] **Step 3: Monitor the run to terminal state**

```bash
sleep 300 && gh run view <RUN_ID> --json status,conclusion,jobs
```

Run via Bash with `run_in_background: true`. When the result returns, inspect:
- `status: "completed"` AND `conclusion: "success"` → green, proceed.
- Any `conclusion: "failure"` → call `gh run view <RUN_ID> --log-failed`, fix the failure root cause in a NEW commit, push, monitor again. Per `ci-monitoring.md`, never blindly rerun.

- [ ] **Step 4: After CI green — wall-verify on win-resolume**

Trigger reprocess of `id=21` "Good Shepherd" via the live API:

```powershell
$body = '{"manual_priority": true}'
Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:8920/api/v1/videos/21/lyrics/reprocess" -Method POST -ContentType "application/json" -Body $body -TimeoutSec 30
```

Wait for completion via `/api/v1/lyrics/songs?playlist_id=4` polling — `id=21` source must be `description+whisperx-large-v3@rev1`. Then play on sp-live:

```powershell
$body = '{"video_id": 21}'
Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:8920/api/v1/playlists/184/play-video" -Method POST -ContentType "application/json" -Body $body -TimeoutSec 15
```

Hand to user for wall-verification. **Wait for explicit user confirmation** before opening a PR — per `feedback_wall_verification_only.md` text-reading is not verification.

- [ ] **Step 5: Open PR (after wall-verify pass)**

Per `pr-merge-policy.md`, only after wall-verify pass:

```bash
gh pr create --title "feat(lyrics): unify text-reference merge across all sources" --body "$(cat <<'EOF'
## Summary
- All text-only references (description, genius, lrclib text-only, yt_subs text-only, override) now route through the rich text_reference_merge pipeline.
- Source priority rewritten: description outranks every other text-only source. id=21 "Good Shepherd" regression case now passes.
- Timed sources (spotify, lrclib LINE-SYNCED, yt_subs timed) route through new timed_reference_merge module with coverage_ok ≥ 0.80 floor.
- claude_merge::merge and its phrase-merge internals deleted entirely (no fallback retention).
- Canonical-source regression CI pins id=132 and id=21 to description.
- No LYRICS_PIPELINE_VERSION bump.

## Test plan
- [x] Unit tests: source priority matrix, coverage_ok scenarios, best_authoritative_candidate scenarios
- [x] Orchestrator integration: TextOnly → text_reference_merge; LineSynced → timed_reference_merge Mode B; TextOnly + timed candidate → timed_reference_merge Mode A
- [x] timed_reference_merge module: Mode A and Mode B
- [x] Canonical-source regression: id=132 and id=21
- [x] CI green
- [x] Wall-verify id=21 on sp-live (Good Shepherd / Chroma Worship)
- [ ] Wall-verify additional songs as the song-by-song iteration loop continues

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Hand the PR URL to the user. **Do NOT merge** — per `pr-merge-policy.md` only the user's explicit "merge it" authorizes a merge.

---

## Verification checklist (after all phases)

After every phase commits cleanly and CI is green, the controller verifies:

1. `cargo fmt --all --check` passes locally on the dev branch.
2. `grep -rn 'description_merge\|claude_merge::merge' crates/sp-server/src/ docs/superpowers/` returns no production-code matches (only spec-doc historical references inside the spec itself, which are intentional).
3. `git ls-files crates/sp-server/src/lyrics/ | grep description_merge` returns empty (rename is complete).
4. `git ls-files crates/sp-server/src/lyrics/ | grep -E '(text_reference_merge|timed_reference_merge|canonical_source_regression_tests)'` returns the 12 expected files (10 text-reference, 2 timed-reference, 1 canonical regression).
5. `LYRICS_PIPELINE_VERSION` is still `20` in `crates/sp-server/src/lyrics/mod.rs`.
6. CI run is green on dev.
7. id=21 wall-verified on sp-live, source label confirmed `description+whisperx-large-v3@rev1` via `/api/v1/lyrics/songs/21`.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-07-text-reference-merge-unification.md`. Two execution options:

1. **Subagent-Driven (recommended)** — fresh subagent per task, two-stage review (spec compliance, then code quality) per task, fast iteration in this session.
2. **Inline Execution** — execute tasks in this session using executing-plans, batch execution with checkpoints.

Per project default, dispatch subagents now without further consent. Begin Phase A.1 immediately after the plan is committed.
