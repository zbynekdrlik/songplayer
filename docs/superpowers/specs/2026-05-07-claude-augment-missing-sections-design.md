# Claude-Augment Missing Sections — Design Spec

**Date:** 2026-05-07
**Status:** Draft
**Working dir:** `/home/newlevel/devel/songplayer`
**Branch:** `dev` (`0.32.0-dev.1`)

---

## Problem

Wall-verify of `id=21` "Good Shepherd" (Chroma Worship) on 2026-05-07 found ~30 % of the song's sung audio has no lyric line displayed:

| Time | Duration | Sung text not in description |
|---|---|---|
| 1:42–1:45 | 3 s | "in your arms" |
| 3:17–3:38 | **21 s** | "in your arms / there's no place I'd rather be / no one like the king" |
| 3:54–4:06 | 12 s | "all my days I will stay in the house" (extra outro repeat) |
| 6:04–7:07 | **63 s** | "and you are my rock in times of trouble / you lift me up / when I fall down / all through the storm / your love is the anchor / my hope is in you alone" |

The YouTube description text we extract has 26 lines covering the main verses + chorus but omits the bridge ("there's no place I'd rather be") and the outro ("you are my rock"). Genius and LRCLib have no record of this song. Spotify has no track ID resolved. Description is the only available text source — and it is incomplete.

WhisperX correctly transcribes those 102 s of sung audio (102/287 = 35 % of all detected words live in the unmatched runs). Phase 1 LCS line-mapping has no description lines to anchor those words against, so they go unmatched. Phase 2 chorus-repeat re-emits **existing** description lines, not new ones. The pipeline ends with audio-coverage gaps.

For a karaoke wall, 30 % gap is unusable.

## Goals

1. Recover description-omitted lyric sections by asking Claude to fill them in from the WhisperX raw transcription, **inside the existing Phase 1 Claude call** — no second Claude round-trip per song.
2. Produce reference text that is grammatically clean (capitalised, punctuated, mishearings corrected against context) — not raw WhisperX output with "shed" / "theres" / lowercase artefacts.
3. Preserve the natural-line-break invariant — added lines must be ≤ 32 chars where possible, otherwise Phase 3 will split them like any other description line.
4. Bound Claude's freedom: only ADD lines for **runs of unmatched ASR words ≥ 5 s** ("missing-section" candidates). Phase 1 must NOT alter, reorder, or drop existing description lines.
5. Single PR, no `LYRICS_PIPELINE_VERSION` bump, no DB schema change.

## Non-goals

- Not building a general "description completer" — only sections WhisperX actually transcribes.
- Not changing Phase 2 / 2.5 / 2.6 / 2.7 / 3 / 4 / 5 logic. Those run unchanged on the augmented description.
- Not changing the orchestrator's source-priority routing. Description still wins; this just expands what description means for that song.
- Not adding a second Claude call. Augmentation rides on the existing Phase 1 prompt.
- Not auto-correcting WhisperX mishearings outside missing-section runs (e.g. "shed" → "shadow" inside an existing matched section). Existing Phase 4 LCS handles those via reference-text override.

## Architecture

### Augmented Phase 1 prompt

Phase 1 currently sends Claude:
- `ref_lines`: description's lines (Vec<String>)
- `asr_words`: WhisperX's flattened word list with timings

And expects back:
- `mapping`: Vec<{asr_word_idx, ref_line_idx}> — which ASR word belongs to which reference line.

Extended Phase 1 sends Claude the same inputs and expects back:

```json
{
  "mapping": [
    {"asr_word_idx": 0, "ref_line_idx": 0},
    {"asr_word_idx": 1, "ref_line_idx": 0},
    ...
  ],
  "added_ref_lines": [
    {"after_line": 12, "text": "There's no place I'd rather be"},
    {"after_line": 12, "text": "No one like the king"},
    {"after_line": 25, "text": "And you are my rock in times of trouble"},
    ...
  ]
}
```

Where:
- `mapping` retains existing semantics (every entry references the ORIGINAL `ref_lines` — Claude must NOT re-index after additions).
- `added_ref_lines.after_line` is the index in the ORIGINAL `ref_lines` after which the added line should be inserted. Multiple `added_ref_lines` with the same `after_line` keep their order.
- `added_ref_lines.text` is grammatically clean reference text (capitalised, punctuated). Claude reconciles WhisperX mishearings against song context.

Claude is instructed to add lines ONLY for runs of unmatched ASR words ≥ 5 s. Sections shorter than 5 s stay unmatched (covered by Phase 2 chorus-repeat or Phase 5 extension).

### Reference-list expansion (post-Phase 1)

