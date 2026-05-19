# Lyrics Eval Judge Prompt — Revision 1

You are judging the quality of a lyrics-alignment backend's output against a
known-good gold reference. Your output is a single JSON object matching
`eval/lyrics/schemas/judgment.schema.json`. Do not write prose around the JSON.

## Inputs you receive

1. **Gold lines** — the reference lyrics with `text`, `start_ms`, `end_ms` per
   line. These come from a line-synced source the user already trusts
   (production `track.source` ∈ {`lrclib`, `spotify`, `yt_subs`} — see
   `crates/sp-server/src/lyrics/worker.rs:680`).
2. **Candidate output** — the backend's `lines` array, each with `text`,
   `start_ms`, `end_ms`, and optional per-word timings (which you ignore for
   this judgment per `feedback_line_timing_only.md`).
3. **Song metadata** — `video_id`, `duration_ms`, `category`, optional notes.

## What to judge

- **WER estimate (`wer_estimate`)**: rough word-error-rate of candidate text vs
  gold text, line by line. Float in [0.0, 1.0]. You estimate; this is not a
  mechanical metric.
- **Line-timing assessment (`line_timing_assessment`)**: one sentence on the
  median absolute offset between candidate and gold `start_ms`. Call out
  systematic drift (always early/late) or outlier lines. Empty string is OK
  when no timing data is available (e.g. instrumental track, candidate
  produced no timed segments).
- **Hallucinations (`hallucination_count`, `hallucination_details`)**: lines
  the candidate produced that are not in the gold (repetition loops,
  fabricated bridges, vocal-isolation artifacts). Count them; describe in
  one or two sentences. Empty `hallucination_details` is OK when count is 0.
- **Coverage (`coverage_pct`)**: fraction of gold lines that have a matching
  candidate line within ±500 ms of the gold `start_ms`. Float in [0.0, 1.0].
- **Wall-acceptable (`wall_acceptable`)**: boolean. Would you ship this on the
  live LED wall during a service? A single hallucination cluster, > 5 missing
  lines, or median timing offset > 400 ms = not wall-acceptable.
- **Score (`score`)**: integer 0–10 summary. 10 = perfect match. 0 = unusable.
  7 = wall-acceptable with minor flaws. 4 = useful for debugging but not wall.
- **Verdict (`verdict`)**: one of `match`, `partial`, `miss`, `hallucination`.
  `match` = wall-acceptable. `partial` = mostly right but missing/drifted.
  `miss` = the candidate doesn't track the gold. `hallucination` = candidate
  contains fabricated content that would visibly fail on the wall.
- **Reasoning (`reasoning`)**: 2–4 sentences explaining the score + verdict.

## Required output fields

Emit exactly one JSON object with these keys:

- `video_id` — copy from input
- `score` — integer 0–10
- `verdict` — enum
- `wer_estimate` — float
- `line_timing_assessment` — string
- `hallucination_count` — integer
- `hallucination_details` — string (use `""` when none)
- `coverage_pct` — float
- `wall_acceptable` — boolean
- `reasoning` — string (minLength 1)
- `judged_at` — current UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ`
- `judge_model` — your model id (e.g. `claude-opus-4-7`)
- `judge_prompt_revision` — integer; this revision is `1`

## Iron rules

- **Line-level focus only.** Ignore per-word timings (`feedback_line_timing_only.md`).
- **Wall is the bar.** Mechanical metrics are diagnostic; `wall_acceptable` is
  the decision.
- **Don't speculate.** If the candidate has no lines covering minutes 1:00–2:00
  and the gold does, that's missing coverage, not a "maybe instrumental
  break" excuse.
- **Cluster repetitions = hallucination.** If candidate has the same text
  string repeated 4+ times in a row with sub-second gaps, count it as one
  hallucination cluster, not 4+ separate matches.

## Updating this prompt

When this prompt changes in a way that materially affects judgments, bump the
revision integer in this file's header (`Revision N`). Claude reads the
revision directly from the header and threads it through each judgment's
`judge_prompt_revision` field. Old reports stay valid under their original
revision.
