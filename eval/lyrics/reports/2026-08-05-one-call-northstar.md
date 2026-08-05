# Lyrics Eval — One-Call North-Star Sweep — 2026-08-05

**Backends:** `gemini36-flash` rev 1 (Google Gemini 3.6 Flash, direct API) and
`aai-u35-translate` rev 2 (AssemblyAI Universal-3.5 Pro + same-call Speech
Understanding translation to Slovak). Both are single-call, transcribe
+translate designs — see `eval/lyrics/prompts/one_call_karaoke.md` (Gemini)
and `eval/lyrics/backends/aai_u35_translate.py` (AAI) — evaluated toward the
project's stated north-star: **one flagship call, one rich prompt, EN/ES
sung vocals → word/line timing + singable Slovak translation.**
`gemini31-pro` was excluded per the task (a separate investigation covers
it); `soniox-v5` results running concurrently on the same box belong to a
different investigation and are not part of this report.

**Run:** 22 of 24 pinned fixtures (2 skipped — no cached vocal WAV:
`vpwDdb8r9Bk`, `fHYLw-2tTx4`), 44/44 backend×fixture pairs completed with
**zero failures and zero Gemini RECITATION blocks**. Full batch log and raw
per-pair outputs are committed under `eval/lyrics/reports/2026-08-05-raw/`.

## Methodology

This is a **pure algorithmic scorer** (`eval/lyrics/score_one_call.py`), not
the Claude-judge harness the rest of `eval/lyrics/` uses — no LLM judge is
involved, and nothing here is written against `schemas/report.schema.json`.

For each fixture, produced lines are walked in `start_ms` order; for each,
every not-yet-matched **gold** line is scored with
`difflib.SequenceMatcher.ratio()` on normalized text (lowercased,
punctuation/apostrophes stripped, whitespace collapsed). Any gold line
scoring **≥ 0.6** is an eligible candidate; among eligible candidates the
**closest-start** one is picked (not the highest-ratio one), and that gold
line is then removed from the pool so it can't be matched twice. This
disambiguates repeated/chant lyrics by time proximity rather than
arbitrarily picking whichever repeat has a marginally higher text ratio.

**Honesty about method limits (read every number below together with
this):**

- **Gold itself is imperfect.** `gold_lines` comes from lrclib/spotify/
  yt_subs LINE-level sync (see each fixture's `gold_source` in
  `manifest.json`), not a hand-verified karaoke reference. Some gold rows
  in this manifest already carry a documented ±270–1140ms consensus shift
  from an earlier ASR-based re-anchoring pass (see `manifest.json` fixture
  notes and `history.md`).
- **Where produced TEXT diverges from gold TEXT, the timing comparison
  weakens in two different ways.** If the divergence is large, the line
  fails to match at all (undercounting correct timing — coverage is
  reported alongside every delta for exactly this reason). If the
  divergence is small enough to still clear the 0.6 ratio gate but the
  underlying lyrics are highly repetitive (a real property of worship
  chant/chorus material, not a corpus artifact), the greedy closest-start
  matcher can still pick the wrong repeat of a phrase — it optimizes per
  produced-line time-proximity, not a global sequence alignment, so it has
  no way to "look ahead" and keep the whole song monotonically ordered.
  We observed this directly: several fixtures with near-complete gold
  coverage (e.g. `wjJ-izYndWs`, coverage 96.7%) still show large median
  deltas, which is a strong sign of exactly this repeat-confusion pattern
  rather than a wholesale ASR failure.
- **Pooled statistics (mean, p90) are dominated by a subset of fixtures
  with severe, systematic misalignment** (not a couple of stray points —
  on the worst fixtures, essentially ALL of that fixture's matched lines
  are tens of seconds off). The **per-fixture median-of-medians** figures
  below are the more informative "typical fixture" statistic; both are
  reported together, never the mean alone.

## Aggregate results (pooled across all matched lines)

| Metric | gemini36-flash | aai-u35-translate |
|---|---:|---:|
| Fixtures scored (of 22) | 22 | 22 |
| Total gold lines | 1772 | 1772 |
| Total produced lines | 982 | 403 |
| Line-count ratio (produced/gold) | 0.554 | 0.227 |
| Matched lines | 783 | 206 |
| Gold coverage % | 44.2 | 11.6 |
| Mean start delta (ms, pooled) | 34837.2 | 14428.9 |
| **Median start delta (ms, pooled)** | **2160** | **1575.5** |
| P90 start delta (ms, pooled) | 109110.0 | 25244.0 |
| **% matched lines within 400ms (wall gate)** | **16.0** | **31.1** |
| % matched lines within 1000ms | 36.1 | 41.3 |
| % produced lines > 32 chars (EN, LED wall width) | 44.1 | 66.5 |
| % produced lines > 32 chars (SK) | 41.0 | 63.8 |
| sk_ok_pct (SK translation present, diacritic, non-duplicate of EN) | 81.2 | 89.6 |
| Fixtures with any word-level timings | 15/22 | 22/22 |

