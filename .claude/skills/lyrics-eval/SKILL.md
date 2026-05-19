---
name: lyrics-eval
description: Score a lyrics-alignment backend against the pinned fixture set in eval/lyrics/manifest.json. Claude orchestrates: picks fixtures, runs audio prep + backend on win-resolume, judges output, writes a report committed to the repo. Use when evaluating a new backend or re-running an existing one after a prompt tune.
user-invocable: true
disable-model-invocation: true
---

# Lyrics Eval

**Single entry point for measuring lyrics-alignment backend quality.** The user
invokes `/lyrics-eval`; Claude drives every step. The user observes and can
intervene at any per-fixture step.

This skill follows the design in
`docs/superpowers/specs/2026-05-18-lyrics-eval-harness-design.md` and the
memories under "Lyrics eval is Claude-orchestrated" and "Eval = Python, not
Rust" in `~/.claude/projects/-home-newlevel-devel-songplayer/memory/MEMORY.md`.
Re-read those before any structural change to this skill.

## Phase 0 — Preconditions

1. **Event status is user-authoritative.** Ask the user whether a live event
   is currently running on win-resolume. Never infer from OBS scene state. If
   an event is running → STOP and report. Per
   `feedback_event_status_user_authoritative.md` +
   `feedback_winresolume_is_shared_event_machine.md`.
2. **win-resolume reachable.** `mcp__win-resolume__Ping` returns success. If
   not → STOP and report.
3. **SongPlayer HTTP API reachable.** `curl -m 5 http://10.77.9.201:8920/api/v1/status` returns 200.
4. **REPLICATE_API_TOKEN set** in the win-resolume environment (or the local
   shell, if the backend caller is being invoked from dev). If missing → STOP
   and ask the user.

## Phase 1 — Configure the run

Ask the user (one `AskUserQuestion`):

- **Backend** to test (default `whisperx-large-v3`; the production champion).
- **Fixtures** subset: `all`, a category name, or a comma-separated list of
  `video_id`s.

Load `eval/lyrics/manifest.json`. Validate against
`eval/lyrics/schemas/manifest.schema.json`. If invalid → STOP and report.

Load the current `eval/lyrics/reports/CHAMPION.md` to know what to diff against
at the end.

- **Backend-id ↔ filename convention:** the file at `eval/lyrics/backends/X.py` exposes `BACKEND_ID = "X-with-underscores-replaced-by-hyphens"` (e.g. `whisperx_large_v3.py` → `BACKEND_ID = "whisperx-large-v3"`). Filenames use underscores so Python can import them; report labels use hyphens because that is what is persisted in CHAMPION.md and history.md.

## Phase 2 — Per-fixture loop

For each selected fixture, in manifest order:

1. **Audio prep.** On win-resolume:
   ```
   mcp__win-resolume__Shell:
       python <repo>\eval\lyrics\audio_prep.py --video-id <id>
   ```
   Captures the printed WAV path. If exit code is non-zero, report the failure
   and ask the user whether to skip this fixture or stop the run.
2. **Backend call.** On win-resolume:
   ```
   mcp__win-resolume__Shell:
       python <repo>\eval\lyrics\backends\<backend_id_with_hyphens_to_underscores>.py \
           --wav <wav_path> \
           --out C:\ProgramData\SongPlayer\eval-cache\<id>_result.json
   ```
   On failure: if Claude has reason to believe the backend prompt could be
   tuned (e.g. timeouts on dense chorus, repetition loops, malformed JSON),
   propose a concrete edit to `eval/lyrics/backends/<backend_id_with_hyphens_to_underscores>.py`, apply
   it, and retry once. Per `feedback_eval_iteration_python_not_rust.md`, Claude
   has free hands here.
3. **Judge.** Read the result JSON + the fixture's `gold_lines`. Apply the
   prompt template in `eval/lyrics/judge_prompt.md`. Emit a JSON judgment
   matching `eval/lyrics/schemas/judgment.schema.json`. Validate the JSON
   shape before continuing.
