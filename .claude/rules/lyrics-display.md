---
paths:
  - "crates/sp-server/src/lyrics/display_plan*.rs"
  - "crates/sp-server/src/lyrics/renderer.rs"
  - "crates/sp-server/src/playback/position_update.rs"
  - "crates/sp-server/src/playback/recovery.rs"
  - "crates/sp-server/tests/fixtures/lyrics_*.json"
  - "crates/sp-server/src/dabing/subtitles*.rs"
---

# LED-wall lyrics display plan — hold, lead, merge, min visibility (#217)

The stored lyrics lines are **sung-timing units**. The base tier
(`gemini-3-5-transcribe`) splits verses into short, mid-sentence fragments. The
wall used to show those lines one by one with `track.line_at()`, which returns
`None` in every gap, and `dispatch_lyrics_if_changed` turned that `None` into
`HideSubtitles`. On "What A God" (`6KuPjo1diLg`) the owner saw text blink and
vanish: 310.7 s of the 633 s song was blank, and 79 fragments were shown for
under 1.5 s.

The fix is **display-only**. `lyrics/display_plan.rs` builds a plan once per
loaded track (in `LyricsState::with_lead_and_offset`; `new` delegates to it).
The Resolume wall (`resolume_lines_with_next`, also used by
`playback/recovery.rs`) and the Presenter (`presenter_lines`) read
`plan.at(position)`. The dashboard karaoke (`LyricsState::update`) and
`resolume_lines` stay on the RAW track, keeping its word timing.

**Never bump `LYRICS_PIPELINE_VERSION` for a display change.** The stored JSON
and the pipeline are untouched. Changing the grouping in the pipeline instead is
owner-gated (it forces a bump and a catalogue reprocess), and it would not fix
songs that are already stored.

## The constants (one place: `display_plan.rs`)

| Const | Value | Meaning |
|---|---|---|
| `LEAD_MS` | 1500 | A line appears up to this long before it is sung, so people can pre-read. |
| `MIN_VISIBLE_MS` | 1200 | The shortest time a line stays on the wall. |
| `LONG_GAP_MS` | 8000 | A sung gap longer than this is an instrumental break: the wall blanks. |
| `HOLD_TAIL_MS` | 3000 | Before a break, and after the last line, the line leaves this long after its end. |
| `FRAGMENT_MAX_MS` | 1500 | A line sung for less than this is a fragment. |
| `FRAGMENT_MAX_WORDS` | 3 | A line with at most this many words is a fragment. |
| `MERGE_MAX_GAP_MS` | 700 | A fragment merges only across a gap of at most this. |
| `MERGE_MAX_CHARS` | 64 | A merge must keep the joined EN AND the joined SK each within this many chars. |

`display_plan_tests::constants_match_the_design_record` pins every value. Change
one only together with the design record on #217.

## The algorithm

1. **Merge, to a fixpoint.** A fragment is a line that is shorter than
   `FRAGMENT_MAX_MS`, has ≤ 3 words, or is followed by a line whose first
   letter or digit is lowercase (a continuation; leading punctuation like
   `'cause` / `…and` is skipped). A fragment joins its FOLLOWING line when they
   `fits` (the gap is ≤ 700 ms, where overlap counts as 0, and the joined EN
   and the joined SK are each ≤ 64 chars; a missing SK counts as empty).
   Otherwise it joins its previous line under the same limits.
   EN and SK are joined with one space: texts are trimmed, an empty side is
   dropped, and a lone SK is kept. `next_merge` rescans from the start after
   every merge. Each merge removes a group, so the loop always ends.
2. **Lead.** `show = min(start, max(start − LEAD, prev.show + MIN_VISIBLE))`.
   A line never shows after it is sung. When the previous lines leave no
   room, it shows exactly when it is sung.
3. **Hold.** `hide = next.show` when the sung gap to the next line is
   ≤ `LONG_GAP_MS`. Before a longer gap, and after the last line, it is
   `end + HOLD_TAIL`.
