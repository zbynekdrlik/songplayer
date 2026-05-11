# Text-Reference Merge Unification — Design Spec

**Date:** 2026-05-07
**Status:** Draft
**Working dir:** `/home/newlevel/devel/songplayer`
**Branch:** `dev` (`0.32.0-dev.1`)

---

## Problem

The lyrics-merge pipeline has two divergent code paths producing dramatically different output quality from the same kind of input:

- **`description_merge::process`** — runs only when the chosen authoritative reference has source `"description"` or `"override"`. Rich pipeline: NW DP align reference→ASR, chorus repeat sliding-window LCS, trim outliers, phantom-cluster filter (PR #82), Claude-driven line splits at 32-char cap, monotonic-extend cap.
- **`claude_merge::merge`** — runs for every other source (`genius`, `lrclib` text-only, `yt_subs` text-only, `lrclib` LINE-SYNCED, `spotify`, `yt_subs` timed). Single Claude call merging WhisperX phrases against reference text. No chorus handling, no sustained-vowel filter, no quality line-splitting.

Wall-verification on 2026-05-07 of `id=21` "Good Shepherd" (Chroma Worship) demonstrated the gap. Description had a clean 26-line artist-curated reference but lost the priority race to a 70-line Genius hit (`source_priority`: description=0, genius=2). The Claude-merge output had broken segmentation ("Will you prepare" / "A table before me With" / "all my" split across four lines), single-word floats ("I", "In", "The", "And"), and 7-second sustained "Oh, good shepherd" lines with no phantom-cluster filter to absorb them. By contrast `id=132` "Holy Forever" (Chris Tomlin) had no Genius hit, so description won by default, ran through `description_merge`, and rendered cleanly on the wall.

Two separate defects compound:

1. **Priority bug** — `description` (artist-curated for the actual sung version) ranks below every other text source in `claude_merge::source_priority`.
2. **Pipeline split** — even if description were the canonical pick, every other text source (genius, lrclib text-only, yt_subs text-only) gets the inferior single-Claude-call merge with none of the structure-preserving processing.

The fix must be a SOTA, non-compromise architectural change: ALL text references deserve the rich merge pipeline, AND description must rank above other text sources.

## Goals

1. Unify all text-only references through a single rich text-merge pipeline (currently `description_merge`).
2. Reorder source priority so YouTube description outranks every other text-only source.
3. Preserve the value of timed references (`spotify`, `lrclib` LINE-SYNCED, `yt_subs` timed) by routing them through a path that respects their existing line timings — not destroying them with text-merge re-alignment.
4. No `LYRICS_PIPELINE_VERSION` bump (per project rule). Affected songs reprocessed song-by-song via `manual_priority`.
5. Land as one PR (no split) — the rename + priority change + timed-merge are tightly coupled and should ship atomically.

## Non-goals

- Catalog-wide auto-reprocess. Per `feedback_song_by_song_iteration.md`, pipeline-version-driven mass reprocess is forbidden without explicit approval.
- New providers. Spotify / lrclib / genius / yt_subs / description / override stay as the source set.
- DB schema changes. Lyrics output is a JSON blob in `videos.lyrics_json`; only the `lyrics_source` string and the JSON content shape change for affected songs.

## Source taxonomy & new priority

| Tier | Source | Priority | Has line timings? | Reason |
|---|---|---|---|---|
| Manual | `override` | **6** | no | operator-edited ground truth |
| Timed | `tier1:spotify` | **5** | yes | Spotify LINE_SYNCED |
| Timed | `lrclib` LINE-SYNCED | **5** | yes (when `synced=true`) | curated time-synced |
| Timed | `tier1:yt_subs` (timed) | **4** | yes (when manual subs) | platform-timed |
| Text canonical | `description` | **3** | no | artist-curated for THIS YouTube video |
| Text DB | `lrclib` (text-only) | **2** | no | curated database |
| Text crowd | `genius` | **1** | no | crowd-sourced; often has unsung verses |
| Text auto | `yt_subs` (text-only) | **0** | no | auto-generated, no curation |

Tie-break by line count (longest wins, current behaviour).

`source_priority` becomes a function of both the source string AND `has_timing` — not just the source string — because the same source label (`lrclib`, `yt_subs`) can be either timed or text-only depending on the candidate.

## Routing decision

```
fn pick_best(candidates) -> Option<&CandidateText>:
    candidates.max_by_key(|c| (
        priority_with_timing(c.source, c.has_timing),  // primary
        c.lines.len()                                  // tie-break
    ))

fn route(best: &CandidateText) -> MergePath:
    if best.has_timing && coverage_ok(best, song_duration_ms):
        TimedReferenceMerge
    else:
        TextReferenceMerge
```

- **`coverage_ok`** = `(last_line.end_ms - first_line.start_ms) >= 0.80 * song_duration_ms`. Floor of 80%. If a timed source has line timings but only covers half the song, downgrade to text-merge using its lines as text reference (drop the timings).

## Provenance label format

| Path | Format | Example |
|---|---|---|
| Text-merge | `{source}+{asr.provenance}` | `description+whisperx-large-v3@rev1`, `genius+whisperx-large-v3@rev1`, `lrclib+whisperx-large-v3@rev1` |
| Timed-merge | `{source}+timed-merge` | `tier1:spotify+timed-merge`, `lrclib+timed-merge` |

`description+whisperx-large-v3@rev1` (current `id=132` label) stays unchanged — same path, same output. New labels `genius+...`, `lrclib+...`, `yt_subs+...`, `override+...` are introduced when those sources win.

The legacy `whisperx-large-v3@rev1+claude-merge` source label is **retired**. Songs currently bearing that label get rewritten labels next time they're reprocessed.

## File structure

```
crates/sp-server/src/lyrics/
├── claude_merge.rs                   # SHRINK to source_priority + best_authoritative_candidate
├── claude_merge_tests.rs             # update for new priority table; remove merge() tests
├── text_reference_merge.rs           # RENAME from description_merge.rs
├── text_reference_merge_phantom.rs   # RENAME from description_merge_phantom.rs
├── text_reference_merge_window.rs    # RENAME from description_merge_window.rs
├── text_reference_merge_*_tests.rs   # rename matching test sibling files
├── timed_reference_merge.rs          # NEW
├── timed_reference_merge_tests.rs    # NEW
└── orchestrator.rs                   # update routing dispatch
```

The `description_merge` module becomes `text_reference_merge`. All tests, public types, internal helpers move with it. Module-level docs updated to describe the unified text-only path. Public API surface unchanged: `process(ai_client, asr, best, audit) -> Result<AlignedTrack>`.

`claude_merge::merge` is **deleted**. The current single-Claude-call merge logic has no remaining caller after routing change. Any lingering references (orchestrator, tests, audit) updated.

## Orchestrator dispatch

Current orchestrator (`orchestrator.rs:145-204`) has:

```rust
match claude_merge::merge(...).await {
    Ok(merged) => {
        if provenance.starts_with("description+") || provenance.starts_with("override+") {
            Ok(merged)
        } else {
            Ok(split_track(&merged, self.split_cfg))
        }
    }
    Err(e) => Ok(split_track(&asr, self.split_cfg)),
}
```

Replaced by:

```rust
let best = best_authoritative_candidate(&text_candidates).filter(|b| !b.lines.is_empty());
let merged_result = match best {
    Some(b) if b.has_timing && coverage_ok(b, asr.duration_ms()) => {
        timed_reference_merge::process(asr, b, audit).await
    }
    Some(b) => {
        text_reference_merge::process(ai_client, asr, b, audit).await
    }
    None => return Ok(split_track(&asr, self.split_cfg)),
};
match merged_result {
    Ok(track) => Ok(track),  // text-merge already handles its own splits internally
    Err(e) => {
        warn!(error = %e, "merge failed; falling back to raw WhisperX");
        Ok(split_track(&asr, self.split_cfg))
    }
}
```

`text_reference_merge::process` outputs already pass through Claude-driven line splits as Phase 3 of its own pipeline (current `description_merge` Phase 3). No external `split_track` wrapping needed for that path. Same for `timed_reference_merge` — its own line-split logic is part of the path.

## Timed-reference merge (new module)

`timed_reference_merge::process(asr, best, audit) -> Result<AlignedTrack>`:

1. Take `best.lines` (text) + `best.line_timings` (Vec<(start_ms, end_ms)>) as authoritative line boundaries.
2. For each reference line, find WhisperX words inside its time window (with ±300 ms tolerance).
3. If WhisperX words inside the window match the reference text well (Levenshtein > 0.7 ratio of reference text length), keep reference text + reference timings; else keep reference text + reference timings (timed source IS the truth — WhisperX text divergence is a transcription error, not a source-of-truth challenge).
4. Apply same sanitize/cap/monotonic-extend pass that text-merge runs.
5. Apply the SAME phantom-cluster filter (sustained-vowel absorb) for consistency.
6. Apply the SAME Claude line-split if any line text exceeds the 32-char cap.
7. Provenance: `{best.source}+timed-merge`.

Defines its own `TimedMergeError`. Failure → caller falls back to `text_reference_merge` with `best.line_timings = None` (drops timings, treats as text-only).

## Tests

**Unit tests for source priority (in `claude_merge_tests.rs`):**

- Table-driven matrix:
  - `(override, has_timing=false)` → 6
  - `(tier1:spotify, has_timing=true)` → 5
  - `(lrclib, has_timing=true)` → 5
  - `(tier1:yt_subs, has_timing=true)` → 4
  - `(description, has_timing=false)` → 3
  - `(lrclib, has_timing=false)` → 2
  - `(genius, has_timing=false)` → 1
  - `(yt_subs, has_timing=false)` → 0

**Unit tests for `best_authoritative_candidate`:**

- description + genius both present → description wins (id=21 regression case)
- description + override both present → override wins (priority 6 > 3)
- description + lrclib LINE-SYNCED, lrclib coverage 0.95 → lrclib wins (priority 5 > 3)
- description + lrclib LINE-SYNCED, lrclib coverage 0.5 → still chooses by raw priority (lrclib 5 > description 3); coverage check happens at routing layer, not selection
- empty candidates → None
- lrclib text-only (priority 2) + genius (priority 1) → lrclib wins

**Routing tests (`orchestrator` integration):**

- description-only → text_reference_merge runs, provenance `description+whisperx-large-v3@rev1`
- genius-only → text_reference_merge runs, provenance `genius+whisperx-large-v3@rev1`
- lrclib LINE-SYNCED with coverage ≥ 0.80 → timed_reference_merge runs, provenance `lrclib+timed-merge`
- lrclib LINE-SYNCED with coverage 0.5 → falls through to text_reference_merge with timings dropped, provenance `lrclib+whisperx-large-v3@rev1`
- timed_reference_merge errors → fallback to text_reference_merge

**Canonical-source regression test (per `feedback_canonical_source_regression_ci.md`):**

- New CI integration test pinning expected source labels for known-good fixtures:
  - `id=132` ("Holy Forever" / Chris Tomlin) → `description+whisperx-large-v3@rev1`
  - `id=21` ("Good Shepherd" / Chroma Worship) → `description+whisperx-large-v3@rev1` (post-fix expected)
  - More songs added as wall-verified anchors emerge during song-by-song iteration

## Migration & reprocess strategy

- **No `LYRICS_PIPELINE_VERSION` bump.** Constant stays at 20. Smart-skip remains `>= 20`.
- **No catalog-wide auto-rerun.** Per `feedback_song_by_song_iteration.md` the loop is strict serial: pick one song, reprocess, wall-verify, fix or move on.
- **Existing songs labeled `whisperx-large-v3@rev1+claude-merge` (~60 catalog rows)** keep their current output until manually reprocessed. Each is a wall-verify candidate.
- **`id=21` reprocessed first** as immediate validation that genius-driven songs reach `description+...` (because description outranks genius now) and render cleanly.

## Failure modes & fallbacks

| Failure | Fallback |
|---|---|
| `text_reference_merge::process` returns `Err` | orchestrator falls back to `split_track(&asr, ...)` (raw WhisperX with line splits) |
| `timed_reference_merge::process` returns `Err` | orchestrator retries via `text_reference_merge` with `has_timing=false` (timings dropped, lines treated as text reference) |
| `best_authoritative` returns None (zero candidates) | orchestrator runs `Tier1Result::None` raw-WhisperX path (already exists) |
| Claude refusal in line-split (Phase 3) | text_reference_merge keeps lines un-split (existing behaviour) |
| WhisperX returns zero phrases | Existing degenerate-case handling in current description_merge ports as-is |

## Code-size & risk

- **LOC touched:** ~600-1000 lines.
  - `claude_merge.rs`: ~200 LOC removed (merge/build_phrases/build_prompt/parse_claude_response/MergedLine/ClaudeResponse types). ~30 LOC added (`priority_with_timing`).
  - `claude_merge_tests.rs`: ~100 LOC removed (merge tests). ~80 LOC added (new priority matrix).
  - `description_merge*.rs` files: rename only (no body change). Keep file-size cap.
  - `timed_reference_merge.rs`: ~250 LOC new.
  - `timed_reference_merge_tests.rs`: ~200 LOC new.
  - `orchestrator.rs`: ~30 LOC modified.
- **File-size cap (1000 lines/file)** respected in every renamed/new file.
- **Single PR**, single commit train.

## Open questions

None at spec time. Routing rules are deterministic; priorities are user-decided; new module is bounded.

## Approval gates

1. **Spec approval (this doc).** User reviews and approves before plan-writing.
2. **Plan approval.** `writing-plans` produces task-by-task plan in `docs/superpowers/plans/2026-05-07-text-reference-merge-unification.md`. User reviews.
3. **Implementation review.** Subagent-driven-development; per-task spec-compliance + code-quality review per project workflow.
4. **CI green.** All gates pass before merge candidate.
5. **Wall-verify.** `id=21` reprocessed post-deploy, played on sp-live, user wall-verifies.

## References

- `feedback_song_by_song_iteration.md` — strict serial iteration with inline code fixes
- `feedback_pipeline_version_approval.md` — no `LYRICS_PIPELINE_VERSION` bump without explicit user approval
- `feedback_no_bump_until_proven.md` — never propose pipeline version bumps
- `feedback_canonical_source_regression_ci.md` — pin expected source labels in CI to detect silent provider regressions
- `feedback_wall_verification_only.md` — only LED wall verifies; sample-text reading is not verification
- `feedback_no_legacy_code.md` — when replacing a code path, delete the old one entirely (`claude_merge::merge` is removed, not deprecated)
- `feedback_line_timing_only.md` — pipeline focus is line-level timing only; `words: None` shipping is normal
- `feedback_no_autosub.md` — autosub stays unregistered as alignment provider; `yt_subs` text candidate is fine
- `feedback_take_ownership.md` — root-cause fix; no band-aids
- `crates/sp-server/src/lyrics/claude_merge.rs:55-133` — current `merge` impl being deleted
- `crates/sp-server/src/lyrics/claude_merge.rs:162-176` — current `source_priority` being rewritten
- `crates/sp-server/src/lyrics/orchestrator.rs:145-204` — current dispatch being refactored
- `crates/sp-server/src/lyrics/description_merge.rs` — being renamed to `text_reference_merge.rs`
