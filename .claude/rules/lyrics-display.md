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
  - "crates/sp-server/src/presenter/**"
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
and clears. Details: `.claude/rules/program-transition.md`, "A held
playlist has no side effects". #221 (release 0.69.0 review 🟡 2): only the
playlist that OWNS the wall (`program-bus.md` "One wall owner": SP-program's
playlist; #221 B4 step 6 deleted the fallback to the one cg OBS was told to
show) writes the wall lines, their clears and the Presenter; another
playlist (the outgoing one of a cut until its OFF, or one played off
program by hand) keeps only its own karaoke WS. The new owner's ON, and an
OFF that leaves another playlist on program, push the owner's line to the
Presenter at once, or (#281) its song's title with no line when it has
none, or clear it when it plays nothing (`scene_off::resync_presenter`,
review rounds 2-3), so the old owner's last line never stays on the stage
display. With NO owner (nothing on air, or
"OBS manuál" on program) nobody writes them — a song played off program by
hand feeds no Presenter — and the last owner's OFF blanks the stage display
with the wall (ROZHODNUTÉ 6002459249, #221 lane 2).

The plan is **display-only**. `lyrics/display_plan.rs` builds it once per
loaded track (in `LyricsState::with_lead_and_offset`; `new` delegates to it).

- The Resolume wall (`resolume_lines_with_next`, also used by
  `playback/recovery.rs`) and the Presenter (`presenter_lines`) read
  `plan.at(position)`.
- **#222: the Presenter push carries BOTH languages.** `presenter_lines`
  returns `PresenterLines {current_en, next_en, current_sk, next_sk}`, each
  EN/SK pair from the SAME plan line (SK punctuation-stripped like the EN,
  "" with no translation). `presenter::payload_for` sends them as
  `currentText`/`nextText` + `currentTranslation`/`nextTranslation` (all
  through `wrap_for_presenter`) and dedups on the WHOLE payload (current +
  next, EN + SK) and, since #281, the song's title (`currentSong`) —
  `PlaylistPipeline.last_presenter_text` holds that `PushedLine {lines,
  song}`, so a new song is pushed even where its lines equal the last
  song's (both blank). So a Slovak line arriving
  later under the same English one is pushed, and so is a sentence sung
  several times in a row whose NEXT line changed ("What a God, what a God."
  ×4: the 4th repeat carries the line after the repeats; release 0.69.0
  review, `a_repeated_line_is_pushed_again_when_its_next_line_changes`). A
  dedup on the current line alone left the stage display's "next" stale.
  **#283: where the wall is blank the stage display's LINES go blank too**
  (`presenter::sync_line`, every position update): before the first line,
  in a break's blank stretch and after the last line's hide, ONE push of
  empty lines, kept as the pushed key, so the next line is pushed again.
  Before, a `None` from `presenter_lines` pushed nothing and the last line
  stayed ("No tak." 45 s past its hide, SNV 18.10.2026). **#281: the song's
  title stays** in that push (`currentSong`), and a song with NO lyrics
  sends its title alone from its position updates; the title goes only at
  the song's end (`clear_lyrics`, `push_empty`), at an OFF with no owner,
  and for an owner that plays nothing
  (`tests_wall_owner.rs::the_stage_display_s_line_goes_with_the_wall_s_and_the_title_stays`,
  `a_song_with_no_lyrics_sends_its_title_to_the_stage_display`). Presenter
  already maps `currentSong` to its snapshot's `songName`; whether its
  translation text mode draws it is Presenter's call (presenter#867).
  **The title is the song's only once it STARTED and while it is not
  paused** (`PlaylistPipeline::stage_title`, review round 1): between a
  Play and its `Started` the last song's `cached_song` is still set, and a
  Position queued before a pause would bring the paused song's title back.
  **Every Play forgets the dedup key** (`begin_play`): after the song-end
  clear, a song played again (Loop, the same song picked again) is pushed
  again. **Every push goes through `PresenterClient::send`**, never a
  `tokio::spawn(client.push(..))`: one task per client sends the NEWEST
  payload, one push at a time (spawned pushes could arrive the other way
  round, a clear landing after the title sent after it), and a failed push
  again after 1 s doubling to 60 s (`retry_wait`) until it goes or a newer
  one replaces it — a song with no lyrics pushes its title once, and the
  engine's dedup counts a failed push as sent.
  Presenter (its issue 799) picks per stage layout what to show; SongPlayer
  has no language setting for it. Read back on the box with Presenter's
  `GET /stage/snapshot?layout=api` (`current.main` = the EN,
  `current.translation` = the SK; `PUT /api/stage` has no GET).
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

**#280: parenthesised asides are left out** (`display_plan_asides.rs`, a
child of `display_plan.rs`). Genius writes ad-libs, backing vocals and the
leader's calls in parentheses. Song 361 "Redeemed" had wall lines of up to
84 chars, and the owner disliked them (16.10.2026).

- `build_plan` reads every source line through `without_asides` BEFORE
  grouping, EN and SK alike. So `fits`, the sentence end
  (`free? (Yeah)` ends in `?`) and the shown text all see the shorter line.
- What `drop_asides` does:
  - every balanced `( … )` goes, nested ones included;
  - the words join again with one space, and a lone `, . ; : ! ? …` is
    glued to the word before it (dropped when it would come first);
  - a line that is ONLY asides keeps its words without the brackets, so the
    wall never blanks a line the room sings (`(Hallelujah)`); one whose
    words are only interjections (`(Oh)`) then leaves by the rule below.
    So does a line whose words OUTSIDE its asides are only interjections:
    it is read with only its brackets gone (`asides::unbracketed`, part 2
    review round 1): `Oh (Hallelujah)` → `Oh Hallelujah`, `Yeah, (I'm
    free)` → `I'm free`, `Oh (yeah)` leaves;
  - a line with no parenthesis, or with parentheses that do not pair up, is
    returned untouched.
- The fixtures hold no `(`.
- The karaoke view keeps the raw text.

**#280 part 2: bare interjections are left out**
(`display_plan_interjections.rs`, a child of `display_plan.rs`, after the
asides; design 6224751018 + the edge rule 6223162067). The owner saw "One
more time we say" + "Oh." as "Ešte raz hovoríme Och." (16.10.2026); on SNV
1353 EN lines (4.1 %) of 349 songs were only interjections.

- `build_plan` takes `shown_lines(lines, profile)`: each line without its
  asides (or with only its brackets gone, above), then through
  `interjections::shown`, each kept line with its index in the track. It
  groups and schedules the kept lines, then maps each group back:
  `src_range = kept[first]..kept[last] + 1`. A left-out line inside a group
  is in its range; one between two display lines is in none. No production
  code reads `src_range`.
- **Sung lyrics only (`Song`).** A dub line (`Speech`) is read only without
  its asides (review round 2): its EN is the whole sentences matched to it
  by time (#184 H5, `dabing.md`), not its translation, so a "Yeah." there
  must not take the dub's own SK subtitle off the wall
  (`a_dub_line_keeps_its_words_whatever_its_english`).
- **The EN line is the judge.** The EN set: `oh ooh oooh oh-oh whoa woah woo
  yeah yea hey ah aah uh mm mmm hmm na la ay ayy eh`, compared lower-case
  with the marks around a word stripped. `yes` is NOT in it: a sung "yes" is
  content.
- **A line whose EN words are ALL interjections leaves the plan**, both
  languages, with its time. The previous display line holds through it by
  the hold rules (until the next line shows; a gap now over 8 s blanks the
  wall). A line with NO EN words is never left out.
- **Its sentence end goes back to the line before it**
  (`interjections::hand_back`, review round 1): the last line kept takes
  the left-out line's final `. ! ? …`, EN and SK each, unless it ends a
  sentence itself or is empty, by the same `ended` as an end run inside a
  line. Without it "One more time we say" + "Oh." + "I still believe it."
  became ONE wall line lit 5 s before its second sentence was sung (the
  #217 failure). A carried EN end closes the sentence, so it ends a
  display line, and the wall and the Presenter strip that final mark; a SK
  end carried while the EN group goes on (EN `Oh,` / SK `Ó.`) shows
  mid-line, as the raw SK had it. 221's line 8 `for you. Let's go for the`
  ends in the `.` of its left-out line 9. Only a sentence end goes back: a
  soft `, ; :` would put commas on the kept words mid-line, and it only
  moves where an over-long sentence splits (review round 2, declined).
- **Edges go only where a mark (`, . ; : ! ? …`, the asides' `GLUED`)
  separates them.**
  - The LONGEST leading run of set words whose last word carries a mark:
    `Yeah, let's dance.` → `Let's dance.`, `Oh, oh Lord, you are good` →
    `Oh Lord, you are good`.
  - The LONGEST trailing run of set words whose PRECEDING word ends in a
    mark: `Say whoa oh. Yeah.` → `Say whoa oh.` (the whole set run `whoa oh.
    Yeah.` is not separated from `Say`, the shorter one is).
  - At least two words must stay, else the line stays whole (`Oh, God.`).
  - Not cut: `Say whoa oh.`, `Oh Lord, you're beautiful` (no mark), and an
    interjection in the middle (`clean, yeah, I'm free indeed`).
  - The first word left takes a capital when the line's first word had one.
    A capital raises the first letter OR DIGIT of a word (past quotes and
    marks): a word that starts with a digit is left as it is (`Oh, 3rd day`
    → `3rd day`, never `3Rd`; review round 4).
  - **The last word keeps its own sentence end, else takes the line's
    final sentence end** (`free indeed, yeah.` → `free indeed.`, `like you.
    Oh,` → `like you.`, `Is he worthy? Yeah.` → `Is he worthy?`). The
    design's literal "the run's mark replaces the kept one" would make
    `like you,` of the last; its own example and reason ("a sentence end
    survives") rule it. A cut can therefore END a sentence the raw line did
    not (221's `like you. Oh,`).
- **SK follows only an EN cut:** at most as many SK words at that edge, each
  from `och ó o oh jé jej hej áno jeah yeah ach ah uh mm hm wow ou`, by the
  same separation rule, down to ONE word (`MIN_KEPT_SK`, review round 2:
  the EN cut already showed that edge to be separate, and the wall shows
  the SK, so `Ó, slobodní` under `Oh, we are free` → `Slobodní`). So `áno` /
  `o` never go on their own (`Yes, Lord` / `Áno, Pane` stays), and a line
  with no SK changes only EN.
- **A left-out line that BEGAN a sentence hands its capital on**
  (follow-up 1, supervisor): `shown_lines` tracks whether the next line
  begins a sentence (the first line does; then each line by its own
  sentence end, `ends_sentence`, a kept line read AFTER its edge cut:
  `like you. Oh,` ends in `.`). A left-out line that begins one gives
  the next line kept its EN and SK capitals, each language by its own
  (`interjections::capitals` / `with_capitals`), unless it ends its
  sentence itself (`Oh.`). What A God's `what a God, what a God.` and
  `more than the silver and gold to you.` now start with a capital; a
  line left out inside a sentence hands none, so `We sing` + `Oh,` +
  `what a God.` stays `We sing what a God.`, as a wall line that starts
  mid-sentence does (`find nobody no.`).
- **A lone mark is glued to the word before it** (follow-up 2): `shown`
  reads a line's words through the asides' `join_words`, so a token made
  only of `, . ; : ! ? …` never stops a run (`Oh , yeah, I'm free` →
  `I'm free`, `Oh , oh` leaves). A dash is not a mark: `Oh - yeah` stays
  whole. A line with nothing to cut is returned as it is.
- **Kept as it is (noted):** a line with unpaired parentheses keeps them
  (`We sing (oh,`). Compound chants (`whoa-oh-oh`, `uh-huh`) are not in the
  decided set.
- Display-only: NO `LYRICS_PIPELINE_VERSION` bump; the karaoke view keeps
  the raw track.
- Tests: `display_plan_interjections_tests.rs` (the census lines, the
  hand-back, the capital and the lone mark, through `build_plan`), `display_plan_fixture_tests.rs`
  `fixture_lines_of_only_interjections_are_in_no_wall_line` and
  `fixture_interjections_at_a_line_edge_go_where_a_mark_separates_them`.
  The fixture oracles read the shown lines through `shown_lines` (`Shown`).

**Never bump `LYRICS_PIPELINE_VERSION` for a display change.** The stored JSON
and the pipeline are untouched. Regrouping lines in the pipeline instead
(g35t grouping at sentence ends) is owner-gated. It forces a bump and a
catalogue reprocess, and it would not fix songs that are already stored.

## The constants (one place: `display_plan.rs`)

| Const | Value | Meaning |
|---|---|---|
| `LEAD_MAX_MS` | 800 | A `Song` line appears at most this long before it is sung (`Speech`: 0). |
| `SUSTAIN_MARGIN_MS` | 1500 | …and only this long after the previous display line's last sung line ends (a real pause). |
| `MIN_VISIBLE_MS` | 1200 | A `Song` line stays up at least this long; the next line waits for it, up to `MAX_LATE_MS`. |
| `MAX_LATE_MS` | 400 | `MIN_VISIBLE_MS − LEAD_MAX_MS`: the most the floor may hold a `Song` line back past its sung start (ROZHODNUTÉ, Design-question 5868750224). |
| `LONG_GAP_MS` | 8000 | A sung gap longer than this is an instrumental break: the display line closes and the wall blanks. |
| `HOLD_TAIL_MS` | 3000 | Before a break, and after the last line, the line leaves this long after its end. |
| `MAX_CHARS` | 52 | ONE wall line: the most chars a merged display line's text AND its Slovak may have (a single longer source line is shown whole). The Presenter wraps at it too (`PRESENTER_WRAP_WIDTH`). |
| `GROUP_MAX_SPAN_MS` | 6500 | A display line's last source line starts at most this long after its first. |

`display_plan_tests::constants_match_the_design_record` pins every value. Change
one only together with the design record on #217.

## The algorithm

1. **Sentences** (`group_lines`, `close_to_fit`).
   - Consecutive source lines form one display line until a line **ends a
     sentence**: its text ends in `. ! ? …`, read past closing quotes and
     brackets (`" ' ” ’ “ ‘ » « ) ]`; `“`, `‘` and `«` close the Slovak
     „…“, ‚…‘ and »…«).
   - A line's text is its EN, or its SK when the EN is empty (dub lines).
     `fits` checks the joined text AND the joined SK against `MAX_CHARS`
     (`display_plan.rs::fits`), so a Slovak translation longer than one wall
     line does split a sentence the EN alone would keep whole (the 29.9.2026
     ruling below: one wall line in BOTH languages).
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
   min(prev_show + MIN_VISIBLE, start + MAX_LATE))`. The first line gets
   `start − LEAD_MAX` (clamped at 0).
   - A line that follows the previous sentence without a pause shows
     exactly when it is sung.
   - The floor comes LAST, so a line sung less than 1200 ms after the
     previous one shows a little late (the design record's order; its
     acceptance "no display under 1.2 s on the fixtures" needs it). It is
     bounded: never more than `MAX_LATE_MS` (400 ms) past the sung start,
     so a fast run of short sentences is shown for under 1200 ms each
     instead of drifting behind the singing.
   - For `Speech`: `show = start`.
3. **Hold.** `hide = next.show` when the sung gap to the next display line is
   ≤ `LONG_GAP_MS`. Before a longer gap, and after the last line, it is
   `sung_end + HOLD_TAIL`.

**Operator lead / per-song offset still apply on top:** the wall lookup is
`plan.at(position + lyrics_lead_ms − lyrics_time_offset_ms)`.

## Gotchas

- **The MIN_VISIBLE floor is bounded by `MAX_LATE_MS` (ROZHODNUTÉ on
  Design-question 5868750224).** Unbounded, it drifted: each sentence
  starting under 1200 ms after the previous one added `1200 − Δstart` ms of
  delay. A chant of eight "Hey!" lines 400 ms apart put the next sentence
  4.8 s late, and before a break over 8 s the last "Hey!" got
  `show > hide` and was never shown.
  - Now every `Song` line shows at most 400 ms after its sung start, and
    `hide > show` whenever the display lines' sung starts strictly
    increase. (Display lines starting at the SAME ms — only possible with
    unsanitized overlapping ★-tier mtl times — can leave the middle one at
    zero length.) The chant's lines are up for 400–1200 ms each,
    and the sentence after it shows 400 ms late
    (`a_chant_of_short_sentences_falls_at_most_400_ms_behind`; it chants
    "Sing!" since #280 part 2, as a "Hey!" line now leaves the plan).
  - Four sentences 1 s apart still show −800, 0, +200 and +400 ms
    (`in_a_fast_run_of_short_sentences_each_keeps_1200_ms`).
  - Every fixture pin is unchanged. The worst delay on the fixtures is
    100 ms: What A God's doubled 0.3 s "What a God, what a God.".
  - The cap-first order never lags, but gives What A God an 1100 ms display.
  - Changing the order or the bound is a design-record change on #217.
- **A merge is bounded by ONE wall line in BOTH languages (29.9.2026,
  supersedes the "SK is not split at 72" ruling of Design-question
  5868750224).** The wall shows the Slovak `#sp-subssk` clip (Line Width
  2400 of the 2580 px composition). Measured on the box, ~39 px a char, so
  ~60 chars fit; `MAX_CHARS` = 52 leaves room for wide glyphs and ` ★`. At
  72 (EN only) song 270 "Thankful" merged its unpunctuated lines into
  58–79-char wall lines that wrapped onto two lines on the wall and on the
  Presenter (owner: "extrémne dlhé texty, pretečie do dvoch riadkov").
  - `fits` checks the joined text and the joined Slovak.
  - A sentence over 52 chars splits at its last soft end, else before the
    line that overflows. What A God's 56- and 53-char sentences now split
    (fixture pins).
  - **A single SOURCE line over 52 is still shown whole** (e.g. song 270's
    `yt_subs+mtl` text keeps two sung lines per YouTube caption, 55–71
    chars). It cannot be split without word timings, and those are never
    synthesized. That is a lyrics-text question, not the display plan's.
  - The Presenter wraps at the same width
    (`PRESENTER_WRAP_WIDTH = MAX_CHARS`), so a wall line is one stage
    line. `wrap_to(text, max)` is the algorithm, and its tests run at
    width 30.
- **Test tracks need sentence marks.** Unpunctuated lines within 6.5 s and 52
  chars (EN and SK) are ONE sentence now, so a test that wants two wall lines must end
  each line in `.`. Examples: `renderer.rs` `wall_track` / `two_line_track`,
  `dispatch_lyrics_tests.rs` `make_track`, `tests_hold.rs` `track`. The wall
  strips the period (`strip_display_punctuation`), but the karaoke WS sends
  the raw text (`"alpha."`). And since #280 part 2 a `Song` test line made
  only of interjections (`Hey!`, `Oh.`, `Yeah.`) leaves the plan: a chant
  test needs a content word (`a_chant_of_short_sentences_falls_at_most_400_ms_behind`
  chants `Sing!`).
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
  - `lyrics_6KuPjo1diLg.json` (What A God, 191 lines): 135 wall lines,
    24 900 ms blank (4 breaks), shortest up 1200 ms, 41 full leads / 77
    exact / 1 late. Lines 11 / 61 / 127 / 187 (`Oh,` `Yeah.` `Oh.` `Oh,`)
    are left out (#280); without line 61 a 9.1 s gap blanks the wall from
    222.1 s to 227.4 s.
  - `lyrics_335.json` (105 lines): 68 wall lines, 28 400 ms blank (2
    breaks), shortest up 1682 ms, 13 / 53 / 0; "Now his will be done." and
    "Lift up your banners and practice your praise." are whole.
  - `lyrics_221.json` (75 lines): 51 wall lines, 40 300 ms blank (5
    breaks), shortest up 1253 ms, 8 / 39 / 0. Lines 9 / 74 (`Yeah. Yeah.`,
    `Hey.`) are left out (#280).
  - Before #280 part 2 the pins were 137 / 68 / 53 wall lines, (3, 19 600)
    / (2, 28 400) / (4, 31 700) and 41/79/1, 13/53/0, 9/40/0.
  - The pins: `fixture_no_wall_line_is_up_for_less_than_1200_ms` (wall
    lines, shortest), `fixture_lead_counts` (full / exact / late),
    `fixture_the_wall_blanks_only_in_instrumental_breaks` (breaks, blank).
  - The fixture tests (`display_plan_fixture_tests.rs`) check on all three:
    no line runs past a sentence end, every split inside a sentence is
    forced by the char/span/gap rule (their own oracle), every lead is
    ≤ 800 ms and after a ≥ 1500 ms pause, and no line is up under 1200 ms.

  When the algorithm changes on purpose, re-derive the pins with a reference
  model that mirrors `build_plan` step by step.
- **The design record's counts (56/49/122 display lines on 335/221/What A
  God) are not the pins.** They were simulated at the old 72-char width, and
  that simulation skipped the char check for a line that ends a sentence (so
  such a line could push its display line past 72 chars); at 72 the code gave
  59/51/123 (review round 2). Since `MAX_CHARS` = 52 in BOTH languages
  (29.9.2026) the pins were 68/53/137, and since #280 part 2 (lines of only
  interjections leave the plan) they are 68/51/135.
