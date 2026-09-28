---
paths:
  - "crates/sp-server/src/lyrics/display_plan*.rs"
  - "crates/sp-server/src/lyrics/renderer.rs"
  - "crates/sp-server/src/playback/position_update.rs"
  - "crates/sp-server/src/playback/recovery.rs"
  - "crates/sp-server/src/playback/dispatch_lyrics_tests.rs"
  - "crates/sp-server/src/playback/tests_hold.rs"
  - "crates/sp-server/tests/fixtures/lyrics_*.json"
  - "crates/sp-server/src/dabing/subtitles*.rs"
---

# LED-wall lyrics display plan — whole sentences, lead only into a pause, hold (#217)

The stored lyrics lines are **sung-timing units**. The base tier
(`gemini-3-5-transcribe`) splits verses into short punctuated phrases:
"Now his will be", "done.", "Lift up your", "banners and practice",
"your praise.".

- **Blinking (27.9.2026).** Shown one by one with `track.line_at()`, the wall
  blanked in every gap. On "What A God" (`6KuPjo1diLg`), 310.7 s of 633 s was
  blank.
- **Mid-sentence switching (28.9.2026, design record 5867952012).** The first
  fix merged phrases by length (fragment rules, 64 chars) and led by 1.5 s.
  The merges ran across sentence ends ("done. Lift up your banners and
  practice your praise"). The lead was floored at transcript line ends, which
  stop before a held note does. So the wall switched while the singers were
  still mid-sentence, and lit later phrases 3–5 s early. Those rules and
  their constants are **deleted**, not kept as a fallback.

**Who may send a line** (release 0.68.0 blockers, design record 5863318980):
`dispatch_lyrics_if_changed` sends nothing for a playlist HELD off program
through a #215 transition (`scene_off_due` is set), and `clear_lyrics_display`
follows the same gates. The wall (`#sp-subs*`) takes only on-program lines
and clears; the karaoke WS and the Presenter also take a song played off
program by hand. Details: `.claude/rules/program-transition.md`, "A held
playlist has no side effects".

The plan is **display-only**. `lyrics/display_plan.rs` builds it once per
loaded track (in `LyricsState::with_lead_and_offset`; `new` delegates to it).

- The Resolume wall (`resolume_lines_with_next`, also used by
  `playback/recovery.rs`) and the Presenter (`presenter_lines`) read
  `plan.at(position)`.
- The dashboard karaoke (`LyricsState::update`) stays on the RAW track, with
  its word timing.
- The raw `resolume_lines` was deleted (ROZHODNUTÉ 5853953564): its name
  promised the wall but it bypassed the plan.

**Two display profiles, chosen at load from the track's `source`**
(`DisplayProfile::for_source`):

- `Song` is every sung-lyrics track: a lead of up to 800 ms into a pause, and
  the 1200 ms floor (step 2).
- `Speech` is a dub subtitle track: `source ==
  dabing::subtitles::SOURCE_LIVE_TRANSLATE` (`"gemini-live-translate"`). That
  is the same marker the translation worker already excludes dub tracks by.
- `Speech` shows each line exactly when it is spoken (no lead, no floor).
  Grouping, hold and the break tail are the same.
- The `lyrics: loaded` log line carries `display_lines` and
  `display_profile`.

**Never bump `LYRICS_PIPELINE_VERSION` for a display change.** The stored JSON
and the pipeline are untouched. Regrouping lines in the pipeline instead
(g35t grouping at sentence ends) is owner-gated. It forces a bump and a
catalogue reprocess, and it would not fix songs that are already stored.

## The constants (one place: `display_plan.rs`)

| Const | Value | Meaning |
|---|---|---|
| `LEAD_MAX_MS` | 800 | A `Song` line appears at most this long before it is sung (`Speech`: 0). |
| `SUSTAIN_MARGIN_MS` | 1500 | …and only this long after the previous display line's last sung line ends (a real pause). |
| `MIN_VISIBLE_MS` | 1200 | A `Song` line stays up at least this long; the next line waits for it. |
| `LONG_GAP_MS` | 8000 | A sung gap longer than this is an instrumental break: the display line closes and the wall blanks. |
| `HOLD_TAIL_MS` | 3000 | Before a break, and after the last line, the line leaves this long after its end. |
| `MAX_CHARS` | 72 | The most chars one display line's text may have (a single longer source line is shown whole). |
| `GROUP_MAX_SPAN_MS` | 6500 | A display line's last source line starts at most this long after its first. |

`display_plan_tests::constants_match_the_design_record` pins every value. Change
one only together with the design record on #217.

## The algorithm

1. **Sentences** (`group_lines`, `close_to_fit`).
   - Consecutive source lines form one display line until a line **ends a
     sentence**: its text ends in `. ! ? …`, read past closing quotes and
     brackets (`" ' ” ’ » ) ]`).
   - A line's text is its EN, or its SK when the EN is empty (dub lines).
     SK is not checked against `MAX_CHARS`: a translation must not re-split
     a sentence the EN keeps whole.
   - The display line closes EARLIER when the next line would not `fits`:
     the joined text would pass `MAX_CHARS`, or the next line starts over
     `GROUP_MAX_SPAN_MS` after the first. It then closes after its LAST line
     ending in `, ; : —` (a soft end), or whole when it has none. The rest has
     no soft end, so it either takes the next line or closes whole too.
   - Before a gap over `LONG_GAP_MS` (from the open line's sung end) it closes
     whole.
   - A display line spans whole source lines. A sentence end INSIDE one
     source line stays: splitting it would need word timings, and those are
     never synthesized (v18 rule).
2. **Show** (`DisplayProfile::show_ms`). For a `Song`:
   `show = max(min(max(start − LEAD_MAX, prev_end + SUSTAIN_MARGIN), start),
   prev_show + MIN_VISIBLE)`. The first line gets `start − LEAD_MAX`
   (clamped at 0).
   - A line that follows the previous sentence without a pause shows
     exactly when it is sung.
   - The floor comes LAST, so a line sung less than 1200 ms after the
     previous one shows a little late (the design record's order; its
     acceptance "no display under 1.2 s on the fixtures" needs it).
   - For `Speech`: `show = start`.
3. **Hold.** `hide = next.show` when the sung gap to the next display line is
   ≤ `LONG_GAP_MS`. Before a longer gap, and after the last line, it is
   `sung_end + HOLD_TAIL`.

**Operator lead / per-song offset still apply on top:** the wall lookup is
`plan.at(position + lyrics_lead_ms − lyrics_time_offset_ms)`.

## Gotchas

- **The MIN_VISIBLE floor after the cap can fall behind in a fast run of
  separate sentences.** Each sentence sung under 1200 ms after the previous
  one shows `1200 − gap` ms later than the one before, and it adds up. For
  example, 1 s apart → 0, 0, +200, +400 ms (pinned by
  `in_a_fast_run_of_short_sentences_each_keeps_1200_ms`).
  - Not on the three fixtures. The worst is 100 ms: What A God's doubled
    0.3 s "What a God, what a God.", whose text is the same.
  - A chant with a sentence mark on every short line would drift.
  - The cap-first order never lags, but gives What A God an 1100 ms display.
  - Changing the order is a design-record change on #217.
- **Test tracks need sentence marks.** Unpunctuated lines within 6.5 s and 72
  chars are ONE sentence now, so a test that wants two wall lines must end
  each line in `.`. Examples: `renderer.rs` `wall_track` / `two_line_track`,
  `dispatch_lyrics_tests.rs` `make_track`, `tests_hold.rs` `track`. The wall
  strips the period (`strip_display_punctuation`), but the karaoke WS sends
  the raw text (`"alpha."`).
- **Dub subtitle tracks go through the same plan, as `Speech`.**
  `gemini-live-translate` tracks (#182/#184) are stored as the same
  `{youtube_id}_lyrics.json` (`dabing/subtitles_store.rs`, `StoredDubTrack`
  flattens `source` into it) and load into the same `LyricsState`.
  - Their lines touch (`finalize_line_ends` trims each end to the next start)
    and many have `en: ""`, so they group by their SK text.
  - Each holds a whole SK sentence, so each is its own wall line
    (`a_dub_subtitle_track_never_collapses_into_one_giant_sk_line`). An
    unpunctuated EN-less run is still bounded by `MAX_CHARS` on the SK
    (`a_line_with_no_english_is_grouped_by_its_slovak`).
  - The wall shows each dub line exactly when spoken:
    `a_speech_line_shows_exactly_when_it_is_spoken`,
    `a_speech_line_is_never_held_back_for_the_one_before`, and through the
    renderer `a_dub_track_loads_as_speech_and_shows_lines_exactly_when_spoken`.
- **The operator `lyrics_lead_ms` ADDS to the plan's lead.** No migration
  seeds it, so it is 0 unless an operator set it. After a deploy, read
  `lead_ms` from the `lyrics: loaded … display_lines=…` log line. If it is not
  0, the total lead is over the plan's. It also shifts `Speech` tracks.
- **A line never leaves before it is sung to its end** on the fixtures
  (`fixture_no_wall_line_leaves_before_it_is_sung_to_the_end`). The next
  line's show is at least the previous line's sung end whenever its own start
  is. Only OVERLAPPING source lines (★-tier mtl line times are not sanitized,
  `orchestrator::run_reference_stage`) can replace a sung line.
- **`close_to_fit` has no loop on purpose.** The cut is at the LAST soft end,
  so the rest has none, and one more check settles it. A `while` loop there
  would hang under the `k + 1` → `k * 1` mutant (a cut that makes no
  progress), and the mutation gate counts a hang as a failure.
- **`at()` is a linear first-match over half-open `[show, hide)`.** At an
  exact boundary the NEXT line wins. The tests pin both sides of every
  boundary (`x − 1` → old line, `x` → new line) for the mutation gate.
- **The fixtures are real songs**, loaded via `include_str!` +
  `CARGO_MANIFEST_DIR` (`crates/sp-server/tests/fixtures/`):
  - `lyrics_6KuPjo1diLg.json` (What A God, 191 lines): 123 wall lines,
    19 600 ms blank (3 breaks), shortest up 1200 ms, 41 full leads / 65
    exact / 1 late.
  - `lyrics_335.json` (105 lines): 59 wall lines; "Now his will be done." and
    "Lift up your banners and practice your praise." are whole.
  - `lyrics_221.json` (75 lines): 51 wall lines.
  - The fixture tests (`display_plan_fixture_tests.rs`) check on all three:
    no line runs past a sentence end, every split inside a sentence is
    forced by the char/span/gap rule (their own oracle), every lead is
    ≤ 800 ms and after a ≥ 1500 ms pause, and no line is up under 1200 ms.

  When the algorithm changes on purpose, re-derive the pins with a reference
  model that mirrors `build_plan` step by step. The Tier-0 box cannot run the
  tests.
