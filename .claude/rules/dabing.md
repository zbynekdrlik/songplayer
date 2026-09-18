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
`dub_status`: `queued → stems (wait) → synth → ready | failed(+backoff)`. Live
folds the EN/SK transcripts INTO the one session, so there is NO lyrics
dependency (the `transcript`/`translation` `DubChainState` variants are unused by
D4). `models_dabing.rs` D4 selectors: `get_next_dub_job` (dub_requested=1,
downloaded, not none/ready, past backoff, **newest `dub_requested_at` first** =
the priority queue), `mark_dub_waiting_stems` (sets `stem_manual_priority=1`),
`mark_dub_synth`, `mark_dub_ready`, `record_dub_deferral`, pure `dub_stems_ready`.

## Worker (`crates/sp-server/src/dabing/{mod,worker,child,chunk_plan}.rs`)
Mirrors the stem worker: 10 s tick, `dub_worker_enabled` kill-switch, venv-python
gate, `HeavyStepPlan::for_activity` (BELOW_NORMAL, never gates playback — owner:
processing runs during playback at reduced priority), #167 startup floor, memory
guard, one heavy child at a time (shared slot). Precondition = both stems present
(the ambient bed + original-voice channels the 4-stream mix needs); while missing
it parks at `stems` and raises `stem_manual_priority`. Live INPUT is the ORIGINAL
`audio_file_path` (not a stem). `dub_engine="gemini-live-translate"`.

- **Chunk plan (Rust owns it):** the worker runs ffmpeg `silencedetect` (a light
  off-slot pass at BELOW_NORMAL), parses it with `chunk_plan::parse_silencedetect`,
  and `chunk_plan::plan_chunks` cuts at pauses ≥ 700 ms into chunks ≤ 8 min (Live
  session headroom), never mid-speech; the plan JSON is the child's `--chunk-plan`
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
`wait_with_stall_timeout` never kills a healthy mid-chunk stream (a chunk can be
8 min with no other work-dir write). Assembles `<base>_dub.flac` at **48 kHz
STEREO loudnorm -16** (must match the stem format `StemMixReader` requires), writes
`<base>_dub_transcripts.json` (D3), and prints the summary JSON on stdout (the ONLY
stdout line — logs go to stderr). Key ONLY via `GEMINI_API_KEY` env (`bootstrap::
ensure_genai` pins `google-genai==2.24.0`, idempotent, never triggers the heavy
qwen/torch reinstall). Cost ~$0.037/min.

## Playback (`stems/reader.rs` + `stems/control.rs`)
When `dub_path(audio)` (`<base>_dub.flac`) exists AND both stems exist,
`open_audio_stream` opens a **4-stream** `StemMixReader` `[original, vocals,
instrumental, dub]` fed `KaraokeControl::dub_gain_handles()`. `dub_gains(r) =
(0, 1−r, 1, r)`: original full-mix silent, vocals = original voice, instrumental =
ambient bed, dub = SK. Default r=1.0 (dub only). `PATCH /api/v1/videos/{id}/dub-mix`
persists to DB AND sends `EngineCommand::SetDubMix{video_id, ratio}` →
`engine.set_dub_mix` → `control.set_dub_ratio` (live, no pipeline reopen — the #186
seam, one stream wider). Any 4-stream open failure degrades to the stem/plain mix.

## Cap note
`lib.rs` was at exactly 1000 lines, so the `EngineCommand` match was extracted to a
sibling `engine_dispatch.rs` (free `dispatch(&mut engine, cmd)`) before adding the
`SetDubMix` arm + the dub-worker spawn.

## Known gotchas (D4, verified on win-resolume 18.9.2026)

- **A dub for a video > 15 min STALLS at `dub_status=stems`.** The 4-stream mix
  precondition needs stems, but the stem worker caps separation at
  `STEM_MAX_DURATION_MS` (15 min, `stems/worker.rs`) and marks a longer file
  `stem_status=unsupported`, so the dub never leaves `stems`. Sermons are long by
  nature — the sample "Morning Prayer & Devotion" (40 min, video 344) hit exactly
  this. Open design question (main's call, filed on #183): the dub-only default
  (r=1.0) does NOT need the vocals stem — degrade gracefully to a 2-stream
  `[original, dub]` / dub-only mix when stems are unsupported/absent, so long
  sermons are dubbable without a multi-hour (or capped-out) stem pass. Until then,
  prove the chain on a SHORT (≤ 3 min) speech video.
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
