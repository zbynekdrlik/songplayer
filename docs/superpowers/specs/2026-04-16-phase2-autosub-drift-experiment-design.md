# Phase 2 — Auto-Sub Drift Experiment

**Status:** Design approved 2026-04-16
**Parent issue:** #29 (Phase 2: lyrics from video description + timing from YT auto-subs)
**Scope:** Validation experiment only. Phase 2 pipeline design is a separate brainstorm, gated on this experiment's outcome.

## Goal

Decide whether YouTube auto-subtitles carry word-level timestamps accurate enough on *sung* vocals to replace Qwen3-ForcedAligner as the timing source. Produce a committed markdown report with per-song drift statistics and a go/no-go recommendation.

## Decision rule

Per-song RMS drift between auto-sub word starts and the current Qwen3 word starts maps to one of three buckets:

| RMS drift   | Bucket  | Action                                                |
| ----------- | ------- | ----------------------------------------------------- |
| < 300 ms    | Green   | Greenlight Phase 2 design with auto-sub timing as-is  |
| 300–700 ms  | Amber   | Phase 2 viable but needs a refinement pass            |
| > 700 ms    | Red     | Premise fails on worship music; close #29             |

The recommendation in the report is the worst per-song bucket across all three test songs (one Red song kills the project; one Amber song downgrades the whole recommendation).

## Test corpus

- #148 — Planetshakers, "Get This Party Started" (live worship — heavy reverb, long-line repetition)
- #181 — planetboom, representative track from the issue
- #73 — Elevation Worship, representative track from the issue

If a song lacks YT auto-subs or a Qwen3 reference in the win-resolume DB, report it as missing data and continue with the remaining songs. A substitute song may be picked at execution time if needed (decision recorded in the report).

## Pipeline

### 1. Data collection

**Auto-subs** (dev machine):
```
yt-dlp \
  --write-auto-subs \
  --sub-format json3 \
  --sub-langs en \
  --skip-download \
  --output '<tmp>/<id>.%(ext)s' \
  https://www.youtube.com/watch?v=<video_id>
```
Output: `<id>.en.json3` with word-level `wStart` ms timings inside event segments.

**Qwen3 reference** (read-only pull from win-resolume):
- SCP the lyrics SQLite database file from `C:\ProgramData\SongPlayer\` to a local tmp dir
- Query the local copy for each test video ID's word-level alignment
- No remote write, no service interruption, no manual edit on the production database

### 2. Word matcher (Option A — exact text + sequential position)

For each video:

1. Read both word streams as `[(text, start_ms)]` lists.
2. Normalize both:
   - Lowercase
   - Strip punctuation (`.,!?;:'"`)
   - Drop tokens matching `[music]`, `>>`, empty strings, and any non-alphanumeric noise
3. Sequential forward walk:
   - Iterate Qwen3 words in order.
   - For each Qwen3 word, advance the auto-sub pointer up to **N=10 words ahead** searching for the first exact-text match.
   - On match: record `drift = auto_start_ms - qwen_start_ms`, advance auto-sub pointer past the matched word, increment matched counter.
   - On miss: increment skipped counter, do not advance auto-sub pointer.
4. No backtracking. No fuzzy match. No reverse search.

### 3. Per-song statistics

- `total_qwen_words`, `total_autosub_words`
- `matched_words`, `match_rate = matched / total_qwen_words`
- Drift distribution: `rms`, `mean`, `median`, `max`, `min`, `p95`, `p05`
- ASCII histogram bucketed at ms boundaries: `[-2000, -1000, -500, -300, -100, 0, +100, +300, +500, +1000, +2000]`

### 4. Report

File: `docs/experiments/2026-04-16-autosub-drift.md`

Required sections:

- **Methodology** — matching algorithm, normalization rules, exclusion list, command lines used
- **Per-song results** — one section per song with video URL, title, artist, full statistics block, ASCII histogram
- **Conclusion table** — three rows, one per song, showing RMS drift and assigned decision bucket
- **Recommendation** — single paragraph: greenlight / refine / kill, grounded in the worst-case song's bucket, citing specific numbers
- **Raw data references** — paths to the temporary auto-sub json3 files (kept in tmp during PR review, not committed)

## Files

| Path                                                        | Purpose                                      |
| ----------------------------------------------------------- | -------------------------------------------- |
| `scripts/experiments/autosub_drift.py`                      | One-shot script (throwaway, kept for repro)  |
| `docs/experiments/2026-04-16-autosub-drift.md`              | Durable report — the actual deliverable     |

No production Rust or Python code is touched. No DB schema changes. No CI pipeline changes.

## Acceptance criteria

A PR is mergeable when all of the following hold:

1. `scripts/experiments/autosub_drift.py` exists and runs end-to-end on the dev machine without manual editing of the script body.
2. `docs/experiments/2026-04-16-autosub-drift.md` exists and contains:
   - Methodology section
   - One results section per test song (or an explicit "no data" section for any missing song)
   - Conclusion table with bucket assignment per song
   - Recommendation paragraph
3. The report's recommendation is supported by the cited numbers (a reviewer can read the histogram and agree with the bucket).
4. CI stays green: `cargo fmt --all --check`, all existing tests, all existing E2E.

## Failure handling

| Failure                          | Behaviour                                                                |
| -------------------------------- | ------------------------------------------------------------------------ |
| yt-dlp: no auto-subs for video   | Skip song, write `no auto-subs available` block, continue                |
| DB has no Qwen3 reference        | Skip song, write `no Qwen3 reference in DB` block, continue              |
| Empty intersection after norm    | Report `match_rate: 0%`, treat as a finding (not a crash)                |
| yt-dlp or SCP failure            | Script exits non-zero with clear error; no partial report committed      |
| `wStart` missing on a json3 event | Fall back to event-level `tStartMs` for that segment, note in report     |

## Verification

The script has no unit tests — it is single-use.

Verification = reviewer reads the produced report, sanity-checks the numbers against the histogram, agrees with the bucket assignments. The PR description must include the report's recommendation paragraph inline so reviewers see it without opening the file.

## Out of scope

Explicitly deferred to a separate Phase 2 brainstorm, gated on this experiment's outcome:

- Description-lyrics extraction (HTML / API / yt-dlp `--write-description` parsing, Gemini prompt design)
- LLM-based lyric-to-ASR matching algorithm
- Refinement-pass design (sliding-window correction, DTW, etc.)
- Per-song decision tree integration in `lyrics/worker.rs`
- E2E coverage for any new pipeline
- Promotion of the throwaway script to a reusable harness

## Risks

1. **Auto-subs may be missing on one or more test songs.** Likelihood: low — YouTube auto-generates English captions for almost all music videos. Mitigation: report "no data" for that song and either accept N=2 or pick a substitute.
2. **`wStart` may be absent on certain events.** json3 sometimes emits sentence-level events without per-word timestamps. Mitigation: documented fallback to event-level start with a noted confidence reduction in the report.
3. **Win-resolume DB layout drift.** If the Phase 1 schema for word-level alignment changes between now and execution, the Qwen3 query needs updating. Mitigation: at execution time, inspect the live DB schema first and adapt the query before running the full experiment.
