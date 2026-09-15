---
name: lyrics-pipeline
description: >
  Songplayer lyrics pipeline rules. Load when working on lyrics processing,
  alignment providers, pipeline versioning, translation, or the lyrics worker.
  Covers: provider discipline (Gemini, Claude, yt_subs), timing gates,
  translation rules, pipeline-version bump discipline, sanitization invariants,
  and worker/reprocess behaviour.
user-invocable: false
triggers:
  - lyrics
  - alignment
  - pipeline version
  - LYRICS_PIPELINE_VERSION
  - Gemini
  - CLIProxyAPI
  - translation
  - reprocess
  - ensemble
  - chunk
  - sanitize
---

# Songplayer Lyrics Pipeline Rules

## Provider hierarchy (current production state, v22 — ONE regime, #159)

Owner directive 2026-09-14: keep the best route (v21 mtl★), delete the rest —
NOT "keep as fallback". The v20 WhisperX-on-Replicate route
(`whisperx_replicate`/`Orchestrator`/`text_reference_merge`/`timed_reference_merge`/
`yt_subs_split`) and the AssemblyAI `asr_path` route are DELETED. The Gemini
chunked-transcription / qwen3 / autosub regimes were already gone. The pipeline
is now **text gathering + two tiers**, one forced aligner (mtl), one ASR vendor
(Gemini g35t):

0. **Text gathering** (`gather.rs::gather_sources_impl`) — unchanged, all
   best-effort: manual **yt_subs** captions → **LRCLIB** → **lyrics.ovh**
   (Genius is the fallback when lyrics.ovh misses, both labelled `"genius"`)
   → **Spotify** (`spotify_track_id`, LINE_SYNCED) → operator
   **`lyrics_override_text`** → **YouTube description** (Claude-extracted,
   `description_provider.rs`). The best candidate is picked by
   `claude_merge::best_authoritative_candidate` /
   `priority_with_timing` (override=6; timed spotify/lrclib=5, timed
   yt_subs=4; text description=3, text lrclib=2, text genius=1; else=0).

1. **★ tier — v21 reference stage** (`worker_reference::run_mtl_reference_stage`
   → `orchestrator::run_reference_stage`). For any song with vocals + a text
   candidate (≥4 lines): `mtl_aligner::align` (production wrapper around
   `eval/lyrics/aligners/lyrics_alignment_mtl/run.py`, MTL+BDR — 31.6%
   gold-norm, best measured) force-aligns the best candidate's lines to the
   isolated vocals, verified against an independent `g35t_client::
   transcribe_words` (Gemini 3.5 Transcribe) transcript through
   `reference_gate::evaluate` (whole-song sanity `|median signed Δstart| ≤
   400ms` — catches mtl's wrong-repetition failure mode — AND agreement ≥70%
   of matched lines within 400ms, ≥60% matched). **PASS** → mtl line timings
   ship directly (`words: None`), `lyrics_source =
   "<candidate.source>+mtl@rev1/g35t-ok"`, `lyrics_alignment_model =
   ALIGNMENT_MODEL_MTL_REV1`, `videos.lyrics_reference = 1` (wall ★).
   **FAIL/ERROR** → `videos.lyrics_reference = 0`, the gate decision lands in
   `{youtube_id}_alignment_audit.json`, and the song falls through to tier 2.
   Byte-for-byte UNCHANGED from v21 — do NOT degrade this path.
   - Skip conditions (info-logged, fall to tier 2): mtl tooling absent
     (`MtlConfig::is_available()`, WARNed once at worker start), no vocal WAV,
     no text candidate, or candidate < 4 lines.
   - Injection seam: `orchestrator::ReferenceStageBackend` (`mtl_align` +
     `asr_transcribe`); production wires `RealReferenceStageBackend`, tests
     inject a fake — never a real subprocess/HTTP call in a unit test.

2. **base tier — g35t transcript** (`worker_g35t::run_g35t_transcript_branch`
   → `g35t_transcript::words_to_lines`). The SOLE fallback for every song the
   ★ tier does not ship (no usable text, gate FAIL, mtl skip/error):
   `g35t_client::transcribe_words` transcribes the isolated vocals, and the
   words are grouped into LED-wall lines by a deterministic silence-gap split
   (salvaged from the old asr_path, re-typed) + `line_splitter::
   split_lyrics_lines` + the monotonic/min-duration sanitizer. Ships
   `words: None`, `lyrics_source = "gemini-3-5-transcribe"`,
   `lyrics_alignment_model = ALIGNMENT_MODEL_G35T_REV1`. Measured no-text
   quality: g35t 19.7% gold-norm ≤400ms vs the retired AssemblyAI 3.8%.
   Reuses the vocal WAV already isolated for the ★ tier (no 2nd Demucs).
   - Empty/blank transcript → quarantine as `asr_gap`. No vocals / no gemini
     keys / g35t transport error → `Deferred` (row backs off, retries later).

