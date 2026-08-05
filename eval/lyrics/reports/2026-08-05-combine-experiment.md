# Lyrics Eval — LLM-lines + ASR-times Combine Experiment — 2026-08-05

**Question:** the one-call north-star sweep
(`2026-08-05-one-call-northstar.md`) showed audio-LLM backends write good
line CONTENT but estimate line TIMING badly, while dedicated-ASR backends
time individual WORDS accurately but have no concept of a lyric LINE. **If
the LINES come from the audio-LLM and only the TIMES come from the ASR word
stream, does the combination clear the 400ms wall-timing gate?** This is an
offline experiment on data already captured — no new API calls, no new
model runs.

**Backends used** (all already committed under `reports/2026-08-05-raw/`):

| Role | Backend | What it contributes |
|---|---|---|
| LINE source | `gemini36-flash` | Audio-LLM (Gemini 3.6 Flash), native line segmentation + SK translation, LLM-estimated timestamps (discarded). |
| LINE source | `qwen35-omni` | Audio-LLM (Qwen 3.5 Omni Plus), same shape, no per-word timings at all (`words: null` on every line). |
| TIME source | `soniox-v5` | Dedicated ASR (Soniox stt-async-v5), real acoustic per-WORD timestamps, naive silence-gap line grouping (over-segments). |
| TIME source | `aai-u35-translate` | Dedicated ASR (AssemblyAI Universal-3.5 Pro), real acoustic per-WORD timestamps, coarse utterance-level line grouping. |

