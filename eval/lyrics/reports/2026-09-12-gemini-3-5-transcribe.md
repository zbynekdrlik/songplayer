# 2026-09-12 — Gemini 3.5 Transcribe on the 21-fixture lyrics bench

Owner question: Google shipped a new speech-to-text model (Gemini 3.5
Transcribe, public preview 2026-08-26) — is it the final candidate for
wall timing? **No.** It is the best *standalone* dedicated-ASR timing
source we have measured (19.7 % gold-normalized ≤400 ms vs Soniox v5 16.0 %),
and paired with Qwen3.5-Omni line text it reaches 29.5 % — but the forced
aligner `lyrics-alignment-mtl` still leads at 31.6 %, and nothing is near the
400 ms wall bar. Same conclusion as 2026-08-06: the model hunt stays closed.

## Setup (identical to the 2026-08-05 shootout so the numbers pool)

- Adapter: `eval/lyrics/backends/gemini_3_5_transcribe.py` (rev 1), run from
  win-resolume against `eval-cache/<video_id>_vocal16k.wav`, `language_codes
  = ["en-US"]`, verbatim mode, word timestamps, no custom vocabulary (the API
  refuses vocabulary + word timestamps together). Raw outputs:
  `reports/2026-09-12-raw/gemini-3-5-transcribe_<video_id>.json` (22 files;
  `vpwDdb8r9Bk`, `fHYLw-2tTx4` have no cached WAV → counted as errored, gold
  lines kept in the denominator).
- Scored with the unchanged `score_one_call.py` (own `lines[]`, 400 ms
  silence-gap grouping as in the AAI/Soniox adapters) →
  `reports/2026-09-12-scores.json`; combos via `run_combine_experiment.py` with
  the new backend as time source (COMBOS overridden from a wrapper, outputs kept
  out of the repo).
- Per-song API latency 12–33 s (upload + transcribe), ~25 audio tokens/s,
  list price ≈ $0.005/min. One key of the five in `gemini_api_key` is invalid
  (`API key not valid`); a free-tier key hits `429 … exceeded your current
  quota` after ~14 songs and recovers within minutes — the batch driver rotates
  keys.

## Results — always with the denominator

Pooled over 21 fixtures (poisoned `Xvm4_fWkXe8` excluded), 1702 gold lines
(1867 incl. the two errored fixtures).

| backend (own lines) | gold-normalized ≤400 ms | conditional ≤400 ms (denominator) | coverage | median Δ | line ratio |
|---|---:|---:|---:|---:|---:|
| **gemini-3-5-transcribe** | **19.7 %** (17.9 % of all 1867) | 36.3 % (922 matched+timed) | 54.2 % | 740 ms | 0.81 |
| soniox-v5 (2026-08-05) | 16.0 % | 22.8 % (1193) | 70.1 % | 1090 ms | 2.13 |
| aai-u35-translate (2026-08-05) | 3.8 % | 31.1 % | — | — | — |
| lyrics-alignment-mtl (forced aligner, text given) | **31.6 %** | 43.5 % (1236) | — | — | — |
| elevenlabs-fa (forced aligner) | 30.7 % | 42.2 % (1236) | — | — | — |

Without the four gold-suspect fixtures of #129 (`hSMJa5tImRU`, `wjJ-izYndWs`,
`JjgkhHlTROQ`, `JRRbGCyr2Ac`; 17 fixtures, 1249 gold lines): 24.1 %
gold-normalized, 42.9 % conditional, median 540 ms — vs mtl 35.3 % in the same
view.

Combos (LLM line text + this backend's word times), official / monotonic view:

| combo | gold-normalized ≤400 ms | conditional | median Δ | word-align rate |
|---|---:|---:|---:|---:|
| qwen35-omni lines × **gemini-3-5-transcribe** times | **29.5 %** / 22.7 % mono | 45.6 % / 51.1 % mono | 500 / 390 ms | 0.81 |
| gemini36-flash lines × gemini-3-5-transcribe times | 18.9 % / 13.8 % | 48.3 % / 40.7 % | 450 / 650 ms | 0.78 |
| gemini36-flash lines × soniox-v5 times (2026-08-05 reference) | 12.7 % / 7.8 % | 29.3 % / 21.5 % | 750 / 950 ms | 0.87 |

By category (own lines, conditional): clean_pop 61.9 %, instrumental_breaks
56.7 %, dense_vocal 42.9 %, chant_repetition 28.6 %, multi_language 28.6 %,
reverb_heavy 18.0 %. Best fixture `tCivrrU4SSM` 95.5 % / 108 ms median; worst
`JjgkhHlTROQ` collapsed to 2 lines / 6 words for a 215 s song (same failure
shape AAI showed on 4 songs in August).

## Reading

- Timing quality per matched line is the best of the dedicated-ASR family
  (36.3 % conditional vs 22.8 % Soniox) and the words are chronological
  enough that the combine step reports only a handful of out-of-order words.
- Coverage is the limiter: 54 % of gold lines matched. It under-produces lines
  (0.81×) — repeated choruses get merged or dropped — and multi-language /
  reverb songs lose most lines.
- It cannot take our lyric text as guidance while emitting timestamps, so the
  forced-aligner advantage (text known, only timing solved) is structurally
  out of reach for it.
- Verdict for the wall: **not the final candidate**; a strong drop-in time
  source if the #130 per-song gate ever needs a cheaper ASR than AssemblyAI,
  nothing more. Gold noise (#129) still caps every number here.