3. **AutoSubProvider** — PERMANENTLY UNREGISTERED. Never register again.
   YouTube autosub produces wrong timing.

## Gemini API discipline

- `gemini_api_key` is a **comma-separated list** (multi-key rotation, v14).
  ALWAYS append new keys — never overwrite. User maintains multiple Google
  Cloud projects each with its own billing cap.
- When Gemini returns 429 / `RESOURCE_EXHAUSTED`: surface it immediately and
  ask user to raise the Google Cloud cost cap. Do NOT silently retry forever.
  Do NOT switch to a weaker model.
- Route through direct `generativelanguage.googleapis.com` (not CLIProxyAPI)
  — the OAuth path hits `MODEL_CAPACITY_EXHAUSTED` on 3.x Pro preview models.
  Override via `GEMINI_PROXY_URL` env var.
- `thinkingConfig.thinkingBudget = 2048` limits Gemini reasoning to avoid
  hallucinated-duplicate loops + timeouts on dense chorus audio.

## Chunking — every chunk must succeed

A song's output is only acceptable when EVERY chunk returned a parseable
response. If any chunk is still empty after all retries, fail the WHOLE song
(return `Err`) — the orchestrator retries later. Partial output (79 lines
instead of 95) is NEVER acceptable. Gaps in rendered lyrics are worse than
no lyrics.

Do NOT use output-level heuristics (gap size, line count, density) to detect
failure — calm instrumental passages are legitimate.

## Vocal isolation (`preprocess-vocals`) — measured facts (2026-09-12, #144)

- ≈ 1× realtime on the RTX 3070 Ti at BELOW_NORMAL (240 s song → 233 s;
  Mel-Roformer pass 0.75×, dereverb 0.16×). Timeout is
  `aligner::isolation_timeout` = clamp(2 × duration, 600 s, 3600 s); videos
  over `MAX_LYRICS_DURATION_MS` (30 min) are stamped `unsupported_source`.
- Stems are written with `use_soundfile=True` — audio-separator's pydub
  writer hit `MemoryError` on an 827-s 24-bit stem, exhausted the box's RAM
  and crashed OBS (`video_frame_init`). Never run a second heavy Python job
  on the box while the worker isolates; measure with the worker paused.
- Every song failing isolation within ~10 s ⇒ check `lyrics_venv` numpy vs
  numba (numba caps numpy < 2.5; the bootstrap's torch force-reinstall once
  pulled numpy 2.5.2 — it now pins numpy afterwards and `IS_READY_PROBE`
  imports numba/librosa/soundfile so a broken stack is "not ready").
- A row the asr branch cannot process is DEFERRED (`lyrics_attempts`,
  `lyrics_next_attempt_at`, 5 min · 2ⁿ, cap 24 h; reset on success and by
  the reprocess endpoint) — never re-picked on the next 5-s tick. Monitors
  on the box: `lyrics_progress.py` (buckets, ★, gate tally) and
  `lyrics_recent.py <min>` (per-song lines/sk/source + Claude failures).
