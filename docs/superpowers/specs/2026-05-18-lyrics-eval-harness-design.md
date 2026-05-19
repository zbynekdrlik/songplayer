# Lyrics Eval Harness Design

**Status:** Brainstormed 2026-05-18, ready for implementation planning.
**Related issues:**
- [#110 Lyrics SOTA: eval harness + monthly /lyrics-research command](https://github.com/zbynekdrlik/songplayer/issues/110) — this spec
- [#111 Lyrics SOTA: backend zoo + transport adapters](https://github.com/zbynekdrlik/songplayer/issues/111) — depends on this spec
- [#112 Lyrics SOTA: no-text-source ASR path](https://github.com/zbynekdrlik/songplayer/issues/112) — depends on #110 + #111

**Related feedback memories:**
- `feedback_lyrics_eval_claude_orchestrated.md` — eval is Claude-orchestrated, not mechanical CI
- `feedback_eval_iteration_python_not_rust.md` — eval iteration in Python, not Rust
- `feedback_repo_scoped_skills.md` — project-specific skills live in repo, not user-wide
- `feedback_canonical_source_regression_ci.md`, `feedback_wall_verification_only.md`, `feedback_no_bump_until_proven.md`, `feedback_line_timing_only.md`, `feedback_winresolume_is_shared_event_machine.md`, `feedback_pipeline_version_approval.md`

---

## 1. Goal

`/lyrics-eval` is a Claude-orchestrated slash command that scores a given lyrics-alignment backend against a fixed fixture set of songs with known-good lyrics. Claude drives every step: picks fixtures, fetches audio, calls the backend (Python), inspects output, judges quality (Claude-as-judge, 0–10 score per fixture), aggregates, and writes a report committed to the repo. The harness lets the user objectively decide which model deserves promotion to production.

Without this harness, every backend swap is anecdotal and risks regressing the catalog. With it, backend exploration becomes data-driven: register a new candidate → run eval → see score → promote or shelve.

## 2. Non-goals

- Adding new backends beyond the current production `whisperx-large-v3` — that is issue #111.
- Routing logic that selects a backend per song in the production pipeline — that is issue #111 + #112.
- Catalog reprocess of any kind — no `LYRICS_PIPELINE_VERSION` bump as part of this PR.
- A CI cron / GH Actions workflow that runs the eval automatically. Eval is interactive, orchestrated by Claude.
- A trait-locked Rust registry of candidate backends. Candidates are throwaway Python files until eval-proven; only the winner gets a Rust impl (in #111).

## 3. Architecture

### 3.1 Repo layout

```
songplayer/
├── .claude/
│   └── skills/
│       └── lyrics-eval/
│           └── SKILL.md                    # repo-scoped slash command
├── eval/
│   └── lyrics/
│       ├── README.md
│       ├── manifest.json                   # fixture list, committed
│       ├── build_manifest.py               # one-shot DB → manifest helper
│       ├── audio_prep.py                   # vocal stem prep on win-resolume
│       ├── backends/
│       │   └── whisperx_replicate.py       # baseline backend caller (PR1)
│       ├── judge_prompt.md                 # versioned judge prompt
│       └── reports/
│           ├── 2026-05-18-whisperx.json    # first run, committed
│           ├── 2026-05-18-whisperx.md      # human-readable summary
│           ├── history.md                  # append-only one-line summary log
│           └── CHAMPION.md                 # current production champion
```

### 3.2 Component summary

- **Skill** (`.claude/skills/lyrics-eval/SKILL.md`) — instructions Claude follows when the user invokes `/lyrics-eval`. Lays out preconditions, the per-fixture flow, the judge prompt template, and the report contract.
- **Fixture manifest** (`manifest.json`) — pinned list of ~30 songs with gold lyrics + line timings, organised by category.
- **Audio prep** (`audio_prep.py`) — yt-dlp + Mel-Roformer + anvuew dereverb, cached on win-resolume.
- **Backend caller** (`backends/<backend_id_with_hyphens_to_underscores>.py`) — one standalone Python file per backend; reads vocal WAV path, calls the model, writes candidate output JSON. Claude can edit the file mid-eval to tune prompts.
- **Judge** — Claude itself, applied via the versioned judge prompt in `judge_prompt.md`.
- **Report writer** — Claude writes per-run JSON + Markdown twin, appends one row to `history.md`, and rewrites `CHAMPION.md` when the user approves a promotion.

### 3.3 Why these boundaries

- Python rather than Rust because every published model ships Python examples / SDKs; adding a new candidate is ~10 lines of throwaway code Claude can write or edit in one turn (`feedback_eval_iteration_python_not_rust.md`).
- Skill in repo rather than user-wide because the skill has no meaning in the user's other projects (`feedback_repo_scoped_skills.md`).
- Claude-as-judge rather than mechanical WER/timing libs because (a) it works on the fixtures we do not have hand-clean gold for and (b) it captures wall-acceptable as the primary signal, not a numerical proxy.
- Reports committed to repo so history survives win-resolume rebuilds and diffs are auditable via `git log`.

## 4. Skill flow

When the user types `/lyrics-eval`, Claude follows `SKILL.md`:

1. **Preconditions.** Win-resolume reachable + idle (no live event per user statement — never inferred from OBS scene per `feedback_event_status_user_authoritative.md`; never saturate during live playback per `feedback_winresolume_is_shared_event_machine.md`).
2. **Ask user** which backend to test and which fixture subset (`all`, a category, or specific `video_id`s).
3. **Load** the manifest + the prior champion report for diff context.
4. **For each fixture:**
   1. `python eval/lyrics/audio_prep.py --video-id <id>` → cached vocal-stem WAV path on win-resolume.
   2. `python eval/lyrics/backends/<backend_id_with_hyphens_to_underscores>.py --wav <path> --out <result.json>` → candidate output JSON. If output looks off, Claude is free to edit the backend file (system message, retry rule, chunk size), re-run on the failing fixture, and continue.
   3. Claude reads the candidate JSON + the gold lines/timings from the manifest, applies the `judge_prompt.md` template, and emits a structured judgment (score 0–10, verdict, hallucination details, wall_acceptable boolean, reasoning).
   4. Print a per-fixture one-liner so the user can intervene if a judgment looks wrong.
5. **Aggregate** per-fixture judgments → mean score, median score, scores-by-category, wall-pass count.
6. **Write** `reports/<ts>-<backend>.json` + `.md`, append one row to `history.md`.
7. **Diff vs champion** if the tested backend is not the current champion. Recommend promote / reject / partial-promote.
8. **On promote** (only after explicit user approval): rewrite `CHAMPION.md`.

## 5. Fixture manifest

### 5.1 Schema

```json
{
  "version": 1,
  "fixtures": [
    {
      "video_id": "BW_vUblj_RA",
      "category": "dense_vocal",
      "gold_source": "lrclib_synced",
      "gold_lines": [
        {"text": "I'm not a sinner, I'm a saint", "start_ms": 25800, "end_ms": 27500}
      ],
      "notes": "Live recording, light reverb"
    }
  ]
}
```

### 5.2 Selection criteria

- ~5 fixtures per category, total ~30.
- Categories: `dense_vocal`, `reverb_heavy`, `instrumental_breaks`, `multi_language`, `clean_pop`, `chant_repetition`.
- Every fixture must have `gold_source ∈ {lrclib_synced, spotify_proxy, yt_subs_manual}`. Songs with `description`-only sources, autosub sources, or `asr_gap` quarantine status are not eligible — there is no line-synced gold for them.
- The 5 chosen songs per category should span easy and hard within the category, picked by the user during the initial manifest build.

### 5.3 Initial build

`python eval/lyrics/build_manifest.py` is a one-shot helper. It queries the production SongPlayer HTTP API on win-resolume (`http://10.77.9.201:8920/api/v1/lyrics/probe-sources` + the existing songs listing endpoint), filters songs by `lyrics_source ∈ {lrclib_synced, spotify_proxy, yt_subs_manual}`, presents the candidates to the user for category bucketing + final pick, writes `manifest.json`. HTTP rather than direct DB access keeps the helper portable (no Python sqlite binding against a remote-FS DB file) and avoids any chance of concurrent writes from the live worker. Run once during PR1. The resulting `manifest.json` is committed and treated as stable thereafter — modifications are explicit PRs of their own.

## 6. Audio prep (`eval/lyrics/audio_prep.py`)

Single-file Python script invoked with `--video-id <id>` (and optional `--force`).

Flow:
1. Check the eval vocal cache at `C:\ProgramData\SongPlayer\eval-cache\<video_id>_vocal16k.wav` on win-resolume. If present and `--force` is not set, print the path and exit zero.
2. Otherwise, download the audio with yt-dlp (audio-only, smallest acceptable format), then invoke the `audio_separator` Python package directly from the existing lyrics-bootstrap venv on win-resolume — same package + same `dereverb_mel_band_roformer_anvuew_sdr_19.1729.ckpt` checkpoint the production `crates/sp-server/src/lyrics/aligner.rs::preprocess_vocals` uses, just called from Python rather than through the Rust shim. Write the dereverbed 16 kHz mono WAV into the eval cache. Print the path.

Path always returned as a win-resolume FS absolute path. Subsequent backend calls run on win-resolume (via `mcp__win-resolume__Shell`), so no file ever needs to cross the network.

`C:\ProgramData\SongPlayer\eval-cache\` is created on first run. Not committed to the repo; the manifest is the source of truth and the cache is rebuildable from yt-dlp.

## 7. Backend caller (`eval/lyrics/backends/<backend_id_with_hyphens_to_underscores>.py`)

One Python file per backend. PR1 ships exactly one: `whisperx_replicate.py`.

Each file is a standalone runnable:

```
python eval/lyrics/backends/whisperx_replicate.py \
    --wav 'C:\ProgramData\SongPlayer\eval-cache\BW_vUblj_RA_vocal16k.wav' \
    --out /tmp/result.json
```

It reads its API token from an env var (`REPLICATE_API_TOKEN` for the baseline), calls the model, and writes JSON to `--out`:

```json
{
  "backend_id": "whisperx-large-v3",
  "backend_revision": 1,
  "wav_path": "C:\\...\\BW_vUblj_RA_vocal16k.wav",
  "duration_ms": 219000,
  "lines": [
    {
      "text": "I'm not a sinner, I'm a saint",
      "start_ms": 25800,
      "end_ms": 27500,
      "words": null
    }
  ],
  "raw_confidence": 0.87,
  "metadata": {
    "model": "victor-upmeet/whisperx-a40-large",
    "model_revision": "...",
    "elapsed_s": 42.1
  }
}
```

The JSON contract is what the judge consumes. `words` is allowed to be `null` per `feedback_line_timing_only.md` and `feedback_no_even_distribution.md` — line timing is the focus, no synthesized word timings.

Per `feedback_eval_iteration_python_not_rust.md`, Claude is free to edit this file mid-eval to tune the call (chunking, system prompt, retry rules, prompt hints). When a tuned prompt produces materially better output, the edit is committed; subsequent runs reproduce the new behaviour. The file is not under unit-test coverage (it is throwaway iteration code).

When a new candidate backend is added (under issue #111), it appears as a sibling file under `backends/` following the same I/O contract. No Rust touched until that candidate wins eval and is promoted.

## 8. Scoring — Claude as judge

There is no mechanical WER or line-offset library. Per fixture, Claude reads:

- The candidate output JSON.
- The gold `lines` + line timings from the manifest.
- Optional context from audio_prep (duration, language).

Claude applies the prompt in `eval/lyrics/judge_prompt.md` and emits a structured judgment:

```json
{
  "video_id": "BW_vUblj_RA",
  "score": 7,
  "verdict": "partial",
  "wer_estimate": 0.18,
  "line_timing_assessment": "median offset ~120 ms, two lines start ~1 s late",
  "hallucination_count": 1,
  "hallucination_details": "Lines 12-23 are duplicate 'What's up?' cluster — model loop",
  "coverage_pct": 0.85,
  "wall_acceptable": false,
  "reasoning": "Most lines match gold within timing tolerance; the hallucination cluster in the bridge would show on the wall as 12 wrong lines. Reject for production until prompt tuned.",
  "judged_at": "2026-05-18T14:23:00Z",
  "judge_model": "claude-opus-4-7",
  "judge_prompt_revision": 1
}
```

`wall_acceptable` is the primary signal: would this output be acceptable on the live LED wall during a service? Mechanical metrics (`wer_estimate`, `coverage_pct`) are diagnostic, not gating.

### 8.1 Reproducibility

- The judge prompt lives in `judge_prompt.md` and is versioned alongside the spec. Edits to the prompt produce a new `prompt_revision` recorded in each per-fixture judgment.
- Each judgment records `judge_model` (e.g. `claude-opus-4-7`) and `judged_at` so future drift can be diagnosed.
- Re-running an eval with the same backend + same prompt revision + same judge model should produce near-identical scores. Small variance is accepted.

## 9. Reports

### 9.1 Per-run JSON

`eval/lyrics/reports/<ts>-<backend>.json`:

```json
{
  "run_id": "2026-05-18T14:30:00Z",
  "backend_id": "whisperx-large-v3",
  "backend_revision": 1,
  "judge_model": "claude-opus-4-7",
  "judge_prompt_revision": 1,
  "fixtures_run": 30,
  "fixtures_passed_wall": 22,
  "aggregate": {
    "mean_score": 7.3,
    "median_score": 8,
    "scores_by_category": {
      "dense_vocal": 6.4,
      "reverb_heavy": 5.9,
      "instrumental_breaks": 7.1,
      "multi_language": 6.2,
      "clean_pop": 8.8,
      "chant_repetition": 7.4
    }
  },
  "per_fixture": [ /* the §8 per-fixture judgment for each fixture */ ]
}
```

### 9.2 Markdown twin

`reports/<ts>-<backend>.md` — human-readable summary: aggregate header, scores-by-category table, per-fixture single-row table with verdict + wall_acceptable + score.

### 9.3 `history.md`

Append-only one-line log:

```
| 2026-05-18 | whisperx-large-v3 | r1 | mean 7.3 | wall-pass 22/30 | champion |
| 2026-05-25 | gemini-3-pro      | r1 | mean 8.1 | wall-pass 26/30 | candidate |
| 2026-05-26 | gemini-3-pro      | r2 | mean 8.4 | wall-pass 28/30 | promoted |
```

### 9.4 `CHAMPION.md`

Short file rewritten only when a backend is promoted. Records: current backend id, revision, score, run id, link to the detail report, date promoted. Nothing more.

## 10. PR1 deliverables

In scope for the first PR that closes #110:

- `.claude/skills/lyrics-eval/SKILL.md` — the full slash-command spec.
- `eval/lyrics/manifest.json` — built once from the production DB, committed.
- `eval/lyrics/build_manifest.py` — the one-shot DB-to-manifest helper.
- `eval/lyrics/audio_prep.py` — vocal stem prep.
- `eval/lyrics/backends/whisperx_replicate.py` — the baseline backend caller.
- `eval/lyrics/judge_prompt.md` — the versioned judge prompt.
- `eval/lyrics/reports/2026-05-18-whisperx.json` + `.md` — the first eval run, committed.
- `eval/lyrics/reports/history.md` — initial row.
- `eval/lyrics/reports/CHAMPION.md` — initial champion = `whisperx-large-v3` @ rev 1.
- `eval/lyrics/README.md` — brief orientation for future contributors.

Out of scope (filed elsewhere):

- Any additional backend caller (Gemini, Qwen, future SOTA) → issue #111.
- Routing the no-text-source path through a raw-ASR backend → issue #112.
- `/lyrics-research` web-search command. Deferred to a follow-up issue after PR1 lands — keeps PR1 focused on the scoring rig.
- Any CI cron / scheduled run / GH Action workflow → explicitly rejected per `feedback_lyrics_eval_claude_orchestrated.md`.

## 11. Testing

- **Schema tests:** `eval/lyrics/manifest.json` and the per-run report JSON each have a small `pytest` schema validator. CI runs `pytest eval/` as a new step.
- **Lint:** `ruff check eval/` added to existing CI lint step.
- **End-to-end verification:** the user invokes `/lyrics-eval --backend whisperx-large-v3 --fixtures all` once on PR1. The resulting committed `reports/2026-05-18-whisperx.json` IS the verification artifact — it proves the full skill flow works on real data.
- **No mocked unit tests** for `whisperx_replicate.py`. The file is throwaway-iteration code per `feedback_eval_iteration_python_not_rust.md`. Adding mocks would freeze it and defeat the purpose.

## 12. Risks + mitigations

- **Judge drift across Claude model versions.** Mitigation: every judgment records `judge_model`. If a future Claude version produces materially different scores on the same backend + same prompt revision, the diff is visible. A "rebaseline" run under a new judge model is then explicit.
- **Manifest staleness.** Songs in the manifest can be removed from the catalog or have their `lyrics_source` change. Mitigation: `build_manifest.py` reports staleness at the start of every eval run; user decides whether to refresh.
- **Win-resolume unavailable.** Eval cannot run without the GPU on the production machine. Mitigation: skill preconditions fail loud and stop; no fallback to dev-machine CPU-only path (would invalidate timing comparisons).
- **Throwaway backend file diverges from production.** Concern flagged for #111 — if `whisperx_replicate.py` here tunes a prompt that proves superior, it must be ported to the production Rust impl in `crates/sp-server/src/lyrics/`. Tracked as part of any future promote-decision PR.

## 13. Open items for the writing-plans phase

The following are open in the sense that the writing-plans skill should turn them into concrete RED-GREEN test steps and ordered work items, not in the sense of unresolved design decisions:

- Concrete category-by-category fixture pick (decided interactively during `build_manifest.py` first run).
- Exact text of `judge_prompt.md` v1 (drafted during PR1 implementation; refined by observation during the first real run).
- Exact CI step name + position for `pytest eval/` and `ruff check eval/`.

These open items are NOT additional design forks. They are implementation-time decisions the writing-plans skill turns into ordered steps.