After Phase 1 returns, the pipeline reconstructs `ref_lines`:

```
expanded_ref_lines = []
for i, line in enumerate(original_ref_lines):
    expanded_ref_lines.append(line)
    for added in added_ref_lines where added.after_line == i (preserving order):
        expanded_ref_lines.append(added.text)
```

The ORIGINAL `mapping` (whose `ref_line_idx` references the ORIGINAL `ref_lines`) is rewritten to point at the expanded list:

```
expanded_mapping[entry.asr_word_idx] = original_index_to_expanded[entry.ref_line_idx]
```

Where `original_index_to_expanded[i]` is `i + count_of_added_lines_before_or_at_i`.

After expansion:
- `Phase 1 emit` is built from `expanded_mapping` over `expanded_ref_lines`. Added lines have `matched_asr_indices = []` initially — they get filled by **Phase 2's** chorus-repeat / sliding-window logic exactly the same way it fills any unmatched reference line.

Wait — Phase 2 chorus-repeat re-emits a reference line for unmatched audio gaps that look like that line repeating. Added lines AREN'T repeats; they're brand-new content. Phase 2 won't pick them up.

**Solution:** Run a Phase 1.5 LCS sub-pass that aligns each added line's words against the unmatched ASR word window between Phase 1's last-emit-before-it and first-emit-after-it. Same `lcs_align` helper already in `text_reference_merge.rs`, just scoped to the added line's audio window.

This Phase 1.5 produces a `LineEmit` for each added line with `asr_word_indices` pointing at the matched WhisperX words in that audio window. Phases 2, 2.5, 2.6, 2.7, 3, 4, 5 then run unchanged — they don't care whether an emit came from Phase 1 (description) or Phase 1.5 (added line).

### Provenance

The output `AlignedTrack.provenance` stays `{best.source}+{asr.provenance}` = `description+whisperx-large-v3@rev1`. No new provenance tag — the augmentation is internal to the description-merge path. Audit JSON gets a new `phase1_added_ref_lines` field for transparency.

## Prompt template (additions to Phase 1 prompt)

The existing Phase 1 prompt asks Claude to emit a JSON mapping. Extend the schema documentation to:

```
Output JSON: {
  "mapping": [{ "asr_word_idx": <int>, "ref_line_idx": <int> }, ...],
  "added_ref_lines": [{ "after_line": <int>, "text": "<string>" }, ...]
}

ADDED LINES — strict rules:
- Add a line ONLY when the audio contains a run of unmatched WhisperX
  words ≥ 5 seconds long that you cannot map to any ref_lines line.
- Each added line's text MUST be grammatically clean (capitalised,
  punctuated, mishearings corrected against song context).
- Each added line MUST be ≤ 64 characters; longer phrasing splits
  into multiple added lines, all sharing the same after_line index.
- after_line is the index in ref_lines AFTER WHICH the added line
  semantically belongs. Use the index of the closest preceding
  description line whose mapped audio ends BEFORE the unmatched run.
- Preserve order: multiple added_ref_lines with the same after_line
  appear in the order they were sung.
- DO NOT add lines for unmatched runs < 5 seconds — those are typically
  filler ("Say", "Oh") or chorus-repeats that Phase 2 will recover.
- DO NOT modify, reorder, or drop existing ref_lines entries — only
  the mapping field assigns audio to them.
- DO NOT add lines that duplicate existing ref_lines text exactly —
  those are chorus-repeats; leave them unmatched and Phase 2 will
  re-emit the existing line.
```

## Phase 1.5 — added-line alignment

New module: `crates/sp-server/src/lyrics/text_reference_merge_added.rs`.

```rust
pub(crate) fn align_added_lines(
    added: &[AddedRefLine],            // from Claude
    expanded_ref_lines: &[String],     // post-expansion
    asr_words: &[AsrWord],
    phase1_emits: &[LineEmit],         // from existing Phase 1 mapping
) -> Vec<LineEmit> {
    // For each added line:
    //   1. Determine the unmatched ASR audio window:
    //        start = last emit-end before this added line's slot, or 0
    //        end   = next emit-start after this added line's slot, or last_asr_word.end
    //   2. LCS-align added line's normalized words against the unmatched
    //      ASR words in that window.
    //   3. Emit a LineEmit { text, asr_word_indices }.
    //   Skip emit if LCS finds zero matched words (Phase 5 will drop micro-windows).
}
```

The function reuses `lcs_align` from `text_reference_merge.rs`. No new alignment logic.

## File structure

