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

## Always use sp-live setlist, never scene-switch

Wall verify MUST use the `sp-live` setlist (playlist 184):
1. `POST /api/v1/playlists/184/items` with the song's `video_id`
2. `POST /api/v1/playlists/184/play-video` with `video_id`

NEVER switch OBS to the song's native scene (`sp-slow`, `sp-fast`, etc.) during
a verify loop. Scene switches disrupt the user's current wall state.

## Pre-flight probe before reprocess

Always call `/api/v1/lyrics/probe-sources` before reprocessing a song. If
`any_text_source=false` (no Demucs/Gemini candidate sources), skip the song
silently — do not waste FFmpeg/Gemini cycles on a whisperx-only song.

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
  `genius` / `lrclib` / `spotify` / `ensemble:gemini` / `asr_gap`
- **YouTube URL** — `https://youtube.com/watch?v=<id>`
- **Provider page URL** — Genius/LRCLib/Spotify page if used
- **First 6 lines sample** — so user can spot-check text
- **Honest one-sentence assessment** — not "looks good"; state what was observed

Bare source label (just "ensemble:gemini") is incomplete reporting.

## yt_subs is ground truth

`yt_subs` lyrics (YouTube's synced subtitles) are GROUND TRUTH for what the
singer says. WhisperX is a helper, never a judge. Never override yt_subs with
ASR output, never score yt_subs against whisperx.

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

If Resolume Arena is hung or not consuming the SP-live NDI, the wall is dark.
Worker dispatching correct subtitles to a stuck Resolume does NOT count as
verification. Detect: `curl 127.0.0.1:8090/api/v1/composition` returns error
or `Get-Process Arena | Format-List Responding` shows `False`.

When wall is dark: halt the verify loop, surface the issue to the user. Ask
to restart Resolume (never force-kill — see win-resolume-ops skill).

## WhisperX ASR miss → quarantine first

When whisperx shows `conf<0.05` ghost-cluster forced-alignment artefacts
(multiple words collapsed to same timestamp), POST `/api/v1/lyrics/quarantine`
IMMEDIATELY. Never propose another "skip ghosts" / "drop low-conf lines"
heuristic — the `asr_gap` escape hatch was shipped specifically for this case.

## Auto-play during reprocess sessions

During catalog reprocess sessions where user says "monitor processing and play":
- After EVERY new v18+ Gemini song completes (all chunks non-empty):
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
