# Lyrics Eval Harness

Claude-orchestrated scoring rig for lyrics-alignment backends. Invoke via the
`/lyrics-eval` slash command.

See:

- `docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` — design
- `docs/superpowers/plans/2026-05-18-lyrics-eval-harness.md` — implementation plan
- `.claude/skills/lyrics-eval/SKILL.md` — slash-command instructions

## Directory contents

| Path | Purpose |
|------|---------|
| `manifest.json` | Pinned fixture set (24 songs with line-synced gold, ~4 per category). Rebuild via `build_manifest.py`. |
| `build_manifest.py` | One-shot helper to (re)build the manifest from the production SongPlayer HTTP API. |
| `audio_prep.py` | yt-dlp + Mel-Roformer + anvuew dereverb on win-resolume. Shells out to `scripts/lyrics_worker.py preprocess-vocals`. |
| `backends/` | One Python file per backend. Ships `whisperx_large_v3.py` (baseline / current champion), `gemini_3_1_flash_lite.py` (OpenRouter), and `assemblyai_universal_3_pro.py` (eval front-runner — 7.6 mean, 5/5 wall-pass on the 5-fixture pilot). |
| `judge_prompt.md` | Versioned judge prompt Claude follows when scoring fixtures. |
| `schemas/` | JSONSchemas (`manifest`, `report`, `judgment`). |
| `reports/` | Committed per-run reports + `CHAMPION.md` + `history.md`. |
| `tests/` | pytest suite covering schemas + helpers + backend parsers. |

## Adding a new backend

1. Drop `backends/<backend_id_with_hyphens_to_underscores>.py` (e.g.
   `backends/whisperx_large_v3.py` exposes `BACKEND_ID = "whisperx-large-v3"`)
   following the same CLI contract as the existing wrappers: `--wav` (input
   WAV path), `--out` (output JSON path), read API token from env. Emits the
   documented backend-output JSON shape (`{backend_id, backend_revision,
   wav_path, duration_ms, lines, raw_confidence, metadata}`).
2. Read the matching env var (see Phase 0 table in
   `.claude/skills/lyrics-eval/SKILL.md`); fail fast with a clear message if
   missing.
3. Optional unit test under `tests/test_<backend_id>_backend.py` exercising
   the per-vendor parser (line splitter, response decoder, etc.) — schema
   tests guard the output shape automatically.
4. Run `/lyrics-eval` against the new backend on the existing manifest.
5. Diff vs current `CHAMPION.md`. If it wins decisively, propose promotion in
   a separate PR that edits `CHAMPION.md` and (if the production pipeline is
   to be swapped) wires the backend into `crates/sp-server/src/lyrics/`.

Dead-end backends are dropped (wrapper deleted) but their `history.md` row
stays so future evals don't re-test them. Current dropped set:
`nemotron-3-nano-omni`, `mimo-v2-5`, `mimo-v2-omni`, `assemblyai-universal-2`.

## Local cache

`audio_prep.py` writes vocal stems to
`C:\ProgramData\SongPlayer\eval-cache\<video_id>_vocal16k.wav` on
win-resolume. This path is rebuildable; the cache dir is `.gitignore`d at
the repo root (`eval/lyrics/.eval-cache/`) so any local-dev mirror does not
pollute git.

The repo-relative `eval/lyrics/.eval-cache/` `.gitignore` entry guards a
local-dev mirror only — it does NOT guard `C:\ProgramData\SongPlayer\eval-cache\`
on win-resolume (that path is outside the repo tree, no gitignore needed).
If you run `audio_prep.py` locally with `--cache-dir`, use the dotted
`eval/lyrics/.eval-cache/` path so the entry matches.

## CI

The `eval-checks` GitHub Actions job lints (`ruff check`, `ruff format`) and
runs `pytest eval/lyrics/tests` on every push. The job is part of the gate's
required-checks list — green CI implies eval/ unit tests pass.
