# Dubbing feature ("Dabing") — EN/SK subtitles + live-mixed Slovak dub for any speech video — design v2 (sub-project A of #174)

Status: v2 after owner review (17.9.2026): approved in principle with amendments — dubbing is a cross-cutting FEATURE (any video), plus an optimized **Dabing** section for fast priority adds, plus a redesigned modern mixer shared with stems. The word "sermon" appears nowhere in code or UI. Supersedes the v1 draft (`spec-2026-09-17-sermon-dubbing-design.md`).

## 1. What the operator does
**Anywhere:** every video row (any playlist) has a **Dabing** toggle. Switching it on queues the dubbing chain with priority; the row shows the chain state (`stiahnuté → stemy → prepis → preklad → dabing → pripravené`, or `chyba: <krok>`). When ready, the video plays in its own playlist through the mixer (original voice ↔ dub ratio) and is also listed in the Dabing section.

**Dabing section** (optimized for "pred/počas bohoslužby, bez klikania"):
1. One field **Vlož URL** + Enter → download + dubbing chain at top priority; the card appears immediately with live state.
2. List of every dubbed video (from the section AND from playlists), newest first, one-click **Prehrať** → plays on the section's own output `SP-dabing` (OBS scene `sp-dabing`, created once by hand; legacy `yt*` scenes untouched), so it can be cut at any moment without configuration.
3. The mixer (below) inline on the playing card; per-video ratio remembered, global default in settings.

**Mixer (new, modern, ONE component):** replaces today's stems mixer. Channels: for songs `vokál / inštrumentál`; for dubbed videos `originál hlas / dabing / ambient`. Vertical faders or a single crossfade + a level per channel, big touch-friendly targets, presets (`len dabing`, `50/50`, `originál`), live VU. Same component in the playlist card, the Dabing card and the fullscreen player view. UI ticket D2 owns the visual design (Leptos component + CSS, Playwright-tested).

## 2. Processing chain (per video with `dub_requested`, resumable, low process priority)
| step | reuse | new |
|---|---|---|
| download | yt-dlp downloader, split-file cache (`_video.mp4` + `_audio.flac`), loudnorm −14 LUFS | Dabing-section adds create the `videos` row from a bare URL (no YouTube playlist) |
| stems | `scripts/stem_worker.py` (RoFormer vocals/other) | `voice.flac` + `ambient.flac` |
| transcript EN | Gemini 3.5 Transcribe route (lyrics base tier), segmented + resumable like #171 | long-form: 10-min chunks, 20 s overlap, sentence timestamps |
| translation SK | Claude via CLIProxy (existing numbered-line path) | sentence-level, timing untouched |
| subtitles | existing lyrics JSON (`lines[]` + SK translation, `words: None`) → wall `#sp-subs` + dashboard unchanged | one track per video |
| dub SK | — | engine per D0 (#175) listening test (Soniox cloud first; Chatterbox on dev2 only if it clearly wins); voice cloned from a clean 15–20 s span of `voice.flac`; per-sentence synthesis fitted to the EN sentence span (§6) → `dub.flac` on the video timeline |
| playback | `SplitSyncedDecoder` (audio master clock), stems mixing path | three stems live: `voice × (1−r)`, `dub × r`, `ambient × 1`, `r` from the mixer |

Failure policy: every step idempotent and resumable from its last completed chunk (the #171 pattern); a failed step marks the row `chyba: <step>` with the child stderr tail, never a silent skip. Heavy steps use the existing one-process-wide heavy slot at BELOW_NORMAL; playback is never gated (#162).

## 3. Data model
- `videos.dub_requested` (bool, default 0) — the toggle; set by the row switch or implicitly by a Dabing-section add.
- `dub_tracks(video_id PK, voice_path, ambient_path, dub_path, subtitles_path, dub_engine, dub_voice_ref, mix_ratio, state, last_error, updated_at)`.
- `playlists.kind = youtube | dubbing` — the Dabing section is one `dubbing` playlist (items added by URL, `ndi_output_name = SP-dabing`).
- settings: `dub_mix_default` (0–100), `dub_engine`.
- Migration: incremental (`schema_version` bump, additive columns/tables only).

## 4. Priority scheduling
Per free heavy slot: `dub chain (oldest first) > lyrics manual_priority > lyrics stale bucket > stems`. A new dub job starts when the current child finishes (never kills a running child); the row shows queue position. Dabing-section adds and toggles share one queue.

## 5. Where compute runs
Everything on win-resolume as today (download, stems, Gemini/Claude/Soniox are network calls). No local TTS model on the box (GPU shared with the live wall). If D0 picks Chatterbox, a separate ticket adds a dev2 GPU worker service (HTTP job API: upload voice sample + SK text, download `dub.flac`).

## 6. Timing fit for the dub
Per EN sentence `[t0, t1]`: synthesize SK; if `dur_sk ≤ t1−t0` place at `t0` and pad; else tempo up to +15 % (engine speed param or atempo); else place at `t0`, overflow up to the next sentence's `t0` − 150 ms, hard-cut with a 50 ms fade (logged `overflow`; D0 reports the overflow ratio per engine).

## 7. Tests
- Unit: timing-fit (fit / tempo / overflow / cut), 3-stem gains, scheduler order, `dub_tracks` state machine, URL-add row creation.
- E2E post-deploy (Playwright): Dabing section renders; URL field creates a card (mock or fixture); a `pripravené` fixture shows the mixer; mixer value persists via API read-back; the row toggle in a playlist card; zero console errors.
- Box acceptance: the owner's sample URL processed end-to-end; `sp-dabing` shows EN/SK subtitles; mixer 100 % = only SK audible, 0 % = only original; DOM version.

## 8. Tickets (D0 filed as #175; the rest after the plan)
- D0 listening test (#175) → engine verdict
- D1 data model (`dub_requested`, `dub_tracks`, `playlists.kind`) + Dabing section with URL add + priority download + row toggle
- D2 modern mixer component (stems + dub), replaces the stems mixer everywhere
- D3 long-form transcript + translation chunking → EN/SK subtitles on the wall
- D4 dub synthesis (engine per D0) + timing fit + `dub.flac` + 3-stem playback
- D5 `sp-dabing` scene/NDI output + post-deploy E2E + box acceptance on the sample
- D6 priority scheduler across dub/lyrics/stems (if D1's simple version is not enough)

## 9. Out of scope here
B (SongPlayer program output / switcher) and C (Companion / Stream Deck layer) — separate epic after A.