4. **Min visibility.** When no text fragment can merge any more, a line that
   the schedule leaves up for less than 1200 ms is merged again under the same
   limits. If no merge fits, it is shown exactly.

**Operator lead / per-song offset still apply on top:** the wall lookup is
`plan.at(position + lyrics_lead_ms − lyrics_time_offset_ms)`.

## Gotchas

- **Step 4 fires only for OVERLAPPING source lines.** With sorted,
  non-overlapping lines, a line that the schedule leaves up for less than
  1200 ms is sung for less than 1200 ms, so step 1 already treated it as a
  fragment and tried the same two merges. The step-4 tests therefore use
  overlapping lines (`a_line_left_under_1200_ms_on_the_wall_merges_when_it_fits`
  and `…exactly_1200_ms…_stays_alone`). Do not delete step 4 as "dead". The
  plan must not assume that every tier's lines are sanitized to be
  non-overlapping: the base tier runs `g35t_transcript::sanitize_lines`, but
  `orchestrator::run_reference_stage` passes the ★ tier's mtl line times
  through unchanged.
- **Dub subtitle tracks go through the same plan.** `gemini-live-translate`
  tracks (#182/#184) are stored as the same `{youtube_id}_lyrics.json`
  (`dabing/subtitles_store.rs`) and load into the same `LyricsState`.
  - Their lines touch, because `finalize_line_ends` trims each end to the
    next start. Many have `en: ""`, and each holds a whole SK sentence of up
    to 20 words.
  - An empty EN is 0 words, so it counts as a fragment, and `"" + ""` always
    fits the EN limit. Before the SK limit (review round 2, 🔴) every EN-less
    run collapsed into one Slovak block. The model gave 10 lines → 1 line of
    679 chars.
  - `a_dub_subtitle_track_never_collapses_into_one_giant_sk_line` pins this.
  - Whether dub tracks should get the lead and hold at all is an open design
    question on #217.
- **The operator `lyrics_lead_ms` ADDS to the plan's 1.5 s lead.** No
  migration seeds it, so it is 0 unless an operator set it. After a deploy,
  read `lead_ms` from the `lyrics: loaded … display_lines=…` log line. If it
  is not 0, the total lead is over 1.5 s.
- **The lead can replace a line while it is still sung, and sometimes even
  before it is sung.**
  - It leaves before its sung end whenever the next line's show falls before
    that end. Usually that is while it is still sung; sometimes it is even
    before it is sung (next point).
  - It leaves at or before its sung START when the next line is sung at most
    `LEAD_MS` after this start and this line had at least `MIN_VISIBLE_MS`
    of lead.
  - This follows the design as written (record 5853195402): a line shows at
    `start − 1500` as long as the previous line got its 1200 ms, and nothing
    waits for the previous line's sung end.
  - On the fixture every line gets the full 1500 ms lead. But 68 of 137 wall
    lines leave before their sung end, 41.7 s in total.
  - "All I have" (sung at 509.6 s) is on the wall only 508.1–509.5 s.
  - The Resolume SK clip shows only the current line, so the Slovak of the
    phrase being sung is what vanishes.
  - A `Design-question:` on #217 asks the main whether to floor the lead at
    the previous line's sung END, the issue-body acceptance "never before the
    previous line's end". Check #217 before changing this.
- **`at()` is a linear first-match over half-open `[show, hide)`.** At an
  exact boundary the NEXT line wins. The tests pin both sides of every boundary
  (`x − 1` → old line, `x` → new line) for the mutation gate.
- **`sung_end` of a merged line is the max of its members' ends.** The
  long-break tail is anchored on that.
- **The fixture is the real song** (`crates/sp-server/tests/fixtures/
  lyrics_6KuPjo1diLg.json`, loaded via `include_str!` + `CARGO_MANIFEST_DIR`).
  Its pins are exact: 137 wall lines, 17 500 ms blank (the three breaks only),
  a shortest line of 1400 ms, and the full 1500 ms lead everywhere. When the
  algorithm changes on purpose, re-derive them with a reference model that
  mirrors `build_plan` step by step. The Tier-0 box cannot run the tests.
