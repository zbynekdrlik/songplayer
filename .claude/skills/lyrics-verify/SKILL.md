---
name: lyrics-verify
description: >
  Songplayer lyrics wall-verification workflow. Load when running /lyrics-verify
  or iterating on catalog songs — covers sp-live setlist approach, song-by-song
  discipline, pre-flight probing, reporting requirements, no-approval-pressure
  rules, and dark-wall detection.
user-invocable: false
triggers:
  - lyrics-verify
  - wall verify
  - wall verification
  - sp-live
  - setlist
  - play on wall
  - quarantine
  - catalog song
---

# Songplayer Lyrics Wall-Verification Rules

## Always use sp-live setlist, never another playlist's scene

Wall verify MUST use the `sp-live` setlist (playlist 184):
1. `POST /api/v1/playlists/184/items` with the song's `video_id`
2. `POST /api/v1/playlists/184/play-video` with `video_id`

Only SongPlayer's PROGRAM playlist reaches the wall (#221): it alone writes the
lyrics lines, the title and the Presenter, and its picture is what the LED wall
shows (`SP-program-MAX`). A play-video while sp-live is NOT on program plays it
off program, and nothing reaches the wall. So sp-live must be the program's
source: check `GET /api/v1/program` (`source` = 184), and if it is not, put it
there with the dashboard's Program control or `POST /api/v1/program/cut
{"source": 184}` before the loop.

NEVER cut the program to the song's native playlist (`sp-slow`, `sp-fast`,
etc.) during a verify loop: that disrupts the user's wall state for nothing.

## Pre-flight probe before reprocess

Always call `/api/v1/lyrics/probe-sources` before reprocessing a song. If
`any_text_source=false` (no yt_subs / description / Genius / LRCLIB /
Spotify text), skip the song silently — such a song can only reach the base
tier (the Gemini 3.5 Transcribe transcript grouped into lines), so a
reprocess spends FFmpeg/Gemini cycles for no text-backed result.

## Song-by-song iteration — no batch, no rollback questions

Lyrics processing must proceed strictly one song at a time:
1. Pick one song
2. Reprocess
3. Wall-verify (ONLY the LED wall reveals segmentation/timing — reading sample
   text is NOT verification)
4. If broken: FIX THE CODE NOW, inline, in the same session
5. Move to next song

Never propose batch processing, auto-approve, or "leave it for later" when a
regression is found. Never ask "should I rollback?" — fix the code.

## Reporting requirements per song

During verify sessions, every song outcome MUST report:
- **lyrics_source** — where lyrics came from: `yt_subs` / `description` /
  `genius` / `lrclib` / `spotify`, each as its v22 provenance — ★ tier
  `<text>+mtl@rev1/g35t-ok`, base tier `gemini-3-5-transcribe` (or
  `…/fullmix`) — or `asr_gap`
- **YouTube URL** — `https://youtube.com/watch?v=<id>`
- **Provider page URL** — Genius/LRCLib/Spotify page if used
- **First 6 lines sample** — so user can spot-check text
- **Honest one-sentence assessment** — not "looks good"; state what was observed

Bare source label (just "gemini-3-5-transcribe") is incomplete reporting.

## yt_subs is ground truth

`yt_subs` lyrics (YouTube's synced subtitles) are GROUND TRUTH for what the
singer says. The Gemini 3.5 Transcribe (g35t) transcript is the gate's
witness for timing and coverage, never a judge of the words. Never override
yt_subs with ASR output, never score yt_subs's wording against the transcript.

## No approval pressure

In verify loops, NEVER write AskUserQuestion options that nudge toward "accept
evidence and move on" when the wall hasn't been observed end-to-end. The user
is the final visual judge. Offer: re-play, skip-unverified, or surface the
issue — never "accept and continue" as the easy path.

## Propose songs with YouTube URL + playlist

When proposing a song for verification, ALWAYS include:
- `https://youtube.com/watch?v=<video_id>`
- Playlist name (`sp-fast`, `sp-worship`, etc.)

So the user can click and inspect the YouTube source before approving.

## Dark wall = halt, not verification

If Resolume Arena is hung or not consuming `SP-program-MAX` (Spout), the wall
is dark.
Worker dispatching correct subtitles to a stuck Resolume does NOT count as
verification. Detect: `curl 127.0.0.1:8090/api/v1/composition` returns error
or `Get-Process Arena | Format-List Responding` shows `False`.

When wall is dark: halt the verify loop, surface the issue to the user. Ask
to restart Resolume (never force-kill — see win-resolume-ops skill).

## Unrecoverable ASR miss → quarantine first

When a song with no usable text source serves a base-tier
(`gemini-3-5-transcribe`) track the transcript got wrong — garbled or missing
lines, only shouts — POST `/api/v1/lyrics/quarantine` IMMEDIATELY. Never
propose another "skip bad lines" / "drop suspect lines" heuristic — the
`asr_gap` escape hatch was shipped specifically for this case.

## Auto-play during reprocess sessions

During catalog reprocess sessions where user says "monitor processing and play":
- After EVERY song that completes at the current pipeline version with
  non-empty lyrics:
  - Report: id, title, artist
  - Add to ytlive playlist (playlist 184) if not already there
  - `POST /api/v1/playlists/184/play-video` with `video_id`
  - Confirm it was played in the next message
- Do NOT ask "want me to play?" — the directive is standing for the session
- Stop immediately on explicit stop-request; disable the worker; do not
  re-enable until the user explicitly asks to resume

## Gold reference verification

Before scoring any backend against lrclib gold timing, cross-check the gold
against the audio. LRCLib is community-sourced and quality varies. If multiple
ASR backends miss gold in the same direction by similar amounts, gold is the
outlier — fix or drop the fixture, not the backend.