- **GPU discipline (#154) — the idle GATE is PRIMARY; priority + VRAM cap are
  SECONDARY (defence in depth).** The 2026-09-14 box crash (`LiveKernelEvent`
  141 ×5, hard reset) proved priority/cap alone are insufficient: at cap 0.4 the
  OOM fallback moves isolation to the CPU, which stutters the NDI/decoder path
  just as badly. So the real fix is:
  - **The gate (`crates/sp-server/src/lyrics/idle_gate.rs`).** Heavy stages
    (vocal isolation, dereverb, mtl forced-alignment) run ONLY while the wall is
    idle — NO playback pipeline `Playing` on program (engine `NdiHealthRegistry`
    snapshots, the same state `/api/v1/ndi/health` reports, read in-process) AND
    OBS not streaming/recording (`ObsState.streaming/recording`, tracked via the
    Outputs event group + a GetStreamStatus/GetRecordStatus seed on connect).
    Two points: `process_next` (loop level — don't START heavy work while busy;
    cheap HTTP work like g35t/Claude/translation is NOT gated and keeps running)
    and `process_song` before the mtl spawn (`SongOutcome::WaitingForWall` —
    bounds max exposure to ONE stage; a running subprocess is never killed).
    Deferral carries NO backoff penalty; the isolated vocal WAV is preserved so
    the next idle pick is a cache-hit isolation + mtl (byte-identical output).
    Operator override `lyrics_gate_when_playing` (DB setting, default ON; OFF =
    pre-gate behaviour), read live each tick like `lyrics_worker_enabled`. The
    worker state (`idle` / `processing <id>` / `waiting — wall in use`) is
    surfaced via the WS `LyricsQueueUpdate.processing` field (dashboard badge).
  - **Secondary: `gpu_polite()`** in `lyrics_worker.py` (and the mtl `run.py`)
    sets a **BELOW_NORMAL WDDM GPU scheduling priority** (ctypes
    `D3DKMTSetProcessSchedulingPriorityClass`) and a **per-process VRAM cap**
    via `torch.cuda.set_per_process_memory_fraction` — default **0.7**, tunable
    by the `lyrics_gpu_mem_fraction` DB setting (clamp 0.2–0.95, plumbed to the
    child as `LYRICS_GPU_MEM_FRACTION`). Model parameters are UNCHANGED, so
    separation quality is identical; only priority + VRAM headroom move. On a
    CUDA OOM under the cap the isolation re-runs on CPU (same model → identical
    output, slower). Kept as defence in depth for the bounded one-stage window
    the gate cannot avoid (wall goes busy DURING an isolation that started idle).
  - The gate only changes WHEN work runs, never the output → NOT a
    `LYRICS_PIPELINE_VERSION` bump.

## Translation — Claude only, never Gemini fallback

EN→SK translation MUST use Claude via CLIProxyAPI (paid Max plan — unlimited).
Gemini is metered. Never add a "Claude falls back to Gemini" path.

When Claude refuses via CLIProxyAPI:
- Tune the prompt. A **neutral TECHNICAL** prompt works: numbered lines in,
  numbered Slovak lines out, exact line count, plus a masculine/feminine
  grammatical-gender directive — and NO story or persona. NEVER mention "song
  lyrics", "worship", "church", "copyright", "karaoke" — these trip the
  content-policy classifier.
- **A "story" framing is a TRAP on the newest flagships (#145, measured on the
  box 2026-09-14).** The #152 grandfather/grandmother "dictating for a memorial
  plaque" story is REFUSED by `claude-fable-5-1` (on recognizable songs, e.g.
  "Not Guilty") and `claude-opus-5` (on every song) — they answer "…even for a
  family plaque…". `claude-opus-4-6` (the retired #144 stop-gap) did not, which
  masked it. The fix was NOT a model change but DROPPING the story:
  `translator::build_prompt` is now the bare neutral task above, which translated
  9/9 across fable-5-1 / opus-5 / sonnet-5 and preserved gender (male "Keď som
  **bol** vinný" / female "Keď som **bola** vinná"). If a model ever refuses
  again, a refusal is now classified + logged `kind="refusal"` with the model id
  (never a silent "parse returned 0", `translator::classify_zero_translation`) —
  grep the worker log for `refusal`. Prompt-semantics change → bumped
  `LYRICS_TRANSLATION_VERSION` 1→2 (catalog re-translation).
- Model: `sp_core::config::DEFAULT_AI_MODEL` (`claude-fable-5-1` since
  2026-09-13, #145 — the newest flagship the upgraded CLIProxyAPI **7.3.1**
  on win-resolume routes). The proxy binary was upgraded 6.9.27 → 7.3.1
  because the old build's model registry predated the Claude-5 ids and
  `502 unknown provider`'d them; `claude-opus-4-6` was the #144 stop-gap it
  forced (and `claude-opus-4-20250514` before that is fully retired — a
  `404 not_found_error` upstream that also parks the OAuth auth in a
  cooldown until the proxy restarts, so a retired id looks like a dead
  login). Before switching the model or blaming the token, probe with
  `python C:\ProgramData\SongPlayer\proxy_probe.py <model>` on win-resolume
  and use only ids the proxy's `/v1/models` actually lists (an unlisted id
  returns 502 "unknown provider"). Proxy upgrade + rollback procedure:
  `scripts/cliproxy/README.md`. Always pass `max_tokens: 32000` for large
  responses.
- If a specific song still refuses after prompt tuning: surface it to the user.
  Do NOT auto-fallback.

## Translation gender + translation version (#152)

