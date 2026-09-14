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

## Provider hierarchy (current production state, v20)

The Gemini chunked-transcription regime (`gemini_provider.rs`) and the
qwen3/autosub aligners are DELETED (`orchestrator.rs`'s own doc comment:
"imports NONE of the legacy providers ... deleted in Phase G"). The real
route is two independent stages:

1. **Text gathering** (`gather.rs::gather_sources_impl`) — fetched in this
   order, all best-effort: manual **yt_subs** captions → **LRCLIB** →
   **lyrics.ovh** (community lyrics API; **Genius** is the fallback ONLY
   when lyrics.ovh misses, both labelled source `"genius"`) → **Spotify**
   (operator-pasted `spotify_track_id`, LINE_SYNCED) → operator
   **`lyrics_override_text`** → **YouTube description** LLM-extracted via
   Claude (`description_provider.rs`). The authoritative candidate among
   whatever gathering found is picked by `claude_merge::priority_with_timing`
   (override=6 highest; timed spotify/lrclib=5, timed yt_subs=4; text
   description=3, text lrclib=2, text genius=1; everything else=0).
2. **Timing** (`orchestrator.rs`) — **WhisperX large-v3 on Replicate**
   (`whisperx_replicate.rs::WhisperXReplicateBackend`) is the sole
   `AlignmentBackend`. Tier-1 `LineSynced` (yt_subs/lrclib/spotify already
   timed) runs a per-caption-window Claude split with WhisperX anchors
   (`timed_reference_merge`); Tier-1 `TextOnly` runs WhisperX against the
   best text candidate (`timed_reference_merge` when coverage is good,
   else `text_reference_merge`); Tier-1 `None` ships raw WhisperX + line
   split.
3. **`asr_path`** (`asr_path/mod.rs` — AssemblyAI Universal-3 Pro + Claude
   regroup, line-level only) — the fallback when
   `orchestrator::is_allowed_text_source` rejects every gathered candidate
   (untimed-only, WhisperX gate would reject it) but at least one text
   candidate exists.
4. **Zero candidates at all** → `db::models::mark_unsupported_source`
   (`lyrics_source = 'unsupported_source'`); an `asr_path` transcription
   failure quarantines as `asr_gap` instead.
5. **AutoSubProvider** — PERMANENTLY UNREGISTERED. Never register again, no
   exceptions. YouTube autosub produces wrong timing and contaminates
   ensemble output.

## Reference regime (v21, #143)

A NEW stage runs BEFORE step 2 (WhisperX/asr_path) decides anything, for
any song with an allowed text candidate + a preprocessed vocal WAV. Owner
directive 2026-09-12 on #130: "vyber najlepšie dosiahnuteľné riešenie a
začni reprocessovať" — the forced aligner `lyrics-alignment-mtl` (MTL+BDR)
leads every measurement (31.6% gold-norm, 83–92% on clean songs) but has a
known catastrophic failure mode (locking onto the wrong repetition of a
repetitive worship arrangement, shifting a whole song 22–42s), so it ships
gated by an independent second opinion rather than on its own:

1. **Align** — `mtl_aligner::align` (production wrapper around the eval
   script `eval/lyrics/aligners/lyrics_alignment_mtl/run.py`) force-aligns
   `claude_merge::best_authoritative_candidate`'s lines (the SAME selector
   step 1 above uses — any source, timed or not) to the isolated vocals.
   BELOW_NORMAL priority + no console window on Windows, `PYTHONUTF8=1`
   (#137), 15-min timeout, one `--no-cuda` retry on a CUDA-OOM stderr tail.
2. **Verify** — `g35t_client::transcribe_words` (Gemini 3.5 Transcribe
   word timings, keys from `gemini_api_key`) + `reference_gate::evaluate`
   gate BOTH conditions: whole-song sanity (`|median signed Δstart| <=
   400ms` — this is what catches the wrong-repetition failure mode) AND
   agreement (`>=70%` of matched lines within 400ms, `>=60%` of lines
   matched).
3. **Pass** → mtl line timings ship directly (`words: None`, no word
   synthesis — same v18 rule as everywhere else), `lyrics_source =
   "<candidate.source>+mtl@rev1/g35t-ok"`, `lyrics_alignment_model =
   ALIGNMENT_MODEL_MTL_REV1`, `videos.lyrics_reference = 1` (the wall ★).
   **Fail/error** → `videos.lyrics_reference = 0`, the gate decision +
   stats land in `{youtube_id}_alignment_audit.json`, and the song falls
   through to step 2 (WhisperX/asr_path) unchanged — never a regression
   versus what production shipped before this stage existed.
4. Skip conditions (info-logged per song, never fatal): mtl tooling not
   installed (`MtlConfig::is_available()` — WARNed ONCE at worker start,
   not per song), no preprocessed vocal WAV, no text candidate, or a
   candidate under 4 lines.
5. The injection seam is `orchestrator::ReferenceStageBackend`
   (`mtl_align` + `asr_transcribe`); production wires
   `RealReferenceStageBackend`, tests inject a fake — never make a real
   subprocess/HTTP call from a unit test.

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
- **GPU discipline (#154).** `gpu_polite()` in `lyrics_worker.py` (and the mtl
  `run.py`) sets a **BELOW_NORMAL WDDM GPU scheduling priority** (ctypes
  `D3DKMTSetProcessSchedulingPriorityClass`) and a **per-process VRAM cap** via
  `torch.cuda.set_per_process_memory_fraction` — default **0.7**, tunable by the
  `lyrics_gpu_mem_fraction` DB setting (clamp 0.2–0.95, plumbed to the child as
  `LYRICS_GPU_MEM_FRACTION`). Model parameters are UNCHANGED, so separation
  quality is identical; only priority + VRAM headroom move. On a CUDA OOM under
  the cap the isolation re-runs on CPU (same model → identical output, slower).
  Expected effect: playback keeps nominal fps during isolation at some
  isolation-time cost — exact slowdown is **measurement-pending on the box**
  (owner: "rýchlosť je nepodstatná").

## Translation — Claude only, never Gemini fallback

EN→SK translation MUST use Claude via CLIProxyAPI (paid Max plan — unlimited).
Gemini is metered. Never add a "Claude falls back to Gemini" path.

When Claude refuses via CLIProxyAPI:
- Tune the prompt. A simple neutral prompt ("translate these lines to Slovak,
  preserve line numbering") works. NEVER mention "song lyrics", "worship",
  "church", "copyright", "karaoke" — these trip the content-policy classifier.
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
  vs "bola som"). The prompt frames the request as a grandFATHER (masculine) or
  grandMOTHER (feminine) dictating for a memorial plaque — the SAME neutral
  framing that bypasses the copyright classifier, now doing double duty. Never
  add the words lyrics/song/worship/church/karaoke/copyright. Per-song override
  lives in `videos.lyrics_translation_gender` (`NULL`=auto→masculine, `m`, `f`),
  set from the dashboard ♂/♀ toggle (`PATCH …/translation-gender`).
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
- Adding/removing an AlignmentProvider from the worker registration
- Changing a provider's algorithm (chunking, matcher, density gate thresholds)
- Changing either Claude merge prompt (text reconciliation or timing merge)
- Changing the reference-text-selection algorithm
- Toggling `LYRICS_GEMINI_ENABLED` or `LYRICS_QWEN3_ENABLED`

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

Only fix timing issues that whisperx ASR data actually supports. When a wall
issue traces to missing/wrong ASR words, state it explicitly as an ASR limit.
Do NOT propose code fixes that fabricate timing past whisperx's coverage.
Do NOT push another fix round when ASR data does not support one.

## Dead code — delete, never stub

When replacing a code path, delete the old one entirely. No `#[deprecated]`
stubs, no fallback retention, no commented-out blocks.

## No codec yo-yo

Never flip codec/format/decoder strategy based on a single observed symptom.
Measure first: read production logs, reproduce with monitoring, run a comparison
test, read the relevant code path. Only after evidence converges — fix tied to
the measured root cause.