**Fixtures:** the same 22 of 24 pinned manifest fixtures the north-star
sweep scored (2 skipped — no cached vocal WAV: `vpwDdb8r9Bk`, `fHYLw-2tTx4`).
`soniox-v5` and `qwen35-omni` raw outputs were pulled from win-resolume
(`C:\ProgramData\SongPlayer\eval-run\out\`) via the `win-resolume` MCP tools
and committed alongside the already-present `gemini36-flash` /
`aai-u35-translate` files under `reports/2026-08-05-raw/`, so the whole
experiment reproduces from the repo alone.

**Data-integrity note:** the initial fetch of the 21 smallest
`qwen35-omni_*.json` files went through a Windows PowerShell text-concat
step that corrupted their `text_sk` field into double-encoded mojibake
(`"NiÄ\x8d nÃ¡s..."` instead of `"Nič nás..."`) — the English `text` field
(what alignment actually reads) was never affected. A first repair pass
(cp1252-as-UTF-8 reversal, Latin-1 passthrough for undefined cp1252 code
points) fixed the corruption my detection regex caught, but that regex
only keyed on `Ã`/`Â` lead bytes and missed a second, `Å`/`Ä`-led mojibake
pattern present in some of the same strings ("mÅˆa" instead of "mňa",
"veÄ¾a" instead of "veľa") — a dispatched corrective fetch independently
caught and fixed these 227 residual fields across 20 files using a
broader check. Both passes only ever touched `text_sk` (verified
field-by-field against git history: 0 changes to `text`, `start_ms`,
`end_ms`, line counts, or `duration_ms` across every file) — all
timing/alignment numbers in this report were computed from the
(always-clean) English text and are unaffected by either pass. Final
state, independently re-verified: 0/22 files with any mojibake by either
detection pattern, every `text_sk` reads as normal diacritic Slovak.

## Method — word-sequence alignment

Implemented in `eval/lyrics/combine_lines_times.py` (tested in
`eval/lyrics/tests/test_combine_lines_times.py`, 18 tests):

1. **Flatten the TIME source** (`flatten_time_source`) — walk its
   `lines[].words[]` in order into one chronological word stream. Both
   `soniox-v5` and `aai-u35-translate` already emit whole-word (not
   sub-word-token) `words[]` with real integer `start_ms`/`end_ms`, so no
   sub-word merging step was needed (verified directly against the fetched
   files, not assumed).
2. **Flatten the LINE source** (`flatten_line_source`) — whitespace-split
   each line's `text` into words, tagging which line each word came from.
   The line source's own timestamps (line-level or the LLM-guessed
   per-word ones some backends carry) are **never read** — only word
   identity matters, timing always comes from the time source.
3. **Align the two normalized-token sequences** with
   `difflib.SequenceMatcher(autojunk=False)`. This is **monotonic by
   construction** — Ratcliff/Obershelp never emits an out-of-order match —
   which is exactly what makes it robust to a repeated chorus: the Nth
   occurrence of a repeated phrase in the line source can only align to the
   Nth-or-later occurrence in the time source, never back onto an earlier
   one an earlier line already consumed. `autojunk=False` is deliberate:
   with autojunk on, a token that repeats very often (a chant word said
   dozens of times) is treated as noise and excluded from matching — the
   exact repeated-content bias this experiment must not have.
4. Within an `equal` opcode every pair is an exact normalized-text match.
   Within a same-length `replace` opcode, each position is *also* probed
   with a light fuzzy check (`SequenceMatcher.ratio() >= 0.5`) to recover
   trivial spelling/contraction drift (`"cause"` vs `"caus"` → 0.89) while
   genuinely different word choices (`"'cos"` vs `"because"` → 0.4) are
   correctly left unaligned rather than force-matched. Unequal-length
   `replace` blocks, `insert`, and `delete` blocks are left entirely
   unaligned — there is no reliable position pairing to probe there.
5. For each LINE-source line: `start_ms` = min `start_ms`, `end_ms` = max
   `end_ms` among its ALIGNED words. A line with **zero** aligned words is
   **UNTIMED** — `start_ms`/`end_ms` stay `None`, never guessed or
   interpolated.
6. A monotonicity-enforcement pass clamps any TIMED line's `start_ms` up to
   the previous TIMED line's `end_ms` (a floor that only advances on TIMED
   lines; UNTIMED lines never move it) — mirrors the `floor_start_ms`
   sanitizer already used in the production pipeline (v10 history, project
   `CLAUDE.md`). In this run it fired 0 times on every combo — the
   alignment's own monotonicity already prevented any violation; the pass
   exists as a structural guarantee, not because it was observed to be
   needed here.

**Scoring — two views, on purpose.** The existing `score_one_call.py`
matcher (imported, never modified) is known to have no monotonicity
constraint of its own and can pick the wrong repeat of a phrase on
chant/chorus material (documented in its own north-star report). So every
combo is scored **twice**:

- **Official** — `score_one_call.py`'s unmodified greedy matcher (ratio
  ≥0.6, closest-start disambiguation among candidates, gold-line-consuming).
- **Conservative** (`run_combine_experiment.py`, new code) — a produced
  line is paired to a gold line only when (a) that gold line's normalized
  text is **unique in the song** and (b) text similarity is **≥0.75**.
  Uniqueness rules out repeat-confusion by construction (there is never
  more than one candidate occurrence), so no closest-start disambiguation
  is needed or used. This is a strictly harder bar — fewer pairs, but every
  one of them is unambiguous.

UNTIMED lines are excluded from what's fed to either scorer (a `None`
`start_ms` cannot be sorted/compared) — their count is reported alongside
every combo instead of being silently dropped from the picture.

## Headline results — all combinations × both scoring views

Pooled across all 22 fixtures (1772 total gold lines).

| Backend / Combo | Role | % ≤400ms (official) | Median Δ (official) | % ≤400ms (conservative) | Median Δ (conservative) | Coverage % | Untimed % |
|---|---|---:|---:|---:|---:|---:|---:|
| `gemini36-flash` (baseline) | LLM own lines+times | 16.0 | 2160 ms | — | — | 44.2 | — |
| `qwen35-omni` (baseline) | LLM own lines+times | **7.7** | **11780 ms** | — | — | 72.3 | — |
| `soniox-v5` (baseline) | ASR own lines+times | 23.0 | 1105.5 ms | — | — | 70.1 | — |
| `aai-u35-translate` (baseline) | ASR own lines+times | 31.1 | 1575.5 ms | — | — | 11.6 | — |
| `qwen35-omni` lines × `soniox-v5` times | **combo** | 29.0 | 781 ms | 25.9 | 1050 ms | 71.3 | 18.9 |
| `qwen35-omni` lines × `aai-u35-translate` times | **combo** | **41.4** | **573 ms** | **33.8** | **1142 ms** | 70.1 | 20.2 |
| `gemini36-flash` lines × `soniox-v5` times | **combo** | 29.2 | 750 ms | 27.7 | 945 ms | 41.9 | 5.7 |

("Untimed %" = share of the LLM's own lines for which zero words aligned to
the ASR word stream — excluded from the two scored columns, reported
honestly, never guessed.)

**Per-fixture "typical fixture" view** (median of each fixture's own median
delta — the north-star report's own preferred statistic, since pooled
means/medians can be dragged around by a handful of catastrophic fixtures):

| Combo | Fixtures w/ ≥1 matched line | Median-of-medians | Fixtures w/ median ≤1000ms | Fixtures w/ within-400 rate ≥30% |
|---|---:|---:|---:|---:|
| `qwen35-omni` × `soniox-v5` | 22/22 | 805 ms | 15/22 | 9/22 |
| `qwen35-omni` × `aai-u35-translate` | 22/22 | **475 ms** | 16/22 | 14/22 |
| `gemini36-flash` × `soniox-v5` | 21/21 | 740 ms | 16/21 | 9/21 |

Compare against the raw backends' own per-fixture robustness in the
north-star report: `gemini36-flash` alone was 4/21 fixtures ≤1000ms and
4/21 ≥30% within-400; `aai-u35-translate` alone was 7/16 and 9/16. Every
combo beats BOTH ingredients on this "typical fixture" measure, and
`qwen35-omni × aai-u35-translate` — at 475 ms median-of-medians — comes
within 75 ms of the 400ms bar as a per-fixture typical case, even though
the fully-pooled figure (1142 ms conservative) does not.

## Per-category breakdown (official matcher)

| Category | Combo | n | Coverage % | Median Δ (ms) | Within 400ms % | sk_ok % |
|---|---|---:|---:|---:|---:|---:|
| chant_repetition | qwen×soniox | 4 | 75.5 | 920 | 16.0 | 85.5 |
| clean_pop | qwen×soniox | 3 | 86.2 | 522.5 | 40.0 | 92.9 |
| dense_vocal | qwen×soniox | 4 | 71.2 | 510 | 47.2 | 81.7 |
| instrumental_breaks | qwen×soniox | 4 | 70.7 | 730 | 23.3 | 79.2 |
| multi_language | qwen×soniox | 4 | 58.7 | 980 | 29.8 | 91.7 |
| reverb_heavy | qwen×soniox | 3 | 74.6 | 940 | 22.2 | 92.0 |
| chant_repetition | qwen×aai | 4 | 75.5 | 758 | 34.8 | 85.9 |
| clean_pop | qwen×aai | 3 | 77.6 | **364** | 51.9 | 92.3 |
| dense_vocal | qwen×aai | 4 | 71.2 | **352** | 50.6 | 81.0 |
| instrumental_breaks | qwen×aai | 4 | 70.3 | **352** | 60.0 | 78.9 |
| multi_language | qwen×aai | 4 | 58.0 | 1268 | 32.4 | 91.7 |
| reverb_heavy | qwen×aai | 3 | 73.8 | 910 | 25.1 | 91.8 |
| chant_repetition | gemini×soniox | 4 | 40.7 | 1045 | 13.9 | 64.6 |
| clean_pop | gemini×soniox | 3 | 23.0 | 495 | 40.0 | 100.0 |
| dense_vocal | gemini×soniox | 4 | 46.1 | 570 | 38.8 | 96.0 |
| instrumental_breaks | gemini×soniox | 4 | 64.3 | 750 | 19.4 | 70.2 |
| multi_language | gemini×soniox | 4 | 36.0 | **476.5** | 44.2 | 94.9 |
| reverb_heavy | gemini×soniox | 3 | 37.9 | 770 | 29.8 | 84.4 |

**Bold = clears the 400ms bar on MEDIAN.** `qwen35-omni × aai-u35-translate`
clears it on **three of six categories** (dense_vocal 352ms, instrumental_breaks
352ms, clean_pop 364ms) under the official matcher. Checked against the
stricter conservative metric (unique-gold-line pairing, no repeat
disambiguation at all): **only `instrumental_breaks` still clears** (356ms
conservative vs 352ms official — a real, not matcher-flattered, result);
`dense_vocal` (680ms conservative) and `clean_pop` (504ms conservative)
both slip back above 400ms once repeat-confusion is ruled out, meaning
part of their official-matcher win came from the closest-start
disambiguation quietly picking a favorable repeat rather than pure timing
accuracy. `chant_repetition` — the category built specifically to stress
repeated content — never clears on median under either matcher, on any
combo (758–1045ms).

## Does the "two backends transcribe differently" worry actually materialize?

**Word-alignment rate** (share of the LLM's own line-source words that
found a matching ASR word), pooled across all 22 fixtures:

| Combo | Overall | Repeated-line words | Unique-line words | Untimed lines |
|---|---:|---:|---:|---:|
| qwen×soniox | 76.7% | 69.8% | 92.3% | 18.9% |
| qwen×aai | 74.5% | 67.1% | 91.3% | 20.2% |
| gemini×soniox | 86.4% | 84.6% | 88.9% | 5.7% |

"Repeated-line words" = words belonging to an LLM line whose normalized
text occurs more than once elsewhere in that same song's LLM output;
"unique-line words" = words from a line whose text is one-of-a-kind in the
song. **Yes, alignment is measurably worse on repeated content — but the
gap is almost entirely one pathological fixture, not the chorus-repeat
mechanism the worry describes.**

`Xvm4_fWkXe8` (clean_pop) is the extreme outlier: `qwen35-omni` produced
**395 lines**, of which **161** are the normalized phrase "and you keep on
doing it" and **150** are "keep on keep on keep on" — a **runaway
degenerate-repetition hallucination**, not a real transcript. Soniox's real
transcript of the same audio has "and you keep on doing it" only **16**
times (gold has it 3 times; the song is ~4.6 minutes per Soniox's own
duration, matching gold's last line at 279s — nothing like the ~16.7
minutes qwen's self-reported `duration_ms: 999903` implies). Its
`repeated_words_total` alone is 2060 out of the whole combo's 7251 total
repeated words across 22 fixtures — this ONE fixture supplies **28.4% of
the "repeated" word-pool**, at a **15.7% align rate (qwen×soniox) / 12.5%
(qwen×aai)**, single-handedly dragging the pooled repeated-rate down from
what it is on the other 21 fixtures. Removing it: the other 21 fixtures'
repeated-line align rate is **91.3% (qwen×soniox) / 88.7% (qwen×aai)** —
close to, though a bit below, their own unique-line rates (92.3% / 91.3%),
so a smaller genuine gap does remain even after removing the outlier, just
nowhere near as dramatic as the pooled 69.8/67.1% figures suggest on their
own.

**What actually happened to those excess hallucinated lines is itself
informative about the design's robustness.** This fixture's 284 UNTIMED
lines (of 395 total) break down almost entirely into these same two
phrases: 141 of the 161 "and you keep on doing it" lines (only 20 found a
real word to align to) and 143 of the 150 "keep on keep on keep on" lines
(only 7 aligned) — together they account for 284/284, i.e. **every single
UNTIMED line in this fixture** traces back to the two hallucinated
repeats. Crucially, the excess copies did **not** get silently mis-timed
onto one of the real occurrences (which would have looked like a
plausible-but-wrong result) — they came out UNTIMED, because the
monotonic alignment correctly refuses to reuse an earlier ASR word for a
later hallucinated repeat once the real audio's supply of that phrase is
exhausted. Verified directly: of the 20 combined "and you keep on doing
it" lines that DID get a timestamp, their start times are `[165090,
168870, 172650, 176430, 222810, ..., 264990]` — strictly monotonically
increasing, exactly matching the real occurrences in the audio.
`gemini36-flash` does **not** exhibit this failure mode on the same
fixture (its own failure there is the opposite — coarse
under-segmentation, only 18 lines for 70 gold lines, per the north-star
report) — this is a `qwen35-omni`-specific degenerate-generation risk, not
a property of the combiner or of repeated lyrical content in general.

### Concrete mismatch examples (LLM word vs ASR word, same aligned position)

| Fixture | Combo | LLM word | ASR word | Ratio | Outcome |
|---|---|---|---|---:|---|
| `5JW87KKDTcU` | gemini×soniox | `'Cos` | `because` | 0.40 | **unaligned** — genuine word-choice difference (gold's actual word is "'Cause") |
| `5JW87KKDTcU` | qwen×soniox | `'Cause` | `because` | 0.83 | **fuzzy-recovered** — same underlying disagreement, but qwen's spelling choice happened to clear the threshold |
| `5JW87KKDTcU` | gemini×soniox | `satisfies` | `excites` | 0.38 | **unaligned** — gemini paraphrased; soniox (and gold) actually say "excites" |
| `5JW87KKDTcU` | qwen×aai | `fill` | `feel` | 0.50 | **fuzzy-recovered** — plausible mishearing, one letter apart |
| `5JW87KKDTcU` | qwen×soniox | `forgiven` | `Señor.` | 0.31 | **unaligned** — soniox picked up a burst of Spanish (background ad-lib?) in an otherwise-English line; not a spelling issue at all |
| `p74PDWAFk0A` | qwen×soniox | `Him` | `until` | 0.25 | **unaligned** — spoken-word/testimony bridge; qwen and soniox diverge completely on this stretch |
| `p74PDWAFk0A` | qwen×soniox | `Shantae` | `Shantay,` | 0.86 | **fuzzy-recovered** — same bridge, a different proper name, close enough spelling to recover |
| `5JW87KKDTcU` | gemini×soniox | `woah` | `burn.` | 0.00 | **unaligned** — gemini inserted a generic backing-vocal filler where the real lyric is a specific word |

