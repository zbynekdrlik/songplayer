---
paths:
  - crates/sp-server/src/db/models_dabing.rs
  - crates/sp-server/src/api/dabing.rs
  - crates/sp-server/src/api/routes_import.rs
  - crates/sp-server/src/startup_dabing.rs
  - crates/sp-server/src/dabing/**
  - scripts/dub_worker.py
  - sp-ui/src/pages/dabing.rs
  - sp-ui/src/components/dabing_list.rs
  - sp-ui/src/components/dub_toggle.rs
---

# Dabing (dubbing) feature — data model + section contract (#180 D1, epic #174)

The dubbing feature ("Dabing" — the word "sermon" appears NOWHERE in code/UI,
owner ruling #174) makes any video dubbable and adds an optimized **Dabing**
section for fast priority adds. Binding spec:
`docs/superpowers/specs/2026-09-17-dubbing-feature-design.md` (v2); plan:
`docs/superpowers/plans/2026-09-17-dubbing-feature.md` (lanes D1–D6).

## Data model (main decision: columns on `videos`, NOT a `dub_tracks` table)

Dub state lives as **columns on `videos`** (migration V26), mirroring the V24
stems pattern — one row per video, no joins. Spec §3's separate `dub_tracks`
table was **rejected** (two patterns for the same per-video processing state is
the worse maintenance deal). V26 columns:
`dub_requested`, `dub_status` (`none|queued|stems|transcript|translation|synth|
ready|failed`), `dub_file_path`, `dub_engine`, `dub_voice_ref_path`,
`dub_mix_ratio` (REAL, default 1.0), `dub_error`, `dub_attempts`,
`dub_next_attempt_at`, `dub_requested_at`, plus `stem_manual_priority` (the
stems worker's manual bucket — D3 uses it).

## Query surface — `db/models_dabing.rs` (declared `pub mod` in `db/mod.rs`)

- `set_dub_requested(pool, id, requested)` — requesting sets `dub_status='queued'`
  + stamps `dub_requested_at` + raises BOTH `stem_manual_priority` AND
  `lyrics_manual_priority` to 1 (so the chain's inputs jump their queues, spec
  §4). Un-requesting sets `dub_status='none'`, `dub_requested=0`, and LEAVES the
  priority flags (the video may still want stems/lyrics).
- `list_dub_videos(pool)` — `dub_requested=1`, newest first
  (`dub_requested_at DESC, id DESC`).
- `set_dub_mix_ratio(pool, id, ratio)` — clamps to `0.0..=1.0` (NaN → 1.0),
  persists, returns the clamped value.
- `dub_chain_state(dub_status, stem_status, lyrics_present) -> DubChainState`
  (PURE, unit-tested). `dub_status` is authoritative for the named stages; in the
  pre-transcript region (`queued`/`none`) it refines by artefacts: lyrics present
  ⇒ Transcript, else stems done ⇒ Stems, else Queued. Never overrides an explicit
  later status. `DubRow.chain_state` carries the resolved wire string so the UI
  does NOT duplicate the logic. The D4 write-side selectors
  (`get_next_dub_job`, `mark_dub_*`, `record_dub_deferral`) belong to D4, not D1.

## Import must pass the cookie gate (#180 addendum)

The Dabing/manual import metadata fetch is cookie-gated exactly like the download
path. `downloader/tools.rs::fetch_video_metadata(ytdlp_path, url, cookies)` takes
an optional cookie jar and threads `--cookies` (pure `metadata_args` helper,
unit-tested) before the URL. The shared import core `api/routes_import.rs::
import_video_core` resolves the jar as `cache_dir.parent()/cookies.txt` (the app
always sets `cache_dir = <data_dir>/cache`, so the parent IS the data dir where
the operator drops `cookies.txt`) and passes it when it exists. Both
`POST /api/v1/videos/import` and `POST /api/v1/dabing/import` reuse this core
(extracted OUT of `api/routes.rs` to respect its 1000-line cap). If the box's
`cookies.txt` has expired the import fails "Sign in to confirm you're not a bot"
— that is an ops step (`.claude/rules/youtube-cookies.md`), never hidden.

## API — `api/dabing.rs` (registered in `api/mod.rs`)

`GET /api/v1/dabing` → `{playlist_id, videos:[DubRow…]}`;
`POST /api/v1/dabing/import {url}` (import into the seeded Dabing playlist, then
`set_dub_requested(true)`); `PATCH /api/v1/videos/{id}/dub {requested}` (204/404);
`PATCH /api/v1/videos/{id}/dub-mix {ratio}` (200 + clamped `{ratio}` — D4 will
also push it to the live `DubControl`; D1 only persists).

## Seed — `startup_dabing.rs::ensure_dabing_playlist_exists`

Idempotent (`WHERE NOT EXISTS … kind='dabing'`), `name='Dabing'`,
`ndi_output_name='SP-dabing'`, `kind='dabing'`, `is_active=1`. Mirrors
`ensure_live_playlist_exists`; called from `lib.rs::start()`. Split into a sibling
`#[path]` module so `startup.rs` stays under the 1000-line cap. The OBS scene
`sp-dabing` is created by hand in D5 — legacy `yt*` scenes are NEVER touched.

## UI

Nav entry "Dabing" (`app.rs::Page::Dabing`, `/dabing`). `pages/dabing.rs` = a
paste field (`POST /api/v1/dabing/import`) + a 2 s poll filling
`store.dabing: RwSignal<Vec<DubRow>>` (page-owned loop, `try_get_untracked` on the
cancel flag per `sp-ui-frontend.md`). `components/dabing_list.rs` renders rows
newest-first with the glyph row `stiahnuté → stemy → prepis → preklad → dabing →
pripravené` (or `chyba: <krok>`) + a **Prehrať** button (reuses
`api::post_live_play_video` — it already accepts any playlist id).
`components/dub_toggle.rs` is the per-row Dabing toggle placed in `video_list.rs`
(any playlist). The `videos` list payload gains `dub_requested` + `dub_status`
(additive, `#[serde(default)]`, populated in `db::models::row_to_video`).

## Cap discipline hit here (do not repeat the mistake)

`db/models.rs` (998) and `lib.rs` (999) were AT the cap. Adding an AppState
`data_dir` field overflowed `lib.rs`, so the cookie jar is derived from
`cache_dir.parent()` instead (no new AppState field). Keep new server code in
sibling `#[path]`/`pub mod` modules; never grow `routes.rs`/`models.rs`/`lib.rs`/
`worker.rs`/`playback/mod.rs`.

# Dabing D4 (#183) — SK dub synthesis via Gemini Live Translate (audio→audio)

Owner ruling (#174): the ONLY dub engine is **Gemini Live Translate**
(`gemini-3.5-live-translate-preview`, audio→audio) — it keeps the preacher's
pacing, continuity and intensity. NO STT → translation → per-sentence TTS chain,
NO `DubEngine` trait, NO voice-reference selection (the older D4 ticket text is
obsolete). Default mix for a dub video = **dub only** (r=1.0); no same-colour
blend.

## Chain + state machine
`dub_status`: `queued → synth → ready | failed(+backoff)`. **Round 2 (#183): the
dub chain NO LONGER waits for stems** — there is no `stems` park state on the
happy path. Live folds the EN/SK transcripts INTO the one session, so there is NO
lyrics dependency (the `transcript`/`translation` `DubChainState` variants are
unused by D4). `models_dabing.rs` D4 selectors: `get_next_dub_job` (dub_requested=1,
downloaded, not none/ready, past backoff, **newest `dub_requested_at` first** =
the priority queue, now also selects `stem_status`), `mark_dub_synth`,
`mark_dub_ready`, `record_dub_deferral`; pure `dub_stems_ready` /
`dub_stems_state` / `synth_ready` (the proceed-without-stems decision) and
`raise_dub_stem_priority` (sets `stem_manual_priority=1` WITHOUT parking at
`stems`). The old `mark_dub_waiting_stems` (parked at `stems`) is RETIRED.

## Worker (`crates/sp-server/src/dabing/{mod,worker,child,chunk_plan}.rs`)
Mirrors the stem worker: 10 s tick, `dub_worker_enabled` kill-switch, venv-python
gate, `HeavyStepPlan::for_activity` (BELOW_NORMAL, never gates playback — owner:
processing runs during playback at reduced priority), #167 startup floor, memory
guard, one heavy child at a time (shared slot). **Round 2 (#183): NO stems
precondition** — the pure `synth_ready(downloaded, dub_stems_state, duration_ms)`
decides `Proceed` / `ProceedRaisePriority` / `WaitForDownload`; the dub NEVER
waits for stems. When stems are merely pending (absent but the duration is within
the 15-min stem cap) the worker raises `stem_manual_priority` ONCE
(`raise_dub_stem_priority`) so a later separation ENRICHES the mix (2-stream →
4-stream on the next open) and proceeds to `synth` immediately; when
`stem_status='unsupported'` (over the cap) or beyond the cap it proceeds without
raising (separation would only be marked unsupported). Live INPUT is the ORIGINAL
`audio_file_path` (not a stem). `dub_engine="gemini-live-translate"`.

- **Chunk plan (Rust owns it):** the worker runs ffmpeg `silencedetect` (a light
  BELOW_NORMAL pass; #144 r2: the dub now holds the heavy slot across it — the
  acquire moved to `process_next` before `synthesize`, so it no longer overlaps
  another heavy child), parses it with `chunk_plan::parse_silencedetect`,
  and `chunk_plan::plan_chunks` cuts at pauses ≥ 700 ms into chunks ≤ the session
  cap (round E: 2 min default, the `dub_session_max_s` setting), never mid-speech;
  the plan JSON is the child's `--chunk-plan`
  INPUT. `placement_for(chunk_start, chunk_len, out_len, next_start)` decides the
  atempo (≤ 1.08, only on overrun); the worker calls it per chunk AFTER the child
  returns each `out_len` to LOG + verify drift ≤ 2 s (`DUB_MAX_DRIFT_MS`), and
  warns if the child's applied tempo disagrees. `out_len` is only known at
  runtime, so the child implements placement at runtime (Python) and the Rust
  `placement_for` is the canonical decision + drift verifier.

## Child (`scripts/dub_worker.py live-translate`)
`--audio --out --transcripts --chunk-plan --work-dir --pace`. Reference:
`eval/dubbing/engines/gemini_live_translate.py`. Per chunk (resumable — reuses
`chunk_N.wav`+`.json`): ffmpeg-slice+resample to 16 kHz mono s16le, stream 100 ms
chunks (real-time by default; `dub_pace` setting → `--pace`, 2× tested on box),
`audio_stream_end`, collect 24 kHz PCM + input/output transcription (SK stamped by
output-audio position), trim trailing silence, atempo if it would overrun the next
chunk, write `chunk_N.wav`. **Heartbeats into `work_dir` every 5 s** so the
`wait_with_stall_timeout` never kills a healthy mid-chunk stream (a chunk can run
its full session cap with no other work-dir write). Assembles `<base>_dub.flac` at **48 kHz
STEREO** (must match the stem format `StemMixReader` requires), **loudness-matched to
the translated input — two-pass LINEAR loudnorm, see "round F" below** (was a fixed
single-pass `loudnorm -16` until 0.65.0-dev.9), writes
`<base>_dub_transcripts.json` (D3), and prints the summary JSON on stdout (the ONLY
stdout line — logs go to stderr). Key ONLY via `GEMINI_API_KEY` env (`bootstrap::
ensure_genai` pins `google-genai==2.24.0`, idempotent, never triggers the heavy
qwen/torch reinstall). Cost ~$0.037/min.

## #184 round G — the dub mix is now the ONE global fader console (SUPERSEDES the per-video ratio below)

The per-video `videos.dub_mix_ratio` column, `set_dub_mix_ratio` / `clamp_dub_ratio`
/ `dub_ratio_if_ready`, `dub_gains` / `dub_over_original_gains` / `DUB_ORIGINAL_FLOOR`,
`PATCH /api/v1/videos/{id}/dub-mix`, `EngineCommand::SetDubMix`, and the
`components/dub_mixer.rs` adapter are **DELETED**. The mixer is now the ONE global
three-fader console (`vokaly` / `podklad` / `dabing`) — see
`.claude/rules/karaoke-stems.md` "#184 round G". Key deltas for dub playback:

- The reader gain sets are DERIVED from `sp_core::mixer_model::stream_gains_*`, not
  a ratio: `DubMix` uses `stream_gains_dub(f)` (`[1,0,0,dabing]` when vokaly &
  podklad full, else `[0,vokaly,podklad,dabing]`); `DubOverOriginal` uses
  `stream_gains_dub_no_stems(f)` = `[vokaly, dabing]` — the whole original at the
  `vokaly` fader, **NO −18 dB floor**. `MixControl` publishes both from
  `set_faders(f)`; `reader::dub_gains_for(kind, MixFaders)` is the pure per-kind
  gain (still unit-tested).
- The mix is set via `PATCH /api/v1/mix {kind, vokaly?, podklad?, dabing?}` →
  `EngineCommand::SetMix{kind, faders}` → `engine.set_mix` → `control.set_faders(kind, f)`;
  the persist is done by the API handler AFTER the live push
  (`api/mix_apply.rs::apply_mix`, the round-A order). The dub video's readiness
  (its `DubRow.dub_status` / `stem_status`) drives which faders are LIVE.
- **Round G2 (SUPERSEDES G1) — the mix VALUES are remembered PER ITEM KIND, and each
  reader FAMILY is fed from its OWN memory; there is NO global "active kind".** The
  console keeps a SONG memory and a DUB memory (`MixConsole { song, dub }`, default
  song `(1,1)` / dub `(0,1,1)`); the dub readers always ramp toward the DUB memory,
  the song reader toward the SONG memory, so a song starting on ANY other output no
  longer doubles a playing dub's voices (and a dub never instrumental-mutes the next
  song). `PATCH /mix` NAMES the memory it edits (`kind` required) + persists only that
  kind's keys (`mix_song_*` / `mix_dub_*`); `GET /mix` returns BOTH memories
  `{song, dub}`. The G1 global `active`/`select_kind`/item-open hook are DELETED. See
  `.claude/rules/karaoke-stems.md` "#184 round G2".
- The dabing list (`dabing_list.rs`) no longer shows a per-row ratio; the console
  lives in the shared Player above.

Everything below is the pre-round-G per-video-ratio history — treat `dub_mix_ratio`
/ `dub_gains(r)` / `DUB_ORIGINAL_FLOOR` / `/dub-mix` / `SetDubMix` as DELETED.

## Playback (`stems/reader.rs` + `stems/control.rs`)
`open_audio_stream`'s pure `audio_source_kind(vocals, instrumental, dub)` chooses:
- **`DubMix`** — dub + BOTH stems → a **4-stream** `StemMixReader` `[original,
  vocals, instrumental, dub]` fed `dub_gain_handles()`. `dub_gains(r) =
  (0, 1−r, 1, r)`: original full-mix silent, vocals = original voice,
  instrumental = ambient bed, dub = SK.
- **`DubOverOriginal` (#183 round 2)** — dub WITHOUT both stems → a **2-stream**
  `StemMixReader` `[original, dub]` fed `dub_over_original_gain_handles()`.
  `dub_over_original_gains(r) = (max(1−r, DUB_ORIGINAL_FLOOR), r)` with
  `DUB_ORIGINAL_FLOOR = 0.125` (−18 dB): the FULL original (English speaker) is
  the bed, floored so the room never goes dead under the dub; at r=0 the original
  is full and the dub silent. This is what lets a long, un-separable video be
  dubbed. Stems arriving later promote a fresh open to `DubMix`.

Default r=1.0. `PATCH /api/v1/videos/{id}/dub-mix` persists to DB AND sends
`EngineCommand::SetDubMix{video_id, ratio}` → `engine.set_dub_mix` →
`control.set_dub_ratio`, which publishes BOTH the 4-stream quad AND the 2-stream
pair from the same `r` (live, no pipeline reopen — the #186 seam; only one mix is
ever open). `dub_gains_for(kind, r)` is the pure, unit-tested per-kind gain
chooser. Any dub-reader open failure degrades to the stem/plain mix.

## Cap note
`lib.rs` was at exactly 1000 lines, so the `EngineCommand` match was extracted to a
sibling `engine_dispatch.rs` (free `dispatch(&mut engine, cmd)`) before adding the
`SetDubMix` arm + the dub-worker spawn.

## Known gotchas (D4, verified on win-resolume 18.9.2026)

- **RESOLVED in round 2 (#183): a long video no longer stalls at
  `dub_status=stems`.** The stems precondition was removed (`synth_ready` never
  waits); a video without both stems plays the 2-stream `DubOverOriginal` mix
  (`[original, dub]`, original bed floored at −18 dB). The 15-min
  `STEM_MAX_DURATION_MS` cap still marks a 40-min file `stem_status=unsupported`,
  but the dub now proceeds to `synth` regardless. The sample "Morning Prayer &
  Devotion" (40 min, video 344) is the acceptance case: `stems → synth → ready`
  on its own, playing the 2-stream mix. (Historical: round 1 required stems and
  parked long videos forever — the owner's actual 40-min use case.)
- **`dub_worker.py` is materialised only when a dub reaches the synth step.**
  `DubWorker::ensure_script` runs AFTER the stems precondition, so on a box whose
  only dub video is stuck at `stems`, `dub_worker.py` is NOT written to
  `cache/tools/` and `google-genai` is NOT yet installed. To test the child
  standalone, write the script + `pip install google-genai==2.24.0` yourself.
- **Live-Translate core proof (real key, on-box):** a 20 s EN slice → 44 s of
  24 kHz SK PCM (~real-time + drain); `google-genai==2.24.0` installs + imports in
  the lyrics venv. 2× pacing (`dub_pace=2.0`) keeps the output complete and ~halves
  the send time (the fixed drain window means elapsed drops < 2×).
- **win-resolume console is cp1252 — a Python `print()` of Slovak (`ď` = ď)
  raises `UnicodeEncodeError`.** The real child is fine (it writes UTF-8 JSON with
  `ensure_ascii=False`); a debug probe must set `PYTHONIOENCODING=utf-8` or write to
  a file, not print SK text to the box console.

# Dabing D3 (#182) — EN/SK subtitles from the dub session (no second transcription)

Owner verdict (18.9.2026): a dubbed video does NOT run a second STT/translation
pass. The Gemini Live Translate session already returns the EN input
transcription + the SK output transcription; the dub child saves them as
`<base>_dub_transcripts.json`, and D3 turns that JSON into a normal
`sp_core::lyrics::LyricsTrack` so the wall/dashboard render bilingual subtitles
exactly like song lyrics.

## Transcript JSON schema (`scripts/dub_worker.py::build_transcripts`)
Each chunk of `<base>_dub_transcripts.json` carries:
`{index, start_ms, end_ms, at_ms, tempo, en, sk, sk_timed:[{t_ms, text}]}`.
- `en` = ONE untimed EN string per chunk; `sk` = the chunk's SK string.
- `sk_timed` = coarse SK fragments stamped by the chunk-local OUTPUT-audio
  position at arrival (`t_ms`).
- `at_ms` = the video-timeline offset where the chunk's output lands; `tempo` =
  the atempo the mix applied. **Both are computed at JSON-write time from the
  SAME per-chunk result the mix uses** (cached/resumed chunks included — NO
  re-synthesis, no extra API calls). Pinned by `test_build_transcripts_*`.

## Subtitle builder (`crates/sp-server/src/dabing/subtitles.rs`, pure + tested)
`transcripts_to_track(&DubTranscripts) -> LyricsTrack`:
- Fragment `i` occupies the chunk-local output window `(t[i-1], t[i]]` (`t[-1]=0`).
- Fragments group into LINES: close at sentence punctuation (`. ! ? …`), at 14
  words (`MAX_WORDS_PER_LINE`), or on a `> 1500 ms` (`LINE_GAP_MS`) arrival gap.
- Line video time = `at_ms + local_ms / tempo` (legacy JSON without `at_ms`/
  `tempo` → `start_ms` and `1.0`); `tempo` is clamped to `0.25..=4.0`
  (NaN/∞/≤0 → 1.0) and `to_video_ms` uses `saturating_add` so a degenerate
  tempo can't overflow the offset (#182 release-review item 8).
- **Two-pass timing (#182 release-review item 7 — no MIN-extension drift).**
  Pass 1 gives each line its TRUE `start` (monotonic — clamped to the PREVIOUS
  line's START, never its extended end) and its TRUE end. Pass 2
  (`finalize_line_ends`) sets the displayed `end = max(true_end,
  start + MIN_LINE_MS)` TRIMMED back to the next line's start when that is
  earlier (the last line keeps the untrimmed value; a same-start pair gets
  `start + 1`). This fixes the old bug where clamping each `start` to the
  previous MIN-EXTENDED `end` made a run of short lines push every later line
  progressively later (accumulated drift). Result: starts monotonic, no
  overlap, and a normal line after a run of 100 ms lines keeps its true start.
- A fragment group whose joined SK is empty after trim produces NO line (#182
  item 9); an all-blank transcript yields an empty track, so
  `subtitles_store::build_and_store_subtitles` stores nothing and returns 0.
- EN per line = the words of the chunk's `en` covering the same cumulative
  character fraction `[a,b)` the line covers of the chunk's SK, snapped to word
  boundaries (deterministic, order-preserving; EN is a reference — exact
  alignment not required). Empty `en` → empty EN lines; no `sk_timed` → no lines.
- `words: None`, `source = "gemini-live-translate"` (`SOURCE_LIVE_TRANSLATE`).
  Every branch is covered in `subtitles_tests.rs`.

## Persist through the SHARED writer (no parallel writer)
The lyrics worker's JSON-sidecar + DB persist was extracted to
`lyrics/track_store.rs::persist_lyrics_track` (writes `{youtube_id}_lyrics.json`
+ `mark_video_lyrics_complete`). BOTH the lyrics worker AND
`dabing/subtitles_store.rs::build_and_store_subtitles` call it — never a second
writer. The dub worker (`dabing/worker.rs::synthesize`) builds + stores the
subtitle track after the dub file is finalized and BEFORE `dub_status = ready`;
a subtitle failure is a WARN log and NEVER fails the dub.

## Startup backfill for dubs finished before D3 (#182)
The builder runs only at the end of a synthesis, so a dub that was already
`ready` (the 40-min acceptance sample) would never get subtitles. Once per
process, `DubWorker::process_next` (after the `dub_worker_enabled` kill-switch)
calls `subtitles_store::backfill_missing_subtitles`: the pure-SQL selector
`models_dabing::list_ready_dubs_without_subtitles` (`dub_requested=1`,
`dub_status='ready'`, `lyrics_source` NULL or != `gemini-live-translate`) → build
from the saved `<base>_dub_transcripts.json` (a legacy JSON without `at_ms`/`tempo`
falls back to `start_ms` / `1.0`). Once per process, never per tick — an unusable
JSON must not loop; failures are WARN only. The call sits in `process_next`
(structurally mutation-excluded), NOT in `run`, so it adds no whole-fn mutant.

## Lyrics queue skips dub videos
Every selector bucket in `lyrics/reprocess.rs` (manual/null/stale/fullmix) ANDs
in ONE shared predicate const `EXCLUDE_DUB_REQUESTED`
(`AND (v.dub_requested IS NULL OR v.dub_requested = 0)`), and
`db/models_dabing.rs::set_dub_requested` NO LONGER raises `lyrics_manual_priority`
(keeps `stem_manual_priority`). A 40-min talk therefore never enters the ★/g35t
song-lyrics pipeline. `LYRICS_PIPELINE_VERSION` is untouched.

## UI (chain + mixer)
- `components/dabing_list.rs`: the chain is the real engine chain
  `stiahnuté → dabing → titulky → pripravené`; a `stemy` step is inserted when
  the video has or is getting stems (`shows_stems_step`: shown unless
  `stem_status = "unsupported"`). `prepis`/`preklad` are GONE.
- `sp_core::mixer_model::dub_channel_labels(has_stems)` +
  `components/dub_mixer.rs`: WITHOUT stems only 2 faders (`originál` / `dabing`,
  ambient hidden — the 2-stream over-original mix); WITH stems the full 3-fader
  strip (`originál hlas` / `dabing` / `ambient`). `has_stems = stem_status ==
  "done"` for the mixer (current reality), broader for the chain (intent).
  `e2e/mixer.spec.ts` asserts both fader shapes; `e2e/dabing.spec.ts` both chain
  shapes; zero console errors.

# Dabing round A+B (#184) — instant mix apply + one mixer rule on every page

## `DubRow.stem_status` is the RAW stems-worker column, NOT the `stems_state` wire vocab
`DubRow.stem_status` (server `db/models_dabing.rs::row_to_dub_row`) is the raw
`videos.stem_status` COLUMN: `NULL` (pending) / `'done'` (ready) / `'failed'` /
`'unsupported'`. This is a DIFFERENT vocabulary from the derived `stems_state`
wire strings (`ready`/`queued`/`processing`/`failed`/`unavailable`) that
`stems::models_stems::stems_state_of` produces for the karaoke/videos payloads.
So `dub_mixer.rs` uses `stem_status == "done"` for `has_stems`, and
`sp_core::mixer_model::mixer_controls`'s stems-capable set MUST include `"done"`
(the real value a dub row carries) — the wire strings alone would make a
stems-ready dub (`stem_status='done'`) fail the karaoke gate on the real box
while passing under a mock that feeds `"ready"`. Never assume the two columns
share a vocabulary.

## Dub-mix applies live-first: push the engine command BEFORE the DB persist
`api/dabing.rs::patch_dub_mix` awaits `EngineCommand::SetDubMix` FIRST, then
`set_dub_mix_ratio` (the persist), via the pure seam
`api/dabing_apply.rs::apply_dub_mix(push, persist)`. The old order (persist then
push) let the DB `acquire()` park up to sqlx's **30 s** default before the live
gains moved — the owner's "~30 s to apply" report. Clamp once via
`models_dabing::clamp_dub_ratio` so the push carries exactly what gets stored; a
persist failure is logged + 500 while the live change already happened. Pairs
with `db/mod.rs::pool_tuning()` (WAL + NORMAL sync + 5 s busy + **2 s** acquire),
applied only to the FILE pool (`create_memory_pool` stays plain — WAL needs a
file). No schema change.

## The `/api/v1/dabing` poll lives in `App`, not the Dabing page (#184 B1)
`store.dabing` (and `store.dabing_playlist_id`) are filled by an App-level
`store::poll_value` loop (`app.rs`, next to the playlists load), so the shared
Player picks the dub mixer for a playing dub video on EVERY page — not only after
visiting `/dabing`. `pages/dabing.rs` just READS the store now. `player.rs`
renders `<DubMixer>` and/or `<KaraokeMixer>` from `mixer_controls(dub_status,
stem_status)`, with `show_karaoke = controls.karaoke || !controls.dub` (a non-dub
song always keeps the karaoke default; a dub video shows karaoke only when
stems-capable). `e2e/dabing-mixer.spec.ts` proves it on Prehľad + Naživo without
a `/dabing` visit.

# Dabing round C (#184) — one stable dub voice per video (pinned Gemini voice)

The dub used to change voice every few sentences (female → male → another male,
one speaker on screen) because `dub_worker.py` built the Live config with NO
`speech_config`, so Gemini re-rolled the output voice per Live session and per
turn. Round C PINS one voice per video.

## The voice is PINNED via `speech_config` (probe-verified)
`dub_worker.py::_translate_pcm` now sets
`speech_config=SpeechConfig(voice_config=VoiceConfig(prebuilt_voice_config=
PrebuiltVoiceConfig(voice_name=<voice>)))` on the `LiveConnectConfig`, ALONGSIDE
the existing `translation_config`. The translate model `gemini-3.5-live-translate-
preview` ACCEPTS `speech_config` (probe 2026-09-21: two runs, same voice, f0
median spread 1.4 st ≤ 2 st) — the abandoned fallback (one session per video, no
pin) is NOT needed. Full probe recipe: `.claude/rules/dubbing-eval.md`.

## Setting → worker → child, mirroring `dub_pace`
`dub_voice` setting (default `sp_core::config::DEFAULT_DUB_VOICE` = `Charon`),
read per tick in `dabing/worker.rs::synthesize` via the pure
`dub_voice_from(setting) -> String` (absent/blank → default, any non-blank name
trimmed + passed through — the catalogue is NOT enforced in the worker, so a new
voice needs no code change), threaded into `child.rs::live_translate_args`
(`--voice`). The six catalogue voices live in `eval/dubbing/voices.py` and are
mirrored in the Nastavenia select (`sp-ui/components/settings_form.rs::DUB_VOICES`,
Slovak labels).

## Voice-keyed resume + persisted voice (repurposed column, NO schema change)
- The child records `"voice"` + `chunk_start_ms`/`chunk_end_ms` in each
  `chunk_N.json`; the pure `dub_worker.py::chunk_reusable(meta, voice, start_ms,
  end_ms)` reuses a cached chunk ONLY when its recorded voice matches the requested
  one AND its boundaries equal the current slot AND it carries the round-E2
  voice-guard record (`voice_medians`, non-empty) (a legacy chunk with no `voice` /
  bounds, one under another voice, one from an OLDER chunk plan, or an UNGUARDED
  chunk is re-synthesized) — so a voice change re-does the dub in one voice, a
  changed session ceiling (round E: 8 → 2 min) never lays old 8-min chunks under
  the new plan (the round-E integration bug caught on video 344's work dir:
  `chunk_0..4` from round C would have been reused for the 2-min slots 0–4 and the
  `amix` would have doubled the speech from ~10 min on), and a re-dub requested to
  FIX drift never silently keeps pre-guard audio (the E2 acceptance re-dub of 344
  reused all 19 round-E chunks — the new guard never ran — until this rule).
- The resolved voice is persisted per video in the EXISTING nullable
  `dub_voice_ref_path` TEXT column REPURPOSED as the voice name (it was dead
  clone-lane plumbing; no migration, documented in the `db/mod.rs` V26 comment),
  via `models_dabing::set_dub_voice`; exposed as `DubRow.dub_voice` on the
  `GET /api/v1/dabing` payload and shown in the Dabing row as `hlas: <voice>`.

## `scripts/dub_voice_check.py` — objective consistency check (box/dev1 tool)
**Round E replaced the round-C IQR rule (it verified nothing).** Reads a dub
FLAC/WAV, per-window (**5 s**, was 30) f0 median, and exits 1 when the FRACTION of
voiced windows more than **6 st ABOVE the file median** exceeds
`MAX_HIGH_BAND_FRACTION = 0.05` — the rotating-voice symptom (recurring female
stretches above a base male voice). The max spread and IQR are still printed but
are **information only**: measured 21.9.2026 on the 36-min sample, ONE pinned
voice has IQR 4.5 st / max spread 14.7 st (natural intonation + an octave-error
window), so the round-C IQR gate diluted 100-s female stretches into a passing
number. A female↔male rotation puts whole 5-s windows in the ~200 Hz band; a
single male voice stays in 80–145 Hz. NB a *balanced* 50/50 octave alternation is
NOT flagged (by definition ≤ 50 % of windows exceed the median, and the arithmetic
median of an equal split sits too close to the high voice) — that is not the real
symptom. It is NOT executed in CI but IS ruff-lint-scoped (`ci.yml` eval-checks).
Its pure helpers (`high_band_fraction`, `f0_autocorr`, `window_medians`) run in CI
eval-checks WITHOUT librosa — the f0 measurement prefers `librosa.pyin` but falls
back to a dependency-free numpy autocorrelation, and the pytest forces the fallback
(`use_librosa=False`) so it RUNS, never skips (CI installs numpy + soundfile, not
librosa).

# Dabing round E (#184) — the pinned voice drifts inside a long session: cap it + a per-chunk guard

Round C pins the voice via `speech_config`, and the pin HOLDS at a session start —
but the model DRIFTS to another voice INSIDE a long session (video 344, 5 sessions
of ~430 s: 55/431 5-s windows landed in a 200–224 Hz female band while the SOURCE
at those seconds was a steady male 116–134 Hz; where the source itself rose to
~200 Hz the dub correctly followed). The 120 s vs 30 s experiment: one pinned 120 s
session = 0 drifted windows; the same slice as four 30 s sessions = 3 (11 %); ~7 min
is where drift showed. So **~2 min is the validated stable point.**

## (a) Session cap — `chunk_plan.rs` + `dub_session_max_s` setting
`chunk_plan::DUB_SESSION_MAX_MS = 120_000` is the default ceiling
`ChunkPlanConfig::default` uses (`MAX_CHUNK_MS` is kept as its alias). The
`dub_session_max_s` setting (`sp_core::config::SETTING_DUB_SESSION_MAX_S`, default
120, clamped 60..=480 s) is read per tick in `worker.rs::synthesize` via the pure
`dub_session_max_ms_from(setting) -> u64` (mirrors `dub_pace_from`/`dub_voice_from`)
and passed as the plan ceiling. `plan_chunks` still cuts only at pauses ≥ 700 ms,
never mid-speech; a 36-min talk is ~18 sessions (the ~27 s drain per session adds
~8 min, ≈ 1.4× realtime, accepted).

## (b) Per-chunk voice-band guard — round E2 (`dub_worker.py`)
Round E measured each output window against the CHUNK's OWN median with a 0.20
re-synth trigger, and on video 344 it still left 7.3 % true-drift windows (file
gate 5 %). Two defects: (1) **the whole-chunk blind spot** — a chunk high
THROUGHOUT (chunk 10) has a high chunk median, so 0 windows clear +6 st and it
falsely passes; (2) **the trigger was 4× laxer than the gate** — 0.20 vs the file
gate's 0.05, so chunk 5 at 18 % passed the guard while failing the file. Round E2:

- **Baseline-relative, not chunk-relative.** `chunk_voice_drift(out_medians,
  in_medians, out_baseline, in_baseline)` measures each output window against the
  RUNNING **pinned-voice baseline** (the median of the voiced OUTPUT windows of the
  chunks ACCEPTED so far) and discounts source-following against the RUNNING
  **source baseline** (the running median of the input windows) — NOT the chunk's
  own medians, so a chunk high throughout is caught. It delegates to the ONE shared
  `dub_voice_check.drift_windows`, so the guard and the file gate measure the SAME
  thing. Baselines accumulate in `cmd_live_translate` order; each `chunk_N.json`
  persists `voice_medians` + `voice_in_medians` so a resumed run rebuilds them
  (`baseline_from_meta`). A still-drifted or guard-skipped chunk NEVER feeds them.
- **Trigger as strict as the file gate.** `_chunk_is_drifted` fires at `>= 2`
  true-drift windows OR fraction `> VOICE_DRIFT_FRAC = 0.05` — the SAME 0.05 as
  `dub_voice_check.MAX_HIGH_BAND_FRACTION`, pinned equal by a unit test.
- **Seed (chunk 0).** With no baseline yet, chunk 0 is checked against the pinned
  voice's expected `VOICE_F0_BAND` (measured by `eval/dubbing/voice_band_measure.py`
  with the SAME `use_librosa=False` autocorrelation the runtime uses, widened to a
  coarse per-voice band; an unknown voice → seed as-is). A wrong seed would poison
  every later baseline, so an out-of-band seed is treated as drifted.
- **Up to 2 re-synths** (`VOICE_RESYNTH_ATTEMPTS`, new session, same pin), keeping
  the fewest-drift take; a chunk still drifted after 2 ships with
  `voice_band_ok=false` (logged, never fails the dub).
- **File gate `--source`.** `dub_voice_check.py --source <original>` reports the
  true-drift fraction (source-following discounted, same shared definition) and
  gates on IT; without `--source` round-E behaviour is unchanged. This is the
  acceptance number: `--source orig344.flac` true-drift ≤ 0.05.

**The guard stays BEST-EFFORT — it must never fail the dub it decorates.** The
whole scan + re-synth lives in `_apply_voice_band_guard`'s `try/except`: any
failure (numpy/`dub_voice_check` import, a truncated/unreadable wav) is logged and
falls through with `voice_band_ok=None`, empty medians (no baseline update), no
re-synth. Do NOT unwrap it (round-E review MAJOR). Two known blind spots persist:
autocorrelation octave-collapse makes `VOICE_F0_BAND` a coarse seed gate (not a
fine voice discriminator — the window guard + the pyin `--source` gate are the
real detectors), and a truly balanced 50/50 octave rotation inside one chunk is
still not the observed symptom.

# Dabing round F (#184) — the dub is loudness-matched to the audio it translates

Owner verdict 2026-09-23 (#184 comment 5793796815): "ked su pomery rovnake tak
dabingovi hlas je tichsi ako orginal!". Measured on video 344 (5–15 min, ebur128):
original −14.5, vocals −14.5, **dub −16.1 LUFS**. The old assembly used a fixed
single-pass DYNAMIC `loudnorm=I=-16`, while the original is two-pass `I=-14`
(`downloader/normalize.rs`). The stems are never re-normalized (`karaoke-stems.md`),
and the mixer applies faders as plain linear gains, so the same fader value played
the dub 1.6 LU quieter than the voice next to it.

**Rule: dub loudness = the MEASURED integrated loudness of the child's `--audio`,
clamped to −24…−10 LUFS, applied with a two-pass LINEAR loudnorm (TP −1.5, LRA 11).**
`--audio` is `job.audio_file_path`, the video's normalized ORIGINAL (not the vocals
stem — the child never receives the stem). For speech the two measure the same (344:
−14.5 / −14.5). `dub_worker.py::_assemble_dub` makes three ffmpeg calls:
1. `loudness_measure_args(ff, audio)` does a loudnorm analysis of the input (null
   muxer), then `loudness_target(input_i)` applies the clamp (`-inf` clamps to −24,
   `NaN` raises).
2. `assembly_args(…, loudnorm_analysis_filter(target), None)` analyses the
   assembled mix (the same `build_mix_filter` graph) to null.
3. `assembly_args(…, build_loudnorm_second_pass(mix, target), out)` writes the dub
   with `measured_I/LRA/TP/thresh` + `offset`, `linear=true`, `print_format=json`.

`parse_loudnorm_json` reads the last `{…}` block carrying `input_i` (CRLF-safe) and
raises if the block is missing or incomplete. The stats (`source_i`, `target_i`,
`mix_i`, `output_i`, `normalization_type`) go to `<work_dir>/loudness.json` and to
the `dub loudness: …` stderr line, which is the last child log line, so it shows in
the sp-server "dub live-translate ok; stderr tail". ffmpeg silently falls back to
DYNAMIC mode when the linear gain would breach TP −1.5 or the mix LRA exceeds 11.
`normalization_type` records which mode actually ran, so read it before trusting a
level. Local real-ffmpeg 6.1 smoke: source −14.75 → output −14.73 (linear), ebur128
orig −14.7 / dub −14.7. The mono mix → `-ac 2` upmix keeps the loudness (swr's
−3 dB centre gain). Tests: `scripts/tests/test_dub_worker_loudness.py` (pure helpers
+ the 3-call orchestration with `_run_stderr` faked; no ffmpeg in eval-checks).

## Rebuild an existing dub so ONLY the assembly reruns (e.g. video 344 after deploy)

`PATCH /api/v1/videos/344/dub` with body `{"requested": true}` (204):
1. `models_dabing::set_dub_requested(true)` sets `dub_status='queued'`, bumps
   `dub_requested_at` (the top of the newest-first queue) and sets
   `stem_manual_priority=1` (harmless).
2. The dub worker (10 s tick) picks it up: `get_next_dub_job` selects
   `dub_status NOT IN ('none','ready')`. It runs `mark_dub_synth`, then the same
   `silencedetect` → the same chunk plan (unchanged `dub_session_max_s`) and the same
   pinned voice (`dub_voice` setting unchanged).
3. The child reuses every `chunk_N.wav`/`.json` for which `chunk_reusable` holds
   (same voice, same `[start,end)`, non-empty `voice_medians`); the log shows
   `chunk N: resume`. No Gemini session is opened for a reused chunk.
4. Only `_assemble_dub` runs again (plus the transcripts JSON + subtitles store),
   then `mark_dub_ready`.

The request needs the NEW `dub_worker.py` on the box. The worker re-materialises the
embedded script (`ensure_script`), so deploy first. Verify with the
`<cache>/<youtube_id>_dub/loudness.json` next to the chunks, then an ebur128 of the
new `<base>_dub.flac` against the vocals stem on the same slice (±1 LU acceptance).
Caveat: a chunk whose guard was skipped (`voice_medians` empty) IS re-synthesized,
per the round-E2 reuse rule. That costs Gemini time, but it is correct.

