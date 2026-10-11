---
paths:
  - "crates/sp-server/src/lyrics/register_check*.rs"
  - "crates/sp-server/src/lyrics/worker_register*.rs"
  - "crates/sp-server/src/lyrics/translator.rs"
  - "crates/sp-server/src/lyrics/worker_translation.rs"
  - "crates/sp-server/src/db/models_register*.rs"
  - "crates/sp-core/src/lyrics.rs"
---

# The register check: worship songs, checked once per track (#250)

Owner, 11.10.2026: PP's wall showed "I won't swell" / "Nenapuchnem" for
"I will dwell in the house of the Lord" ("We Raise", base tier). The songs
are Christian worship songs. A line a transcript misheard into an indecent
or absurd reading is caught WHILE the track is processed, never at display.
ROZHODNUTÉ 6106231933: no tokens per showing.

## Why after the transcript, not in it

- Gemini 3.5 Transcribe has no prompt or context field.
- Its only text input, `transcription_config.custom_vocabulary`, is refused
  together with word timestamps, and the base tier needs those timestamps
  (ai.google.dev/gemini-api/docs/transcribe, parameter table, read
  11.10.2026).
- The translator prompt stays neutral (`translator.rs` "Prompt design").
  "worship", "song" and "lyrics" made Claude refuse the translation.

## The check (`lyrics/register_check.rs`, `lyrics/worker_register.rs`)

- **One Claude call per track.** The worker calls it right after
  `translate_track` and before `persist_lyrics_track`.
  - Input: the numbered `EN || SK` lines.
  - Output: `N | English | Slovak` per line, `N | ? | ?` when the model
    cannot tell what was meant, or `NONE`.
- **Fixes and flags.**
  - A fix rewrites the line's `en`, its `sk` and drops its `words`. Its
    timing stays. Its old text goes into `LyricsTrack::register.fixed`.
  - A `?` line is flagged for the operator.
  - More fixes than `max_fixes` (a quarter of the lines, at least 3) is a
    rewrite. Then nothing is applied and every listed line is flagged.
  - An operator's own text (`override` sources) or a ★ text (`/g35t-ok`)
    is never rewritten, only flagged (`may_fix`).
  - `NONE.` / `NONE!` read as `NONE`.
- **Refusals and failures.**
  - A refusal (in the answer, or Claude's content filter) is verdict
    `refused`. The track stays as it is. The next song waits out the
    shared translation backoff, because each content-filter refusal cools
    the proxy's credential down for 60 s.
  - Any other failure leaves no check in the track. The stale pass asks
    again after that backoff, and after `MAX_FAILED_CHECKS` (3) failures
    stamps the song forward with no verdict and moves on.
  - Every new track for the video resets the columns to unchecked
    (`mark_video_lyrics_complete`: a reprocess, an upgrade, a peer's copy).
- **Where the verdict lives.**
  - In the track: `{yt}_lyrics.json` `register` (`sp_core::lyrics::RegisterCheck`).
  - On every row of the video: V36 `lyrics_register_version` /
    `lyrics_register_verdict`, through `models_register::stamp_register`.
  - On `GET /api/v1/lyrics/songs` as `register_verdict`.
- **The stale pass, `recheck_next_stale_register`.**
  - It runs with the translation passes and checks ONE served track per
    tick that was never checked, or was checked under an older
    `REGISTER_CHECK_VERSION`.
  - It rewrites the JSON in place.
  - A track that already carries the current check (a peer's copy) is
    only stamped, with no call.
- **Paid AI.** The check is paid AI. The fresh path holds it with the
  translation (`translation_allowed`).
  - A node that checks nothing itself (paid AI off or no AI client,
    i.e. PP) never asks or holds.
  - Every 10 minutes it stamps the stale songs whose track already carries
    a peer's check (`stamp_peer_checks`, `scan_due`).
- **Versions.** No `LYRICS_PIPELINE_VERSION` bump. Bump
  `REGISTER_CHECK_VERSION` when the prompt or the apply rules change what
  the check does.

## Before a prompt change ships

Probe it live on SNV through the real proxy (`proxy_probe.py`, the
`ai-proxy.md` rules), on:

- "We Raise"'s original base-tier transcript
  (`~/.claude/work-products/pp_8_r1kXQWtbA_lyrics.orig.json` on dev1);
- 5 ★ tracks and 5 base-tier tracks.

Check two things: no refusal, and no correction of a line that was right.
Each content-filter refusal cools the proxy's credential down for 60 s.
