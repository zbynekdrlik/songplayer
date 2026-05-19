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
| `backends/` | One Python file per backend. PR1 ships `whisperx_replicate.py` (baseline). |
| `judge_prompt.md` | Versioned judge prompt Claude follows when scoring fixtures. |
| `schemas/` | JSONSchemas (`manifest`, `report`, `judgment`). |
| `reports/` | Committed per-run reports + `CHAMPION.md` + `history.md`. |
| `tests/` | pytest suite covering schemas + helpers + backend parsers. |

## Adding a new backend

Out of scope for PR1; see issue [#111](https://github.com/zbynekdrlik/songplayer/issues/111).

When the time comes:

1. Drop `backends/<backend_id_with_hyphens_to_underscores>.py` (e.g. `backends/whisperx_large_v3.py` emits `BACKEND_ID = "whisperx-large-v3"`) following the same I/O contract as
   `whisperx_large_v3.py` (`--wav`, `--out`; writes the documented JSON shape).
2. Optional unit test under `tests/test_<backend_id>_backend.py`.
3. Run `/lyrics-eval` against the new backend on the existing manifest.
4. Diff vs champion. If it wins decisively, propose promotion in a separate PR
   that edits `CHAMPION.md`.

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
