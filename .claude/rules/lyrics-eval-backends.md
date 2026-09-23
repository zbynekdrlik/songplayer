---
paths:
  - "eval/lyrics/**"
---

# Lyrics-eval backend integration — verified API shapes and traps

Every shape below was proven against the LIVE API on 2026-08-05, and in several
cases the vendor docs were WRONG. Trust this file over the docs; re-verify only
if a call starts failing.

## Driving win-resolume from a backend session

- **`mcp__win-resolume__FileWrite` SILENTLY TRUNCATES content over ~20,000
  characters.** No error — it writes less than you gave it and reports the
  smaller count. Two dispatched agents lost significant time to this. Write
  large files in `append: true` chunks and verify the remote size/line count
  afterwards.
- **PowerShell mangles inline python.** `python -c "..."` breaks on `$`, quotes
  and backticks (`select key, length(value)` → *"The term 'value' is not
  recognized"*). ALWAYS `FileWrite` a `.py` file, then run it via `Shell`.
- **`FileRead` truncates around ~100,000 characters** (undocumented), and pulling
  a batch of result files one-by-one over MCP is punishingly slow. To retrieve
  many/large outputs, start a temporary `python -m http.server` on win-resolume,
  `curl` the files from the dev side, then stop the server. Two independent
  agents converged on this on 2026-08-05.
- Venv with `requests`: `C:\ProgramData\SongPlayer\cache\tools\lyrics_venv\Scripts\python.exe`.
  Fixture audio: `C:\ProgramData\SongPlayer\eval-cache\<video_id>_vocal16k.wav`.
- API keys live in SQLite `C:\ProgramData\SongPlayer\songplayer.db`, table
  `settings`. `eval-run\run_backend.py` holds a `BACKEND_SPECS` registry that
  injects them into the child's env — add new backends there. Never put a key on
  a command line or into a repo file. Note `gemini_api_key` is a COMMA-SEPARATED
  LIST (take the first entry).
- To store a NEW key without it ever touching the transcript: `airuleset.py
  secret request <NAME>` for a one-shot encrypted URL, then `secret exec <NAME>
  -- python3 store.py` where the script PATCHes
  `http://10.77.9.201:8920/api/v1/settings` (a dict of `{name: value}`, returns
  204), then `secret forget <NAME>`. `airuleset.py upload` is a FILE uploader —
  wrong tool for a credential.

## Vendor API shapes (verified live — docs were wrong where noted)

**DashScope / Qwen-Omni**
- Endpoint: the classic `https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions`.
  **Docs claiming an `sk-ws-` workspace key needs a per-workspace
  `https://{WorkspaceId}.<region>.maas.aliyuncs.com` host are empirically FALSE** —
  the classic host resolves the workspace from the Bearer token. No
  `X-DashScope-WorkSpace` header needed. `dashscope-us` rejects an international key.
- Audio: `{"type":"input_audio","input_audio":{"data":"data:;base64,<B64>","format":"wav"}}`
  (empty MIME before `;base64,`). Set `modalities:["text"]` to avoid paying for
  synthesized speech. `stream:true` is documented as required.
- **The zh docs' "base64 must be <10 MB" is FALSE** — 19.2 MB base64 (14.4 MB raw)
  worked. The real failure is a TLS drop (`SSLEOFError`, not a clean 4xx) around
  57 MB base64; transcode anything over ~14 MB raw to mono MP3 first.
- `qwen3.8-max` has NO audio input (`input_modalities: ["text","image","video"]`,
  and video is frame-sampling only — the audio track is never decoded). Audio
  lives in the Omni line: `qwen3.5-omni-plus` / `-flash`.

**AssemblyAI Universal-3.5 Pro + same-call translation**
- `speech_models: ["universal-3-5-pro"]` — **dashes, not dots**; a wrong id
  returns HTTP 400 listing the valid set.
- Translation nests under `speech_understanding.request.translation`, NOT a
  top-level `translation` key.
- `match_original_utterance` requires `speaker_labels: true`.
- Translated text comes back per-utterance at
  `utterance["translated_texts"]["sk"]` — not `transcript["translation"]["sk"]`.
- Known limitation: translated text carries only utterance-level timing; per-word
  timings exist for the source language only.

**Gemini 3.5 Transcribe (`gemini-3.5-transcribe`, preview 2026-08; verified 2026-09-12)**
- Upload `POST https://generativelanguage.googleapis.com/upload/v1beta/files`
  (`x-goog-api-key`, `X-Goog-Upload-Protocol: raw`,
  `X-Goog-Upload-Header-Content-Type: audio/wav`, body = WAV bytes) → `file.uri`;
  poll `GET /v1beta/{file.name}` until `state != PROCESSING`; then
  `POST /v1beta/interactions` with `{model, input:[{type:"audio",uri,mime_type}],
  generation_config:{transcription_config:{language_codes:["en-US"],
  mode:{type:"verbatim",timestamp_granularities:["word"]}}}}`.
- Words: `steps[*].content[*].annotations[*]` with `type: "word_info"`,
  `start_offset`/`end_offset` as STRINGS `"5.200s"` / `"9s"` (100 ms grid), no
  per-word confidence. `DELETE /v1beta/{name}` afterwards.
- `custom_vocabulary` is refused together with word timestamps — no text-guided
  path. 30 min max per file with timestamps. ≈ $0.005/min; a free-tier key
  returns `429 … exceeded your current quota` after ~14 songs and recovers in
  minutes — rotate through the `gemini_api_key` CSV list (entry #1 of the
  five is INVALID: `API key not valid`).
- Result 2026-09-12: 19.7 % gold-normalized ≤400 ms own lines (best dedicated
  ASR), 29.5 % as time source under Qwen lines — below mtl 31.6 %. Report:
  `reports/2026-09-12-gemini-3-5-transcribe.md`.

**Soniox stt-async-v5**
- `POST /v1/files` (multipart) → `POST /v1/transcriptions` with
  `{"model":"stt-async-v5","file_id":...,"translation":{"type":"one_way","target_language":"sk"}}`
  → poll `GET /v1/transcriptions/{id}` (error detail is `error_message`) →
  `GET /v1/transcriptions/{id}/transcript`.
- Auth is `Authorization: Bearer <key>` (unlike AssemblyAI's raw token).
- **Translated tokens carry NO timestamps** and are produced at segment
  granularity; distinguish them by `translation_status`
  (`original`/`none` vs `translation`). Tokens can be sub-word — join on leading
  spaces before grouping.

## Scoring — ALWAYS state the denominator, and never quote one view alone

A 2026-08-06 adversarial review found four scoring defects that had already put
wrong numbers in front of the user. All are fixed; these are the invariants that
keep them fixed. **Quoting a bare "% ≤400 ms" is the mistake — it is meaningless
without its denominator.**

### Corrections (2026-08-06)

A second-pass prose review of this file's own claims (not the JSON scorers —
those were already correct) found two more errors in the bullets below,
independently of the scoring-defect fixes above.

| # | Was | Is now | Cause |
|---|---|---|---|
| 1 | Conditional-column denominator stated as **1138–1243** | **1138–1236.** The max `n matched` across every committed row (baseline 1191, `ctc` 1139, `ctc-star` 1138, `elevenlabs-fa` 1236, `lyrics-alignment-mtl` 1236) is 1236 — no `1243` value exists in any committed JSON. | Independently re-derived from `2026-08-05-aligner-scores.json` / `2026-08-05-combine-scores.json` / `aligners_11l/scores.json`. |
| 2 | Monotonic view "drops the dropped pair from numerator AND denominator, so every backend's number rises — only the relative ORDER is meaningful" — stated as a universal law | **Usually rises, but not always** — it can FALL when the reordered greedy pass re-pairs to worse deltas (e.g. `gemini36-flash × soniox-v5` 29.3%→21.5% in `2026-08-05-combine-experiment.md`). Also, the 27–28% "binding backwards" rate and the 34–39% "pairs dropped" rate are two different numbers, not one — the original text conflated them. | Overgeneralized without checking every report's combo rows. |

Three views now exist, and a backend comparison quotes at least the first two:

- **conditional** (`pct_within_400ms`) — divides by matched-AND-timed lines only,
  so each backend is graded on the subset it handled and the denominator VARIES
  per backend (1138–1236 in the shootout). Useful, but **not** "% of lines
  correctly timed", and it flatters whichever backend left most lines untimed.
- **gold-normalized** (`pct_gold_within_400ms`) — same numerator over the
  identical gold-line count. **This is the comparable figure.** In the shootout
  the conditional band was 29–44% while the gold-normalized band was 20–32%.
- **monotonic** (`monotonic_match`) — the same matcher with an ordering
  constraint. `greedy_match` has NO monotonicity constraint and picks the
  CLOSEST-START eligible candidate, so on repetitive worship material 27–28% of
  pairs bind backwards in the song; rejecting them cascades and removes
  34–39% of all pairs — two different numbers, not one. The penalty differs
  per backend by up to 4 points, which exceeded the reported winning margin.
  It drops the dropped pair from numerator AND denominator, which USUALLY
  raises each backend's number — but not always: it can FALL when the
  reordered greedy pass re-pairs to worse deltas (e.g. `gemini36-flash ×
  soniox-v5` 29.3%→21.5% in `2026-08-05-combine-experiment.md`). Only the
  relative ORDER is meaningful here, never the direction of change.

Also load-bearing:

- **`POISONED_FIXTURE_VIDEO_ID = "Xvm4_fWkXe8"` must be excluded identically by
  EVERY scorer.** `score_aligner.py` excluded it and `run_combine_experiment.py`
  did not, so the baseline pooled 22 fixtures against the aligners' 21 under a
  header saying 21 — and 297 of the baseline's 394 "untimed" lines came from that
  one fixture, inflating a headline figure 3.3×. A test pins the two constants
  together; keep it.
- **Errored fixtures keep their gold lines in the denominator**
  (`total_gold_lines_all_fixtures`). Otherwise a backend that crashes with NO
  output file scores strictly BETTER than one that honestly writes all-untimed
  nulls — and backends in the same shootout used opposite conventions.
- **`mean` and `p90` are still not quotable** (the out-of-order pairs carry most
  of the delta mass, including a physically impossible 603 s pair).
- **Runtime must be split by device.** A CUDA-OOM→CPU fallback blended two
  fixtures at ~575 s into a 19-fixture GPU mean of 106 s and published 150 s.
  `metadata.device` / `cuda_oom_retried` are carried through — read them.

Reuse `combine_lines_times.py` / `run_combine_experiment.py` / `score_aligner.py`
so numbers stay comparable across sessions; see the `## Corrections (2026-08-06)`
block at the top of each report in `eval/lyrics/reports/`.

Gold itself (lrclib/spotify/yt_subs line-sync) has its own **unmeasured** noise
floor, and the models often transcribe what is REALLY sung more accurately than
the gold text — which weakens text-similarity matching while the timing is fine.
Say so when quoting any figure. Measuring the gold's own accuracy on a few
hand-verified fixtures is the highest-value next step before any further model
hunting — until it exists, every number here is a floor, not a verdict.

## CI eval-checks — verify locally with the PINNED ruff, not your system ruff

The `eval-checks` CI job (`.github/workflows/ci.yml`) runs `ruff check eval/` +
`ruff format --check eval/` + `pytest eval/lyrics/tests` under **`ruff==0.5.7`**
(pinned in the pip install step). A newer local ruff (0.16.x) enables rules the
0.5.7 default set does not (RUF059, I001, E501…) and will report failures CI
never sees — do NOT trust it. Verify eval changes against CI's exact toolchain:

```bash
python3 -m venv /tmp/evalvenv
/tmp/evalvenv/bin/pip install -q ruff==0.5.7 pytest==8.3.2 jsonschema==4.23.0 requests==2.32.3
/tmp/evalvenv/bin/ruff check eval/ && /tmp/evalvenv/bin/ruff format --check eval/
/tmp/evalvenv/bin/python -m pytest eval/lyrics/tests -q
```

Deleting a backend means deleting its `tests/test_<backend>.py` too (pytest
imports it), but the run-script name-string registries (`BASELINE_BACKENDS` /
`COMBOS` / `DEFAULT_BACKENDS`) are dynamic — a stale name there is a `ruff`/
`pytest` no-op, not a failure, so prune them for cleanliness, not correctness.

## mtl aligner memory + on-box probe (#144 r3)

- **The upstream `wrapper.align` runs whole-song forwards with autograd ON.**
  It keeps the acoustic model's whole-song graph alive while the boundary model
  runs; past ~4:15 an unchecked allocation in torch's CPU conv2d faults the
  interpreter (`c10.dll 0xc0000005`, ~8 s after `preprocess done`). Our
  `run.py::align_fixture` wraps EACH of the three `wrapper.align()` calls (cuda,
  cpu, cuda-OOM→cpu retry) in `torch.inference_mode()` via `_inference_ctx()` —
  drops the graph (probe B: 170 s, peak WS 2.9 GB, identical output, so NO
  `LYRICS_PIPELINE_VERSION` bump). Never remove it; never call the wrapper
  outside it. The mtl child also carries `-X faulthandler` (`mtl_aligner.rs::
  build_args`, before the script path) so any future AV prints the Python stack
  to the stderr tail `run_once` already logs.
- **On-box probe recipe that found it:** pause the workers via
  `PATCH /api/v1/settings {"lyrics_worker_enabled":"false","stem_worker_enabled":"false"}`,
  then run `verify\mtl_probe.py` DETACHED with `Start-Process
  -RedirectStandardOutput/-RedirectStandardError` (Windows PowerShell 5.1 has NO
  `-PriorityClass` — set `$p.PriorityClass='Idle'` and
  `$p.ProcessorAffinity=[IntPtr]0xE00000` on the launcher AND on its `python.exe`
  child after start), `-X faulthandler` so an AV prints the stack; re-enable the
  workers afterwards.
- **`mtl_aligner_venv\Scripts\python.exe` is the venv LAUNCHER (268 KB), not the
  interpreter** — the real CPython is a child `C:\Program Files\Python312\
  python.exe`. So a containment fault sampler on the launcher pid sees 0 faults/s
  and Application-log crash events name the SYSTEM python path, not the venv one.
