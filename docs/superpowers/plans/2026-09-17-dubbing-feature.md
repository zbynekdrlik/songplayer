# Dubbing feature ("Dabing") — implementation plan (sub-project A of #174)

> **For agentic workers:** each ticket D1–D6 is ONE autopilot-worker lane (sequential mode). Work RED→GREEN per task, TIER-0 (no local cargo compile; `cargo fmt --all --check` only; after `cargo fmt --all` run `git checkout -- crates/sp-server/src/db/models.rs`), 1000-line cap (`api/routes.rs` 999, `lyrics/worker.rs` 998, `db/models.rs` 997, `playback/mod.rs` 996 — NEVER add lines there; use sibling `_impl`/`#[path]` modules), one push per lane, foreground bounded CI wait, box verification via MCP only.

**Goal:** any video can be marked for dubbing; a priority chain produces voice/ambient stems + EN transcript + SK translation + an SK dub track; playback mixes original voice / dub / ambient live with an operator slider; a Dabing section lets the operator paste a URL for a priority add and play it on its own NDI output.

**Spec:** `docs/superpowers/specs/2026-09-17-dubbing-feature-design.md` (v2, owner-approved with amendments on #174). **Deviation from spec §3 (main decision):** dub state lives as columns on `videos` mirroring the existing stems columns (V24 pattern, `db/models_stems.rs`), not a separate `dub_tracks` table — one row per video, same query style, no joins.

**Codebase seams (mapped 17.9.2026):**
- `playlists.kind` exists (`'youtube' | 'custom'`, V19) → add `'dabing'`; seed like `startup.rs::ensure_live_playlist_exists` (SP-live).
- Bare-URL add exists: `POST /api/v1/videos/import` (`api/routes.rs:521-585`, UI `components/import_url_box.rs`) inserts a `videos` row into an existing playlist; the download worker picks it up in ≤ 5 s.
- Stems: `stems/worker.rs` (10 s tick, oldest-first, no manual bucket), `stems/mod.rs::stem_paths` (`{base}_vocals.flac` / `{base}_instrumental.flac`), V24 columns `vocals_file_path, instrumental_file_path, stem_status, stem_attempts, stem_next_attempt_at`, mixer `crates/sp-decoder/src/audio/karaoke.rs::KaraokeAudioReader` (2 streams, atomics gains), chooser `stems/reader.rs::open_audio_stream`, control `stems/control.rs::KaraokeControl`.
- Lyrics: transcript `lyrics/g35t_client.rs::transcribe_words` → `g35t_transcript.rs` (group on silence) → `LyricsTrack{lines[{start_ms,end_ms,en,sk,words:None}]}`; translation `lyrics/translator.rs::translate_via_claude` (+ `worker_translation.rs`); priority bucket pattern `lyrics/reprocess.rs::fetch_bucket_manual` (`lyrics_manual_priority=1`).
- Heavy slot: `lyrics/heavy_slot.rs` `Semaphore(1)` process-global; `heavy_plan.rs::HeavyStepPlan::for_activity` (priority class / timeouts).
- Playback seam: `playback/pipeline.rs:547-560` and `pipeline_paced.rs:186-202` call `open_audio_stream(audio_path, &control)` → `SplitSyncedDecoder::new(video, audio)`; commands `engine_command.rs::EngineCommand::{EnsurePipeline, PlayVideo, SetKaraoke}`.
- UI: `sp-ui/src/store.rs::DashboardStore`, components `playlist_card.rs`, `video_list.rs` (259), `karaoke_control.rs` (130), `import_url_box.rs`, pages `dashboard.rs/live.rs/lyrics.rs/settings.rs`; mock `e2e/mock-api.mjs`; post-deploy `e2e/post-deploy.spec.ts`.

---

## D1 — data model + Dabing section + priority add + row toggle (server + list UI)

**Files:** new `db/migrations_v26.rs`? no — append `MIGRATION_V26` to `crates/sp-server/src/db/mod.rs` (that file is 3xx lines, fine); new `crates/sp-server/src/db/models_dabing.rs` (`#[path]` included from `models.rs` like `models_stems.rs`); new `crates/sp-server/src/api/dabing.rs` (routes) registered in `api/mod.rs` after L215; `startup.rs` seed; `sp-ui/src/pages/dabing.rs` + `components/dabing_list.rs` + `components/dub_toggle.rs`; `sp-ui/src/api.rs` wrappers; `e2e/mock-api.mjs` endpoints; `e2e/dabing.spec.ts`.

1. **Migration V26 (RED: migration test expects schema_version 26 + columns).** `ALTER TABLE videos ADD COLUMN dub_requested INTEGER NOT NULL DEFAULT 0; dub_status TEXT NOT NULL DEFAULT 'none' /* none|queued|stems|transcript|translation|synth|ready|failed */; dub_file_path TEXT; dub_engine TEXT; dub_voice_ref_path TEXT; dub_mix_ratio REAL NOT NULL DEFAULT 1.0; dub_error TEXT; dub_attempts INTEGER NOT NULL DEFAULT 0; dub_next_attempt_at TEXT; dub_requested_at TEXT;` plus `ALTER TABLE videos ADD COLUMN stem_manual_priority INTEGER NOT NULL DEFAULT 0;` (stems worker gets a manual bucket, D3 uses it). Tests in `db/tests.rs` style: run migrations on `create_memory_pool`, assert columns via `PRAGMA table_info(videos)`.
2. **`models_dabing.rs`**: `DubRow {video_id, playlist_id, title, dub_status, dub_error, dub_mix_ratio, dub_file_path, stem_status, lyrics_present: bool}`, `set_dub_requested(pool, video_id, bool)` (sets `dub_requested`, `dub_status='queued'|'none'`, `dub_requested_at`, `stem_manual_priority=1`, `lyrics_manual_priority=1` when requesting), `list_dub_videos(pool) -> Vec<DubRow>` (all `dub_requested=1`, newest request first), `get_next_dub_job(pool) -> Option<DubRow>` (D4 uses), `mark_dub_status/mark_dub_ready/mark_dub_failed/record_dub_deferral`, `set_dub_mix_ratio`. Unit tests on the in-memory pool for each (state transitions, ordering).
3. **Seed the Dabing playlist**: `startup.rs::ensure_dabing_playlist_exists` — `kind='dabing'`, name `Dabing`, `ndi_output_name='SP-dabing'`, `is_active=1`, idempotent; test mirrors the SP-live seed test.
4. **API (`api/dabing.rs`)**: `GET /api/v1/dabing` → `{playlist_id, videos:[DubRow…]}`; `POST /api/v1/dabing/import {url}` → calls the existing import path (`import_video` logic extracted into a pub(crate) fn in a sibling `api/routes_import.rs` if it cannot be reused without touching `routes.rs` line count — do NOT grow `routes.rs`) with the Dabing playlist id, then `set_dub_requested(true)`; `PATCH /api/v1/videos/{id}/dub {requested: bool}`; `PATCH /api/v1/videos/{id}/dub-mix {ratio}`. 202/204 semantics like the other routes; unit tests for the pure request parsing.
5. **Chain state derivation (pure, tested):** `dub_chain_state(row) -> DubChainState` = `Queued | Stems | Transcript | Translation | Synth | Ready | Failed(reason)` from `dub_status` + `stem_status` + lyrics presence — displayed as `stiahnuté → stemy → prepis → preklad → dabing → pripravené`.
6. **UI**: page `Dabing` in the nav (next to Lyrics): `import_url_box`-style paste field wired to `POST /api/v1/dabing/import`; list of dub videos (newest first) with the state chain glyph row, error text, and a **Prehrať** button (`EngineCommand::PlayVideo` via the existing `post_live_play_video` wrapper generalised to any playlist id); in `video_list.rs` rows (any playlist) a `Dabing` toggle (`dub_toggle.rs`, PATCH). Store: `dabing: RwSignal<Vec<DubRow>>` refreshed by a 2 s poll like the lyrics queue.
7. **E2E**: `e2e/dabing.spec.ts` against the mock (page renders, paste → row appears with `queued`, toggle PATCH observed, zero console errors); mock endpoints in `mock-api.mjs`. Post-deploy: the Dabing page renders on the box with the seeded playlist (SP-dabing).

## D2 — modern mixer component (replaces the karaoke panel visuals; keeps the #177 state contract)

**Files:** new `sp-ui/src/components/mixer.rs` (+ `mixer_channel.rs`), CSS in `sp-ui/style.css`, replaces `karaoke_control.rs` usage in `dashboard.rs` and the Dabing page; `e2e/karaoke.spec.ts` updated + `e2e/mixer.spec.ts`.

1. Load `frontend-design` skill (owner: today's stems mixer "hrozne škaredý"). One component `Mixer { channels: Vec<Channel{label, gain: RwSignal<f32>, enabled}>, presets, title, state_line }`; songs → `vokál / inštrumentál` bound to `KaraokeControl` (mode + vocal gain mapped onto the two faders; the mode select becomes presets `Plný mix / Karaoke / Iba vokály / Iba hudba`); dubbed videos → `originál hlas / dabing / ambient` bound to the dub ratio API (D4).
2. Big touch targets (≥ 44 px), vertical faders or one crossfade + per-channel level, live VU from the now-playing payload if present (else none — no fake meters), presets row, title shows the song it controls, disabled state with the reason (`#177` contract: stems state per song).
3. Playwright: faders change the API values (mock), presets apply, disabled when the song has no stems; zero console errors. Post-deploy: the mixer renders on the playing card.

## D3 — long-form transcript + translation for dub videos (reuse lyrics chain, priority buckets)

**Files:** `crates/sp-server/src/lyrics/long_form.rs` (new: chunk plan), `lyrics/worker_g35t.rs` (route a `dub_requested` video through long-form), `lyrics/reprocess.rs` (manual bucket already first — `set_dub_requested` sets `lyrics_manual_priority=1`), `db/models_stems.rs` (`get_next_video_for_stems` adds a manual bucket on `stem_manual_priority` FIRST), `scripts/lyrics_worker.py` (chunked transcript), tests.

1. **Stems manual bucket (RED/GREEN):** `get_next_video_for_stems` returns `stem_manual_priority=1` rows first (then the existing oldest-first); test with two rows.
2. **Long-form chunk plan (pure, tested):** `long_form::chunk_plan(duration_ms) -> Vec<Chunk{start_ms,end_ms}>` — 10-min chunks, 20 s overlap; `merge_words(chunks_words) -> Vec<AsrWord>` de-duplicates overlap by timestamp (words in the overlap window keep the EARLIER chunk's copy). Sentence grouping for speech: `group_sentences(words) -> lines` on punctuation/pauses ≥ 700 ms, max 14 words per line (the wall reads two lines).
3. **Route:** in the lyrics worker, a `dub_requested=1` video skips the vocal isolation step (voice stem from D1/D3 stems is the ASR input when present, else the full mix) and uses the long-form plan; translation via the existing `translate_via_claude` sentence-level in batches of ≤ 60 lines; output the normal `LyricsTrack` (`source = "gemini-3-5-transcribe/longform"`, `words: None`) so the wall + dashboard render EN/SK unchanged. `dub_status` advances `stems → transcript → translation`.
4. Resumable per chunk (the #171 pattern: `<cache>/<id>_longform/` work dir, `chunk_N.json`), stall-based timeout, child stderr tail on failure → `dub_status='failed'`, `dub_error`.
5. Tests: chunk plan boundaries, overlap merge, sentence grouping, state advance; CI mutation shards on the new pure fns.

## D4 — SK dub synthesis + timing fit + 3-stem playback + mix ratio

**Files:** new `crates/sp-server/src/dabing/{mod.rs, engine.rs, soniox.rs, timing_fit.rs, worker.rs, voice_ref.rs}`; new `crates/sp-decoder/src/audio/dub_mix.rs` (`DubAudioReader`, 3 streams); `stems/reader.rs::open_audio_stream` chooses `DubAudioReader` when the video has `dub_file_path` and the dub ratio control is active; `stems/control.rs` gains a `DubControl {ratio: Arc<AtomicU32>}` (global, set per play from the video's `dub_mix_ratio`, PATCH updates both live + DB); settings keys `soniox_api_key` (secret, never logged), `dub_mix_default`, `dub_engine`; `scripts/dub_worker.py` only if the engine needs Python (Soniox is plain HTTPS — do it in Rust with reqwest).

1. **`DubEngine` trait (pure interface, mock-tested):** `async fn clone_voice(&self, sample_wav: &Path) -> VoiceRef`; `async fn synthesize(&self, text: &str, voice: &VoiceRef, lang: "sk") -> SynthResult{wav: Vec<u8>, duration_ms}`; `soniox.rs` implements it against Soniox TTS v2 + voice cloning (endpoints and payloads verified from the vendor docs at implementation time; API key from settings via header, never on a command line or in logs — print lengths only).
2. **Voice reference (pure selection, tested):** `voice_ref::pick_clean_span(voice_stem_rms: &[f32], window_ms) -> (start_ms,end_ms)` — a 15–20 s span with the highest voiced ratio and no long pauses; cut with ffmpeg to `<base>_voiceref.wav`.
3. **Timing fit (pure, tested — spec §6):** `timing_fit::place(lines: &[LineSpan{t0,t1}], synth_durations: &[u32]) -> Vec<Placement{at_ms, tempo: f32, cut_at: Option<u32>, overflow: bool}>` — fits / tempo ≤ +15 % / overflow to next `t0 − 150 ms` / hard-cut with 50 ms fade; stats `{fit, tempo, overflow, cut}` counts.
4. **Dub worker (`dabing/worker.rs`):** 10 s tick; `get_next_dub_job` (status `translation` done ⇒ `synth`); per line: synthesize SK (`sk` text from the `LyricsTrack`), apply tempo via ffmpeg `atempo` when needed, place on a silent timeline of the video's duration, write `<base>_dub.flac` (loudnorm to the voice stem's LUFS), set `dub_file_path`, `dub_status='ready'`; runs inside `heavy_slot` at `HeavyStepPlan::for_activity` priority; resumable per line (`<cache>/<id>_dub/line_N.wav`), failures → `failed` + `dub_error` + backoff (`dub_attempts`, `dub_next_attempt_at`).
5. **3-stem mixer (`dub_mix.rs`, unit-tested with mock streams like `karaoke.rs`):** `out = clamp(voice·(1−r) + dub·r + ambient·1)` with `r` read per chunk from `DubControl`; `open_audio_stream` picks it when `dub_file_path` exists (voice = `vocals_file_path`, ambient = `instrumental_file_path`); FullMix fallback if any stem is missing/broken (logged).
6. **API/UI:** `PATCH /api/v1/videos/{id}/dub-mix {ratio}` (D1 route) now also updates the live `DubControl` via `EngineCommand::SetDubMix{video_id, ratio}`; the D2 mixer binds the three faders; presets `len dabing (1.0) / 50/50 / originál (0.0)`.
7. Tests: engine mock round-trip, timing fit table, mixer math, worker state machine on the in-memory pool; post-deploy E2E: a dub-ready fixture shows the 3-channel mixer.

## D5 — `sp-dabing` OBS scene + NDI output + box acceptance on the sample URL

1. Create the OBS scene `sp-dabing` with an `ndi_source` input `sp-dabing_video` = `RESOLUME-SNV (SP-dabing)` via `obs-create-input` (MCP), bounds `OBS_BOUNDS_SCALE_INNER 1920×1080`, alignment 5 — never touch legacy `yt*` scenes; verify with `obs-get-scene-items`.
2. Box acceptance: paste the owner's sample URL (https://www.youtube.com/watch?v=Dhp-qrZDK1g) into the Dabing page → chain reaches `pripravené` (record durations per step); cut OBS to `sp-dabing`, Prehrať → wall shows EN/SK subtitles, mixer 1.0 = only SK audible, 0.0 = only original (verify by ear on the box's monitor output or by reading the mixer gains + a short capture); restore the program scene after.
3. Post-deploy E2E: Dabing page lists the sample as ready; the SP-dabing output appears in `/api/v1/ndi/health`.

## D6 — cross-worker priority gate (only if D1/D3 buckets are not enough)

`heavy_slot::acquire_slot` gets an optional `yield_if_dub_pending` check: the lyrics and stems workers skip a tick while `models_dabing::dub_pending(pool)` is true and the dub chain is not the caller. Tested with the in-memory pool + a paused-time tokio test.

---

## Verification (whole feature)
1. CI green on every lane (Gate, Deploy, E2E, mutation shards).
2. Box: the sample dabing processed end-to-end ahead of the lyrics/stems queues; the Dabing page shows the chain; the wall on `sp-dabing` shows EN/SK subtitles; the mixer slider changes what is heard; version on the DOM.
3. No wall fps / genlock telemetry change while the dub chain runs (heavy slot at BELOW_NORMAL, #162).