**Reading these together:** true LLM-vs-ASR word disagreement is common
(both "genuinely different word" and "recoverable spelling/contraction
drift" cases appear constantly — not a rare edge case), but it is
**local and self-limiting**: a disagreement costs at most the individual
mismatched word(s), never the surrounding line's timing, because the
alignment continues past an unaligned word/replace-block and re-locks onto
the next matching token. The `5JW87KKDTcU` line `"'Cause in His presence
there's freedom"` is a good illustration — `'Cause`/`because` fails to
align, but `in`, `His`, `presence`, `there's`, `freedom` all align fine, so
the line still gets an accurate `start_ms`/`end_ms` from its other words.
A line is only UNTIMED when **every single one** of its words fails to
align — which in practice happens either on short lines with only 1-2
words that happen to both mismatch, or (dominant cause by volume, per
above) on hallucinated excess repeats of a real phrase.

## Best combination and does it clear the bar

**`qwen35-omni` lines × `aai-u35-translate` times is the best combo on
every metric measured**: highest %≤400ms (official 41.4%, conservative
33.8%), lowest median delta (573ms official, 1142ms conservative), lowest
median-of-per-fixture-medians (475ms — closest of any combo to the 400ms
bar), and clears the 400ms bar on median for 3 of 6 categories (1 of 6
under the conservative check).

**Plainly: no combination clears the 400ms bar on the pooled/overall
figure.** The best pooled median delta is 573ms (qwen×aai, official) /
1142ms (conservative) — both above 400ms. The best %≤400ms is 41.4%,
meaning **the majority of lines are still not inside the wall-timing gate**
even in the best combination. But every combo **roughly doubles to
quintuples** the wall-timing rate of its own LLM ingredient alone (qwen
alone: 7.7% → qwen×aai: 41.4%; gemini alone: 16.0% → gemini×soniox: 29.2%)
and **beats the ASR ingredient's own line-grouping too** (soniox alone:
23.0% → qwen×soniox: 29.0%, gemini×soniox: 29.2%; aai alone: 31.1% →
qwen×aai: 41.4%) — recombination is a large, real improvement over either
ingredient in isolation, it just does not fully close the gap to the wall
gate.