4. **Print one-liner** to the user:
   `[7/30] BW_vUblj_RA  dense_vocal  score=7  verdict=partial  wall_acceptable=false`
   so they can interrupt if something looks wrong.

## Phase 3 — Aggregate + write report

After all selected fixtures judged:

- Compute `mean_score`, `median_score`, `scores_by_category`,
  `fixtures_passed_wall`.
- Build the per-run report JSON matching
  `eval/lyrics/schemas/report.schema.json`. Validate the JSON shape before
  writing — assert `fixtures_run == len(per_fixture)` and
  `fixtures_passed_wall <= fixtures_run`.
- Write `eval/lyrics/reports/<YYYY-MM-DD>-<backend_id>.json` AND a human
  Markdown twin `eval/lyrics/reports/<YYYY-MM-DD>-<backend_id>.md` with:
  - Header (run_id, backend, judge model, prompt revision)
  - Aggregate table
  - Per-category score table
  - Per-fixture single-row table (video_id | category | score | verdict |
    wall_acceptable | one-line reasoning)
- Append one row to `eval/lyrics/reports/history.md`.

## Phase 4 — Diff vs champion + recommend

If the tested backend is the current champion: report aggregate, ask the user
whether to update CHAMPION.md's `Score:` line (initial baseline case).

If the tested backend is NOT the current champion: load the champion's most
recent report from `history.md`. Diff mean score, median score,
wall-pass count, and per-category breakdown. Recommend one of:

- **Promote** — composite improvement across all critical categories, no
  regression on any.
- **Reject** — net regression or critical-category regression.
- **Partial promote** — wins decisively on a specific category (e.g.
  `asr_gap`-adjacent songs) but loses on others. Suggest routing the new
  backend ONLY for that category in future production work (this becomes
  input to issue #112's routing decision).

Present the recommendation to the user. NEVER edit CHAMPION.md without
explicit user "promote" approval.

## Phase 5 — Commit

After the user approves the report:

```bash
git add eval/lyrics/reports/<YYYY-MM-DD>-<backend_id>.{json,md} \
        eval/lyrics/reports/history.md \
        eval/lyrics/reports/CHAMPION.md
git commit -m "eval(lyrics): run <backend_id> rev <N> — mean <X>"
```

Push only after running the pre-push checks:

```bash
python3 -m ruff check eval/
python3 -m ruff format --check eval/
python3 -m pytest eval/lyrics/tests -p no:html
```

If any fail → fix and re-commit. Never push a broken eval/.

## Iron rules

- **Win-resolume idle gate.** Live event running → STOP. Always.
- **Free hands on prompts, not on schemas.** Claude may edit
  `backends/<id>.py` mid-run to tune prompts. Claude may NOT change the
  output JSON shape, the manifest schema, the report schema, or the judgment
  schema during a run — those changes require a separate PR.
- **No mechanical fallback scoring.** If Claude cannot judge a fixture (e.g.
  candidate JSON is malformed), the fixture is marked `verdict=miss`,
  `wall_acceptable=false`, `score=0`, with the malformed-output snippet in
  `reasoning`. Do not invent a fake score.
- **No catalog reprocess from this skill.** The skill does NOT trigger
  reprocess of any production songs. It runs only against the manifest
  fixtures on win-resolume's eval-cache. Production songs are untouched.
- **No pipeline-version bump from this skill.** Per
  `feedback_no_bump_until_proven.md`.
- **No backend promotion without user approval.** Per
  `pr-merge-policy.md` + `approval-scope.md`.

## When the user corrects you

If the user corrects Claude mid-run (e.g. "that hallucination call is wrong,
those are valid backing vocals"), ADD the correction as a new bullet under
"Iron rules" in THIS file at the end of the session. The skill grows with
experience, same pattern as `lyrics-verify`.