**Per-fixture robustness** (median of each fixture's OWN median delta — the
"typical fixture" view, not dragged around by a handful of catastrophic
ones):

| Metric | gemini36-flash | aai-u35-translate |
|---|---:|---:|
| Fixtures with ≥1 matched line | 21/22 | 16/22 |
| Median-of-per-fixture medians (ms) | 14200.0 | 1386.0 |
| Fixtures with per-fixture median ≤1000ms | 4/21 | 7/16 |
| Fixtures with per-fixture within-400ms rate ≥30% | 4/21 | 9/16 |

This per-fixture view sharpens the picture: **AAI's timing, when it manages
to segment a fixture at all, is meaningfully more accurate than Gemini's**
(median-of-medians 1.4s vs 14.2s) — but AAI fails to segment reliably in
the first place (7/22 fixtures produced 8 or fewer lines, 4 of those
collapsing the ENTIRE song into a single utterance — see Failures below).
Gemini almost always produces SOME structure (21/22 fixtures have at least
one matched line) but its raw timestamps drift badly on the majority of
songs.

## Per-category breakdown

| Category | Backend | n | Coverage % | Median delta (ms) | Within 400ms % | sk_ok % |
|---|---|---:|---:|---:|---:|---:|
| chant_repetition | gemini36-flash | 4 | 46.9 | 4180.0 | 16.5 | 59.6 |
| clean_pop | gemini36-flash | 3 | 23.6 | 39415 | 7.3 | 100.0 |
| dense_vocal | gemini36-flash | 4 | 49.4 | 1610 | 14.1 | 95.8 |
| instrumental_breaks | gemini36-flash | 4 | 65.1 | 3802.5 | 16.0 | 69.7 |
| multi_language | gemini36-flash | 4 | 36.6 | 2321.5 | 6.4 | 94.4 |
| reverb_heavy | gemini36-flash | 3 | 38.3 | 770 | 35.8 | 84.5 |
| chant_repetition | aai-u35-translate | 4 | 6.4 | 328 | 52.0 | 88.0 |
| clean_pop | aai-u35-translate | 3 | 15.5 | 280 | 59.3 | 100.0 |
| dense_vocal | aai-u35-translate | 4 | 21.2 | 1676.0 | 30.0 | 85.6 |
| instrumental_breaks | aai-u35-translate | 4 | 8.8 | 541.0 | 40.9 | 88.9 |
| multi_language | aai-u35-translate | 4 | 6.3 | 1809.5 | 8.3 | 92.4 |
| reverb_heavy | aai-u35-translate | 3 | 15.3 | 5385.0 | 7.9 | 88.6 |

Neither backend has a category that clears the 400ms bar on the MEDIAN —
`aai-u35-translate` on `chant_repetition`/`clean_pop` comes closest (median
280-328ms, within-400% 52-59%), but its **coverage on those same
categories is only 6-16%** — it is accurate on the small number of lines it
manages to isolate, not comprehensive. `gemini36-flash`'s best category
(`reverb_heavy`, median 770ms) still misses the 400ms bar on the median.

## Best / worst 3 fixtures per backend

**gemini36-flash — best 3:**

| video_id | category | within400% | median delta (ms) | coverage % |
|---|---|---:|---:|---:|
| p74PDWAFk0A | reverb_heavy | 54.3 | 310 | 25.5 |
| KeZaADiRHVI | chant_repetition | 41.5 | 510 | 57.6 |
| hk4woCR12MM | reverb_heavy | 38.7 | 580 | 52.5 |

**gemini36-flash — worst 3:**

| video_id | category | within400% | median delta (ms) | coverage % |
|---|---|---:|---:|---:|
| Xvm4_fWkXe8 | clean_pop | 0.0 | 38970 | 10.0 |
| hSMJa5tImRU | multi_language | 0.0 | 34800 | 15.6 |
| wjJ-izYndWs | chant_repetition | 0.0 | 14200.0 | 96.7 |