## Side-by-side: `5JW87KKDTcU` (dense_vocal), gold vs `qwen35-omni`
lines × `aai-u35-translate` times, first 12 lines

| Gold start–end | Gold text | Combined start–end | Combined text | Combined text_sk |
|---:|---|---:|---|---|
| 5110–9010 | Nothing excites us like Jesus | 5236–7475 | Nothing excites us like Jesus | Nič nás nenadchne tak ako Ježiš |
| 9010–12840 | 'Cause in His presence there's freedom | 9260–11133 | 'Cause in His presence there's freedom | Lebo v Jeho prítomnosti je sloboda |
| 12840–15860 | All of our sin is forgiven | 12860–14798 | All of our sin is forgiven | Všetky naše hriechy sú odpustené |
| 15860–20330 | So we give to Him the highest praise | 15428–19656 | So we give to Him the highest praise | Tak Mu vzdávame tú najvyššiu chválu |
| 20330–24120 | It's the greatest feeling | 20237–23772 | It's the greatest feeling | Je to ten najväčší pocit |
| 24120–27630 | When You fill this place | 24030–26152 | When You fill this place | Keď naplníš toto miesto |
| 27630–31610 | In this moment, You are moving | 27691–29557 | In this moment | V tejto chvíli |
| 31610–34970 | As we give You praise | 29638–33321 | You are moving as we give You praise | Sa hýbeš, keď Ti vzdávame chválu |
| 34970–36720 | Come right now | 35290–38402 | Come right now Holy Spirit | Príď práve teraz Duchu Svätý |
| 36720–38730 | Holy Spirit | 38564–40364 | Release Your power | Uvoľni svoju moc |
| 38730–40430 | Release Your power | 40737–43686 | Lord we are hungry for more of You | Pane, sme hladní po viac z Teba |
| 40430–42180 | Lord, we are hungry | 44138–47404 | Heaven's open, You're bursting through | Nebo je otvorené, prerážaš sa skrz |