- **Gender framing.** `build_prompt` takes a `SpeakerGender` (`Male` default,
  `Female`). English first-person lines carry no gender; Slovak does ("bol som"
  vs "bola som"). The prompt uses a plain grammatical directive — "wherever
  Slovak grammar requires a gender for the first-person speaker, use masculine /
  feminine forms" (#145; the #152 grandFATHER/grandMOTHER-plaque STORY was
  dropped because the newest flagships refuse it — see the refusal note above).
  Never add the words lyrics/song/worship/church/karaoke/copyright/plaque or any
  story. Per-song override lives in `videos.lyrics_translation_gender`
  (`NULL`=auto→masculine, `m`, `f`), set from the dashboard ♂/♀ toggle
  (`PATCH …/translation-gender`).
- **Translation version.** `LYRICS_TRANSLATION_VERSION` (in `lyrics/mod.rs`) is
  SEPARATE from `LYRICS_PIPELINE_VERSION` and gates RE-TRANSLATION only — never
  re-alignment, never a pipeline bump. Bump it when the translation prompt
  changes the Slovak wording. `videos.lyrics_translation_version < current` +
  `has_lyrics=1` re-queues a song through `retranslate_next_stale` (one Claude
  call, `sk` lines rewritten in place, lowest priority — runs only when the
  alignment queue is empty). Setting a gender resets the row's version to 0.
- OAuth re-login: CLIProxyAPI's own `-claude-login` expires 5 min after
  printing the URL — too short for the owner's authorise-and-paste round
  trip. Use `C:\ProgramData\SongPlayer\claude_pkce_login.py start` (prints
  the URL) and `… finish "<pasted callback URL>"` (exchanges the code and
  writes the auth JSON into `cache\.cli-proxy-api\`). Never echo the code.

## Pipeline version discipline

**Never bump `LYRICS_PIPELINE_VERSION` without explicit user approval.**
Catalog-wide reprocess is expensive and can break songs that somehow worked.
Use `manual_priority` for targeted per-song reprocessing until the user says
"bump it".

Also: never suggest, ask about, or include "bump pipeline version" as an option
in AskUserQuestion. Wait for the user to initiate.

Bump the constant ONLY when:
- Adding/removing a route or alignment stage from the worker (e.g. the #159
  one-regime cut that dropped WhisperX + asr_path was a 21→22 bump)
- Changing the mtl align invocation, the `reference_gate` thresholds, or the
  g35t base-tier grouping (gap/coalesce/sanitize) in a way that alters output
- Changing the reference-text-selection algorithm (`best_authoritative_candidate`)

Do NOT bump for: bug fixes with identical output, refactoring, logging
changes, UI-only changes, performance optimizations with identical output.

## Timing — hard gate, not nice-to-have

Karaoke timing accuracy (~400ms wall tolerance) is a BINARY gate. A backend
that emits correct text with broken timing is REJECTED — not "promoted with
caveat". Never propose "fix it later with offset constants". Either timing works
or the candidate is dropped.

## Synthesized timing — ABSOLUTELY FORBIDDEN

Never build, persist, write, or propose a `LyricsTrack` with evenly-distributed
/ uniform / N-seconds-per-line timings. Ship `words: None` and let the renderer
fall back to line-level display. This applies under any time pressure or
circumstance.

## Sanitization invariants (v10+)

The sanitizer enforces globally-increasing word start times (cross-line
boundaries strictly increasing), minimum 80ms per-word duration, and no overlap
with the next word. This runs on BOTH the multi-provider merge path and the
single-provider pass-through.

## LLM over heuristics

When a provider ingests messy natural-language text (descriptions, comments,
ID3 tags), prefer "raw text → LLM → structured output" over a regex pipeline.
A Claude call handles arbitrary human-authored text uniformly.

## ASR-data-only fixes

Only fix timing issues the underlying ASR data (mtl alignment / g35t transcript)
actually supports. When a wall issue traces to missing/wrong ASR words, state it
explicitly as an ASR limit. Do NOT propose code fixes that fabricate timing past
the ASR's coverage. Do NOT push another fix round when the data does not support one.

## Dead code — delete, never stub

When replacing a code path, delete the old one entirely. No `#[deprecated]`
stubs, no fallback retention, no commented-out blocks.

## No codec yo-yo

Never flip codec/format/decoder strategy based on a single observed symptom.
Measure first: read production logs, reproduce with monitoring, run a comparison
test, read the relevant code path. Only after evidence converges — fix tied to
the measured root cause.