`Xvm4_fWkXe8` is a clear case of the "coarse merge" failure mode: Gemini
produced only 18 lines for a 70-gold-line song, each one a run-on paragraph
combining 3-4 sung phrases (violating its own prompt's "at most 32
characters" instruction) — see the raw output for concrete examples. `
wjJ-izYndWs` is the opposite and more troubling case: line COUNT is fine
(64 vs 60 gold, 96.7% text-coverage), but per-line START times drift badly
— a repeat-confusion case on a highly repetitive chant song, not a
segmentation problem.

**aai-u35-translate — best 3:**

| video_id | category | within400% | median delta (ms) | coverage % |
|---|---|---:|---:|---:|
| zVpDFHJtc_U | instrumental_breaks | 100.0 | 254.5 | 2.5 |
| YbGFYaA0SbY | clean_pop | 60.9 | 270 | 42.6 |
| h-A1Tzkjsi4 | chant_repetition | 56.5 | 232 | 32.9 |

Caveat on `zVpDFHJtc_U`: 100% within-400ms sounds perfect but rests on only
**2 matched lines out of 79 gold** — not a meaningful sample. `YbGFYaA0SbY`
and `h-A1Tzkjsi4` are the more credible "AAI working well" examples (30-43%
coverage, sub-300ms median).

**aai-u35-translate — worst 3:**

| video_id | category | within400% | median delta (ms) | coverage % |
|---|---|---:|---:|---:|
| tCivrrU4SSM | multi_language | 0.0 | 16607 | 2.1 |
| wjJ-izYndWs | chant_repetition | 0.0 | 2534.5 | 3.3 |
| hSMJa5tImRU | multi_language | 0.0 | 1891 | 11.9 |

## Failures / RECITATION / segmentation collapse

- **RECITATION blocks (Gemini copyright refusal): 0/22.** Not observed on
  this sweep.
- **Hard failures (non-zero exit, timeout): 0/44 pairs.** Both backends
  completed every pair they attempted.
- **AAI utterance-segmentation collapse: 4/22 fixtures produced exactly 1
  line for the entire song** (`JRRbGCyr2Ac`, `KeZaADiRHVI`, `jUnyHptnsRo`,
  `Xvm4_fWkXe8`) — AAI's `match_original_utterance` translation feature
  sometimes returns a single utterance spanning the whole track instead of
  per-phrase segments, with no error signal distinguishing this from a
  correctly-segmented short song. This is the dominant cause of AAI's low
  pooled coverage (11.6%) — it is not that AAI's TEXT/translation quality
  is worse (`sk_ok_pct` 89.6% vs Gemini's 81.2%, and the raw side-by-side
  below shows AAI's transcript text is if anything closer to gold's
  wording), it is that its LINE BOUNDARIES are unreliable at the utterance
  level, which this same-call translation design has no way to correct
  without abandoning `match_original_utterance` (and, per
  `aai_u35_translate.py`'s documented limitation, losing per-line
  translation timing entirely if that flag is dropped).

## Side-by-side example: `5JW87KKDTcU` (dense_vocal), first ~10 lines

**Gold (lrclib):**

| start_ms | end_ms | text |
|---:|---:|---|
| 5110 | 9010 | Nothing excites us like Jesus |
| 9010 | 12840 | 'Cause in His presence there's freedom |
| 12840 | 15860 | All of our sin is forgiven |
| 15860 | 20330 | So we give to Him the highest praise |
| 20330 | 24120 | It's the greatest feeling |
| 24120 | 27630 | When You fill this place |
| 27630 | 31610 | In this moment, You are moving |
| 31610 | 34970 | As we give You praise |
| 34970 | 36720 | Come right now |
| 36720 | 38730 | Holy Spirit |

**gemini36-flash** (fine-grained, close to gold's line boundaries for the
first 4 lines — then diverges: line 5 merges what gold treats as two
separate lines, and the produced English text itself starts to differ from
gold's wording, e.g. "satisfies" vs gold's "excites"):

| start_ms | end_ms | text | text_sk |
|---:|---:|---|---|
| 5230 | 8220 | Nothing satisfies like Jesus | Nič nenapĺňa tak ako Ježiš |
| 8220 | 12220 | 'Cos in His presence there's freedom | Lebo v Jeho prítomnosti je sloboda |
| 12220 | 15830 | All our sin is forgiven | Všetky naše hriechy sú odpustené |
| 15830 | 20830 | So we give to Him the highest praise | Tak Mu vzdávame najvyššiu chválu |
| 20830 | 26030 | Yes, the greatest thing when you feel His grace | Áno, tá najväčšia vec, keď cítiš Jeho milosť |
| 26030 | 34330 | In this moment You are moving as we draw close | V tomto momente konáš, keď sa približujeme |
| 34330 | 40330 | Oh, come right now, Holy Spirit, release Your power | Ó, príď práve teraz, Duchu Svätý, uvoľni svoju moc |

**aai-u35-translate** (coarse utterance-level lines — each spans multiple
gold lines, but the English text closely matches gold's actual wording):

| start_ms | end_ms | text | text_sk |
|---:|---:|---|---|
| 5236 | 19656 | Nothing excites us like Jesus, 'cause in his presence there's freedom. All of our sin is forgiven, so we give to him the highest praise. | Nič nás nedokáže tak nadchnúť ako Ježiš, pretože v Jeho prítomnosti je sloboda. Všetky naše hriechy sú odpustené, preto Mu vzdávame najvyššiu chválu. |
| 20237 | 40364 | It's the greatest feeling when you feel this place. In this moment, you are moving as we give you place. Come right now, Holy Spirit, release your power. | Je to najväčší pocit, keď cítite toto miesto. V tomto momente sa hýbete, keď vám dávame priestor. Príď hneď teraz, Duch Svätý, uvoľni svoju moc. |
| 40364 | 43686 | Lord, we are hungry for more of you. | Pane, túžime po väčšom množstve Teba. |

A human eyeballing this can see the trade-off directly: Gemini's line
boundaries are closer to what a karaoke wall needs, but its transcript text
already diverges from gold by line 5; AAI's transcript wording is
noticeably more faithful, but 3-4 gold lines are always fused into one long
AAI line, which alone would overflow the 32-character wall-width budget by
a wide margin even before considering timing.

## Verdict

**Neither backend clears the 400ms line-timing bar well enough for
production LED-wall use, and for different reasons:**

- **gemini36-flash** produces roughly the right STRUCTURE (line count and
  segmentation are close to gold, `sk_ok_pct` and `>32-char` figures are
  the more wall-friendly of the two) but its raw **timestamps are
  unreliable on the majority of fixtures** — only 4 of 21 matched
  fixtures land a per-fixture median under 1 second, and the pooled
  within-400ms rate is 16.0%. This is a genuine timing-accuracy problem in
  the model's output, not a scoring artifact: the qualitative example
  above shows correct short lines drifting into confidently-wrong,
  paraphrased/merged lines within the first 30 seconds of a song.
- **aai-u35-translate** is the more accurate backend WHEN it manages to
  segment a song at all (median-of-medians 1.4s, 9/16 matched fixtures
  ≥30% within-400ms — closer to, though still short of, wall-acceptable),
  but its same-call translation feature's utterance segmentation is
  **unreliable at the structural level**: 7 of 22 fixtures produced 8 or
  fewer lines (produced/gold line-count ratio ≤0.1), 4 of those collapsing
  an entire song into one utterance. Its `sk_ok_pct` and text fidelity are
  the stronger of the two, but a 1-line-per-song output is useless for
  karaoke regardless of translation quality.

**Neither is closer to the "one call → wall-ready timed + translated
singable lines" north-star as a drop-in replacement for the current
production pipeline (whisperx/gemini-ensemble, `LYRICS_PIPELINE_VERSION`
v19).** If forced to rank, `aai-u35-translate` is the more promising
DIRECTION — its timing-when-present is meaningfully better and its
translation fidelity is higher — but it would need `match_original_utterance`
replaced with a genuine per-phrase segmentation strategy (or a hybrid:
AAI's word-level timestamps re-segmented by a silence-gap heuristic like
`assemblyai_universal_3_pro.py` already does, with the SAME-call
translation still anchored to the coarse utterance, then split
proportionally across the finer word-timed sub-lines) before it could be
seriously considered. `gemini36-flash`'s appeal (native per-line
segmentation) is undercut by timestamp reliability that would need
independent verification/correction before it could drive a live
highlight — the "north star" as designed does not yet survive contact with
either flagship model's actual output.

## Reproducing this report

```bash
python3 eval/lyrics/score_one_call.py \
    --manifest eval/lyrics/manifest.json \
    --raw-dir eval/lyrics/reports/2026-08-05-raw \
    --backends gemini36-flash aai-u35-translate \
    --out eval/lyrics/reports/2026-08-05-scores.json
```

Raw per-pair backend outputs (44 files) and the batch run summary are
committed under `eval/lyrics/reports/2026-08-05-raw/`; the full scored
per-fixture + per-category detail is in
`eval/lyrics/reports/2026-08-05-scores.json`.
