# ElevenLabs Forced Alignment backend (`elevenlabs-fa`)

2026-08-05 benchmark of the ElevenLabs Forced Alignment API against the same
22-fixture worship-song eval set used by `eval/lyrics/aligners/` (the
`ctc-forced-aligner` / `lyrics-alignment-mtl` shootout) and the earlier
`2026-08-05-combine-experiment.md` ensemble runs. Full methodology, results,
and verdict: `eval/lyrics/reports/2026-08-05-elevenlabs-fa.md`.

## What this is

A TRUE forced aligner: given FIXED reference text (an audio-LLM's
already-produced `lines[].text`, English as sung) plus the isolated-vocal
WAV, it returns per-word timestamps for that EXACT text. It does not
transcribe or reinterpret — unlike an ASR backend, there is no fuzzy
text-similarity matching between "what the model heard" and "what we asked
for": the words that come back ARE the words we sent, in the same order,
just timed.

## Verified request shape (live, 2026-08-05)

- **Endpoint + method** — `POST https://api.elevenlabs.io/v1/forced-alignment`
  (docs.elevenlabs.io/api-reference/forced-alignment/create).
- **Auth** — header `xi-api-key: <ELEVENLABS_API_KEY>`. A request without it
  returns 401 (verified empirically).
- **Request body** — multipart/form-data, two fields:
  - `file` (binary): the audio file. "All major audio formats are
    supported. The file size must be less than 1GB." (api-reference page).
  - `text` (string): "The text to align with the audio." The capabilities
    page adds: "Plain string only — do not wrap input text in JSON or any
    other structure." (docs.elevenlabs.io/overview/capabilities/forced-alignment).
- **Limits** (capabilities page): max audio duration "10 hours"; max text
  length "675,000 characters". Two DIFFERENT max-file-size figures appear
  across the two doc pages — api-reference's own field description says
  "less than 1GB" while the capabilities overview separately states "3 GB".
  Reported verbatim rather than silently picking one; every fixture WAV
  here is 9-33 MB, far under either figure.
- **Languages** — 29 languages, including "English (USA, UK, Australia,
  Canada)" and "Spanish (Spain, Mexico)" — both languages this catalog
  needs (`multi_language` fixtures code-switch EN/ES) are confirmed
  supported.
- **Pricing** — "the same rate as the Speech to Text API" (per-audio-second,
  not per-request); exact $/min is account-plan-dependent, see the
  account's own usage dashboard for the actual charge this run incurred.
- **Diarization** — "Not supported; providing diarized text will produce
  unexpected results." Not relevant here (isolated solo-vocal stems,
  single-speaker reference text).

## Response shape (verified against a live 200)

Top-level: `{characters: [...], words: [...], loss: <float>}`.

- `characters[]`: `{text, start, end}` — one per character of the exact
  text sent, `start`/`end` in **seconds** (float).
- `words[]`: `{text, start, end, loss}`. **The docs describe this as "List
  of words with their timing information," which reads as one entry per
  whitespace-delimited word — that is NOT what the live API returns.**
  Verified empirically on the first real call (276 tokens sent -> 551
  `words[]` entries back, exactly `2*276 - 1`): the array INTERLEAVES a
  separate whitespace-text entry (`{"text": " ", ...}`) between every pair
  of real words — the same convention `backends/soniox_v5.py` already
  documents for a different vendor's token stream. `elevenlabs_fa.py`'s
  `filter_content_words()` strips these before word-index mapping runs.
  `loss` (LOWER is more confident — it is a loss, not a probability) is
  "The average alignment loss/confidence score for this word, calculated
  from its constituent characters."
- `loss` (top-level, float): "The average alignment loss/confidence score
  for the entire transcript."

## Files here

- `elevenlabs_fa.py` — the backend script (`--wav --text-json --out` CLI,
  same convention as `backends/soniox_v5.py` / `backends/aai_u35_translate.py`).
- `confidence_correlation.py` — joins each matched line's `mean_word_loss`
  to its greedy-match `abs_delta_ms` (via `score_one_call.greedy_match`,
  imported not modified) and reports a pooled Pearson r plus 4 confidence
  quartile buckets.
- `raw/elevenlabs-fa_<video_id>.json` — committed per-fixture output for
  all 22 fixtures. **`words[]` is intentionally omitted from every
  committed file** (stripped remotely before transfer) — scoring only
  needs `text`/`text_sk`/`start_ms`/`end_ms`/`mean_word_loss` at the LINE
  level, and dropping ~87% of the payload keeps the repo diff reasonable.
  The full per-word response was inspected live during development
  (see the interleaving discovery above) but is not persisted.
- `scores.json` — `score_aligner.py` output (official + conservative views,
  per-category, poisoned-fixture, untimed, runtime).
- `confidence_correlation.json` — `confidence_correlation.py` output.

## Reference text preparation

Each source line's `text` (from `qwen35-omni_<video_id>.json`, the
audio-LLM's already-produced sung-lyric lines) is whitespace-tokenized,
punctuation left attached — identical convention to
`combine_lines_times.tokenize_line_text`, kept as a LOCAL copy (not
imported) so this backend has zero cross-import dependency on the offline
combine-experiment tooling. Every line's tokens are concatenated,
single-space-joined, into ONE transcript string sent as the `text` field —
the API has no line concept, only words. The token->line mapping is kept
locally so the returned flat `words[]` array (after interleave-filtering)
is split back into per-line `start_ms`/`end_ms` by plain index arithmetic
— no fuzzy text matching needed, unlike the ASR backends, because the
aligner is timing the EXACT text sent, not re-transcribing it. **Line
start_ms = the first aligned word's start_ms**, per the task brief.

If the API ever returns a word count different from the number of tokens
sent, this is an unexpected-shape failure and the whole fixture fails
loudly (both counts logged) rather than silently using an invalid mapping.

## Remote transfer note (2026-08-05, not part of the backend script itself)

Pulling the 22 output files back from the Windows machine to this repo was
NOT done via `mcp__win-resolume__FileRead` text relay — an initial attempt
at that produced a byte-length match but a **wrong sha256** on a 35,000-char
base64 chunk (content silently altered somewhere in the relay, despite
matching length), confirming this channel is unsafe for exact-byte transfer
at that size. The reliable path used instead: a temporary
`python -m http.server` on the Windows machine serving the (words-stripped)
`raw/` directory, `curl`'d directly from this dev box over the LAN
(`10.77.9.201` is reachable directly, confirmed with `ping`/`curl` before
using it), then the temporary server was stopped. Every fixture was
verified post-transfer by parsing the local JSON and comparing
`n_lines`/`sum(start_ms)`/`sum(end_ms)`/`n_untimed` against values computed
remotely before transfer — 22/22 matched exactly. See
`.claude/rules/lyrics-eval-backends.md` for the general FileRead/FileWrite
size-limit traps this avoids.