```
crates/sp-server/src/lyrics/
├── text_reference_merge.rs              # MODIFY — call mapping + added expansion + 1.5 alignment
├── text_reference_merge_mapping.rs      # MODIFY — new prompt schema, parse added_ref_lines
├── text_reference_merge_added.rs        # NEW   — Phase 1.5 align_added_lines
├── text_reference_merge_added_tests.rs  # NEW   — 5 unit tests
└── text_reference_merge_audit.rs        # MODIFY — record phase1_added_ref_lines
```

## Tests

**Unit tests (new — `text_reference_merge_added_tests.rs`):**

1. `align_added_lines_matches_words_in_unmatched_window` — added line "There's no place I'd rather be" within an unmatched 5 s window where ASR has those words → emit with all matches.
2. `align_added_lines_skips_when_window_empty` — no ASR words in the gap → returns empty Vec.
3. `align_added_lines_respects_after_line_ordering` — two added lines after the same description line → ordered emit using contiguous ASR sub-windows.
4. `align_added_lines_handles_partial_lcs_match` — added line text differs from ASR text (mishearing) → LCS picks at least one matched word, emit kept.
5. `align_added_lines_uses_full_song_tail_when_after_last_line` — added line after the last description line → window = (last-emit.end..last_asr_word.end).

**Mapping prompt parser tests (modify `text_reference_merge_mapping_tests` if exists; else add to existing test mod):**

6. `parse_mapping_response_extracts_added_ref_lines` — Claude returns mapping + added_ref_lines → both parsed.
7. `parse_mapping_response_handles_missing_added_field` — Claude returns only mapping (no added_ref_lines key) → `added_ref_lines = []`. Backward-compatible.
8. `parse_mapping_response_rejects_added_line_after_invalid_index` — `after_line` out of range → drop that added line, log warn, continue.

**Integration test (new in description_merge top-level test mod):**

9. `description_merge_id21_outro_section_emits_added_lines` — fixture with description missing outro + ASR containing 60 s of "and you are my rock…" words → expanded ref_lines includes the outro lines + final emits cover ≥ 80 % of ASR words.

## Failure modes & fallbacks

| Failure | Fallback |
|---|---|
| Claude returns malformed JSON | existing Phase 1 NW-DP fallback runs (no augmentation, behaviour matches today) |
| Claude returns valid `mapping` but invalid `added_ref_lines` | drop `added_ref_lines`, keep `mapping` — pipeline runs as today |
| Claude adds duplicate-text added lines | post-parse dedup: drop adds whose text equals an existing description line (same as Claude's instruction; defensive) |
| Phase 1.5 LCS finds zero matched words for an added line | drop that emit (Phase 5 micro-window threshold also drops it) |
| `after_line` index out of range | drop that added line, log warn |

## Migration

None. Output JSON shape unchanged. Existing songs that wall-verified clean stay clean (description is complete for them — Claude returns empty `added_ref_lines`). Songs whose description was incomplete (id=21 case) get added lines covering the missing sections on next reprocess.

No `LYRICS_PIPELINE_VERSION` bump per project rule. Affected songs reprocessed song-by-song via `manual_priority`.

## Code-size & risk

- ~150 LOC in mapping.rs (prompt addition + parser extension)
- ~100 LOC in new added.rs (align_added_lines)
- ~150 LOC in added_tests.rs
- ~30 LOC in text_reference_merge.rs (orchestration glue)
- ~20 LOC in audit.rs (record added_ref_lines field)
- File-size cap respected; new sibling file keeps the parent under 1000 lines.

## Approval gates

1. Spec approval (this doc).
2. Plan written via writing-plans, user reviews.
3. Implementation via subagent-driven-development.
4. CI green.
5. Wall-verify on sp-live: id=21 plays through, bridge + outro now show lyric lines.
6. id=132 regression check: byte-identical to pre-augment baseline (Holy Forever description is complete — Claude must return empty `added_ref_lines`).

## References

- `crates/sp-server/src/lyrics/text_reference_merge.rs` — Phase 1 dispatch
- `crates/sp-server/src/lyrics/text_reference_merge_mapping.rs::claude_map_words_to_lines` — current Phase 1 Claude call
- `crates/sp-server/src/lyrics/text_reference_merge.rs::lcs_align` — reused for Phase 1.5 alignment
- `feedback_song_by_song_iteration.md` — strict serial reprocess
- `feedback_pipeline_version_approval.md` — no `LYRICS_PIPELINE_VERSION` bump
- `feedback_no_legacy_code.md` — replace cleanly, no aliases
- 2026-05-07 wall-verify of id=21 — bridge + outro NOT in description, no Genius / LRCLib / Spotify match for "Chroma Worship Good Shepherd"