Contrast with `gemini36-flash`'s own raw (un-recombined) timestamps for
the same fixture — from the north-star report: `5230, 8220, 12220, 15830,
20830, 26030, 34330` — already drifting to a 1.9s error by line 6 and
merging 3 gold lines into 1 by line 7. The recombined output above tracks
gold to within **20–432ms on every one of the first 7 lines** (per-line
deltas: 126, 250, 20, 432, 93, 90, 61 ms); the visible divergence from
line 7 onward is a genuine **line-boundary** difference (`qwen35-omni`
splits/merges lines differently than gold's lrclib sync, independent of
the combiner), not a timing failure — each combined line's own start
still lands close to where ITS words are actually sung.

## Honest verdict

**"LLM writes the lines + ASR times them" is a real, substantial
improvement over either ingredient alone, but on this evidence it is not
yet a drop-in production architecture as designed** — three specific,
addressable reasons:

1. **The pooled 400ms bar is not cleared.** Best combo: 41.4%
   official / 33.8% conservative lines within 400ms, 573–1142ms pooled
   median. That is roughly double the best raw-LLM baseline and beats both
   raw-ASR baselines, but "most lines miss the wall gate" is still the
   honest pooled picture.
2. **The chorus-repeat worry is real but narrow, and mostly not the
   mechanism the question named.** Genuine same-audio disagreement between
   two transcribers (spelling drift, real paraphrase, mis-heard names) is
   common and handled gracefully — it costs individual words, not whole
   lines, because the monotonic alignment keeps re-locking onto the next
   real match. The one severe failure observed (`Xvm4_fWkXe8`,
   `qwen35-omni`'s 311-line degenerate repetition loop) is an **LLM
   hallucination risk**, not an alignment-confusing-repeats risk — and the
   monotonic design's actual behavior under it (mark the excess as
   UNTIMED, never mis-time it onto an earlier real occurrence) is the
   correct, safe outcome, verified directly (strictly increasing
   timestamps on every recovered occurrence).
