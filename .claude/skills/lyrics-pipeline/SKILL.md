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

## Provider hierarchy (current production state)

1. **yt_subs** — registered FIRST as `AlignmentProvider`; short-circuits when
   `has_timing=true`. Ground truth — never judge or override with ASR output.
2. **Gemini 3.x Pro (gemini-3.1-pro-preview or newer)** — sole alignment
   provider after yt_subs. Never downgrade to 2.5-pro, Flash, or any older
   model even when preview is capacity-exhausted — wait.
3. **AutoSubProvider** — PERMANENTLY UNREGISTERED. Never register again, no
   exceptions. YouTube autosub produces wrong timing and contaminates ensemble
   output.

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

## Translation — Claude only, never Gemini fallback

EN→SK translation MUST use Claude via CLIProxyAPI (paid Max plan — unlimited).
Gemini is metered. Never add a "Claude falls back to Gemini" path.

When Claude refuses via CLIProxyAPI:
- Tune the prompt. A simple neutral prompt ("translate these lines to Slovak,
  preserve line numbering") works. NEVER mention "song lyrics", "worship",
  "church", "copyright", "karaoke" — these trip the content-policy classifier.
- Model: use `claude-opus-4-20250514` (not the short-form `claude-opus-4-6`
  which returns empty via OAuth). Always pass `max_tokens: 32000` for large
  responses.
- If a specific song still refuses after prompt tuning: surface it to the user.
  Do NOT auto-fallback.

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