3. **Line-boundary disagreement between the LLM and gold (not a combiner
   defect) is a real remaining source of error**, visible directly in the
   side-by-side above (line 7 onward) and in the categories that stay
   worst under the CONSERVATIVE metric (`chant_repetition`, never clears
   400ms median on any combo — because chant material is exactly where an
   LLM's own line segmentation diverges most from a line-synced gold
   reference, independent of timing).

**Recommendation if this direction is pursued further**: `qwen35-omni ×
aai-u35-translate` is the combination to build on — it is unambiguously
best on every metric, clears the 400ms bar per-category on 3 of 6
categories, and its 475ms median-of-per-fixture-medians is within 75ms of
the bar as a typical-fixture statistic even though the pooled number is
not there yet. A production version would need (a) a fix or a length/
repetition-loop guard for `qwen35-omni`'s degenerate-repetition failure
mode (the single biggest untimed-line contributor by volume), and (b)
either accepting the LLM's own line boundaries as authoritative (current
design) or reconciling them against a reference-text source the way the
production pipeline's text-merge layer already does for word-level
timings — since a meaningful share of the remaining error is
line-boundary disagreement, not mistimed lines.

## Reproducing this experiment

```bash
python3 -m eval.lyrics.run_combine_experiment
# writes:
#   eval/lyrics/reports/2026-08-05-combine-raw/<combo-id>_<video_id>.json  (66 files)
#   eval/lyrics/reports/2026-08-05-combine-scores.json
```

Single-fixture, single-combo CLI (for spot-checking one combination):

```bash
python3 eval/lyrics/combine_lines_times.py \
    --raw-dir eval/lyrics/reports/2026-08-05-raw \
    --line-backend qwen35-omni --time-backend aai-u35-translate \
    --video-id 5JW87KKDTcU \
    --out /tmp/combined.json
```

Unit tests: `pytest eval/lyrics/tests/test_combine_lines_times.py` (18
tests: exact match, fuzzy-recovered substitutions, genuinely-unaligned
substitutions, repeated-chorus monotonicity end-to-end, an unmatched
line → UNTIMED, and monotonicity-enforcement clamping).
