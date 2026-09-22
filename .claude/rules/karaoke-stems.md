---
paths:
  - "crates/sp-server/src/stems/**"
  - "crates/sp-decoder/src/audio/stem_mix*.rs"
  - "crates/sp-server/src/playback/karaoke.rs"
  - "scripts/stem_worker.py"
---

# Karaoke stem separation (#14) — separator choice, gotchas, architecture

Karaoke mode lowers or isolates vocals during playback by mixing two ML-separated
stems into the wall's NDI audio. Stems are generated once, in the background, and
stored next to the mix sidecar.

## Separator: Mel-Band RoFormer "Kim" (`vocals_mel_band_roformer.ckpt`, MIT)

Chosen 2026-09-14 after a fresh SOTA survey + on-box measurement (owner directive
on #14: "use what is actually best on the day, not what was good 5 months ago").

- **Why Kim, not the on-box BS-RoFormer viperx (`model_bs_roformer_ep_317_sdr_12.9755.ckpt`):**
  quality is within measurement noise (MVSep Multisong vocals 11.01 vs 10.87,
  instrumental 17.32 vs 17.17 — Kim is marginally *ahead*), but Kim is **MIT**
  while viperx has no published license (Boosty paywall, "dev-build only, no
  commercial grant"). Karaoke stems go into the NDI/broadcast output, so a clean
  commercial license matters — unlike the lyric-alignment vocal isolation, whose
  16 kHz mono output is internal + throwaway and still uses viperx.
- Higher-SDR options exist (MVSep 124-band, becruily "deux") but are blocked by
  "no public weights" / "non-commercial" — do NOT chase them.
- Loads via `audio-separator` (already on the box in `lyrics_venv`), auto-downloads
  the checkpoint (~913 MB) on first `load_model`.

**Measured (dev2 RTX 5050 8 GB, 3 real songs, 2026-09-14):** peak VRAM ~3.0 GB
(fits 8 GB with huge headroom), ~8× realtime, additivity residual −119.6 dB.

## Gotchas (measured, do not relearn the hard way)

- **Stem picker: match the PARENTHESIZED token, never the raw filename.** Kim's
  model name *contains* the substring `vocals`, so audio-separator writes
  `song_(vocals)_vocals_mel_band_roformer.flac` + `song_(other)_vocals_mel_band_roformer.flac`.
  A naive `"vocals" in filename` picker grabs the wrong file. Match `(vocals)` and
  `(other)`/`(instrumental)` in parentheses (`scripts/stem_worker.py::_stem_token`).
  For a vocals model, `(other)` IS the instrumental/accompaniment.
- **Additivity: the instrumental is `mix − vocals` (bit-exact, −119 dB).** So
  `vocals + instrumental == mix`, i.e. the stems are "loudnorm-matched to the mix"
  BY CONSTRUCTION. Do NOT re-loudnorm the stems (breaks additivity, distorts the
  balance, risks clipping); the source mix is already −14 LUFS. FullMix plays the
  real mix file; the mixer clamps to [-1, 1] for the rare overshoot.
- **Native output is 44.1 kHz** (model rate); the mix + the decoder require
  **48 kHz stereo**, so `stem_worker.py` resamples every stem to 48 kHz stereo
  before writing FLAC (PCM_24).

## #184 round G2 — each reader FAMILY owns its memory; NO global "active kind" (SUPERSEDES round G1)

The ONE console keeps **TWO** remembered fader triples (`sp_core::mixer_model::
MixConsole { song, dub }`, defaults song `(1,1,·)` Plný mix / dub `(0,1,1)` Len
dabing). The **invariant**: each reader family is fed from its OWN memory, ALWAYS —
the 3-stream song reader (`gains[3]`) from the SONG memory, the dub readers
(`dub_gain_atomics[4]` + `dub2_gain_atomics[2]`) from the DUB memory. The strip edits
the memory of ITS item's kind.

- **Why G1's global "active kind" was wrong:** the wall runs SEVERAL pipelines at
  once, so "the playing item's kind" is NOT a single value. G1 published ALL THREE
  gain sets from the ONE active memory, so a song opening ANYWHERE re-published the
  dub readers' atomics from the SONG memory (`stream_gains_dub((1,1,1)) = [1,0,0,1]`
  = doubled voices on SP-dabing), and `select_mix_kind_for_video` only re-selected on
  a fresh open, so `play 344 → PATCH → play a song elsewhere → play 344 again` left
  the kind stale and the API edited the wrong memory.
- **`MixControl`** holds `song_faders[3]` + `dub_faders[3]` + the derived gain
  atomics, NO `active`. `set_faders(kind, f)` writes ONE memory and republishes ONLY
  that kind's family (`publish_song` / `publish_dub`); `faders(kind)` / `console()`
  (both memories). `kind()` / `select_kind` and `mixer_model::mix_kind_for_dub` +
  `playback/mix.rs::select_mix_kind_for_video` (the item-open hook) are **DELETED**.
  Boot still reads the five V28 settings (`mix_song_*` / `mix_dub_*`); no migration
  change. `EngineCommand::SetMix { kind, faders }`.
- **API:** `GET /api/v1/mix` → `{song:{vokaly,podklad}, dub:{vokaly,podklad,dabing},
  …}` (no `kind`, no flat faders). `PATCH {kind:"song"|"dub", vokaly?, podklad?,
  dabing?}` — `kind` REQUIRED (400 without it; `dabing` on `song` → 400), applies
  `control.set_faders(kind, target)` directly (the round-G race rule) + persists ONLY
  that kind's keys after the live push.
- **UI:** `live_mixer.rs` — the strip's kind is its `is_dub` Memo; `load` reads its
  memory from the GET's `song`/`dub` object, every PATCH carries `kind`, the kind-flip
  reload Effect stays.

## #184 round G — ONE mixer console (SUPERSEDES the karaoke-MODE model below)

The karaoke MODE + `KaraokeControl` + `preset_gains` + `KaraokeMode` enum +
`GET/POST /api/v1/karaoke` + `EngineCommand::SetKaraoke` + `KaraokeStateChanged`
are **DELETED**. The mixer is now THREE independent faders that ARE the state:

- **Model = `sp_core::mixer_model`** — `MixFaders { vokaly, podklad, dabing }`
  (each `0..=1`, clamped; ANY non-finite → the default `(1,1,1)`). The per-stream
  gains are DERIVED, not preset-shaped:
  - `stream_gains_song(f) -> [original, vocals, instrumental]` = `[1,0,0]` when
    `vokaly==1 && podklad==1` (bit-exact original, the old FullMix) else
    `[0, vokaly, podklad]`.
  - `stream_gains_dub(f) -> [original, vocals, instrumental, dub]` = `[1,0,0,dabing]`
    when both full else `[0, vokaly, podklad, dabing]`.
  - `stream_gains_dub_no_stems(f) -> [original, dub]` = `[vokaly, dabing]` — the
    `vokaly` fader IS the whole original bed, **NO floor** (the −18 dB
    `DUB_ORIGINAL_FLOOR` is GONE).
  - Presets are fader SNAPSHOTS (`SONG_PRESETS` always, `DUB_PRESETS` only with a
    ready dub); `preset_for_faders(f, has_dub)` highlights within 0.01
    (`karaoke_low`: `vokaly<1 && podklad==1`), dub presets win when a dub is
    present. `fader_availability(stems_ready, dub_ready)` → which faders are live.
- **Live control = `stems::control::MixControl`** (was `KaraokeControl`): three
  fader atomics + the three DERIVED gain sets (`gains[3]`, `dub_gain_atomics[4]`,
  `dub2_gain_atomics[2]`). `set_faders(f)` recomputes ALL THREE in lock-step from
  the pure `stream_gains_*`; `gain_handles` / `dub_gain_handles` /
  `dub_over_original_gain_handles` keep their stream orders. The #186 no-reopen
  seam is unchanged. Restored at boot from settings `mix_vokaly`/`mix_podklad`/
  `mix_dabing` (`init_from_settings`).
- **API = `GET/PATCH /api/v1/mix`** (`api/mix.rs`): GET returns the three faders +
  stem progress + the #177 now-playing block; PATCH takes any subset
  `{vokaly?, podklad?, dabing?}` (live push FIRST via `EngineCommand::SetMix` +
  `engine.set_mix`, THEN the settings persist — the round-A order in
  `api/mix_apply.rs::apply_mix`). Broadcast is `ServerMsg::MixChanged`.
- **UI = `components/live_mixer.rs`** (ONE strip over the shared `Mixer`) —
  replaces `karaoke_mixer.rs` + `dub_mixer.rs`. Test ids `mix-vokaly` /
  `mix-podklad` / `mix-dabing`, presets `mixer-preset-<id>`, state line
  `karaoke-now-playing` (kept). Migration V27 folds the old
  `karaoke_mode`/`karaoke_vocal_gain` settings into `mix_*` and DROPs
  `videos.dub_mix_ratio`.

Everything below is the pre-round-G history; read it for the #186 seam mechanics
but treat `KaraokeControl`/`preset_gains`/`KaraokeMode`/`/api/v1/karaoke` as
DELETED names.

**Round-G gotchas (cost a review round):**
- **A partial `PATCH /api/v1/mix` reads the UNSPECIFIED faders from the
  process-global `MixControl`.** So the handler must APPLY `control.set_faders(target)`
  DIRECTLY (idempotent with the engine's own `set_faders`), not rely ONLY on the
  async `EngineCommand::SetMix` loop — otherwise a rapid 2nd partial PATCH reads a
  stale console (race), AND the axum test harness (`routes_tests::test_state` DROPS
  the engine receiver) never updates the global, so a partial-merge test can't pass.
- **A `preset_for_faders` test for a DUB preset MUST set `dabing` to the snapshot's
  pinned value** (`half` = `(0.5, 1, 0.5)`): dub presets match all three within
  0.01, so `(0.5, 0.995, 1.0)` does NOT match `half` (dabing 1.0≠0.5) — it falls to
  the song reading (`karaoke_low`). Song presets ignore `dabing`; dub presets pin it.

## Architecture

- **#186 — MODES ARE LIVE GAIN PRESETS, NEVER A REOPEN. The mixer opens every
  stream that exists ONCE; a preset change only writes gain atomics.** This is the
  load-bearing invariant: the pre-#186 design baked the mode into WHICH reader
  opened, so a mode change sent `PipelineCommand::Play` at the cached position →
  decoder teardown + A/V resync → seconds of silence on the wall (owner: "zvuk na
  par sekund vypadne … neopuzitelne pre live"), and the fader only ever acted in
  `KaraokeLow` (the one mode sharing a live atomic). NEVER reintroduce a reload on
  a mode change; `set_karaoke` must not contain `PipelineCommand::Play`
  (`playback/karaoke.rs::mode_change_needs_reload` is the unit-tested guard, always
  `false`).
- **Mixing (`sp_decoder::StemMixReader`, `audio/stem_mix.rs`)** wraps N
  sample-aligned `AudioStream`s behind ONE `AudioStream`, so `SplitSyncedDecoder` /
  pacer / genlock / NDI submit are UNTOUCHED. `out = clamp(Σ stream_k·gain_k, -1,
  1)`; each stream's applied gain RAMPS linearly toward its live target atomic over
  `ramp_samples = sample_rate/20` frames (**50 ms**, ≤ `1/ramp_samples` per frame),
  so a preset change is a crossfade, never a click. A song opens at its current
  preset (no fade-in). Buffers each stream (FLAC packet boundaries differ); an ended
  stream mixes as silence; timestamps from a cumulative frame counter. #183 (dub)
  and #181 (D2 UI) reuse the SAME type with different streams — do NOT add a second
  mixer.
- **Live control (`crate::stems::control::KaraokeControl`)** is a process-global
  (mode = `AtomicU8`, `vocal_gain` = the stored fader position, `gains:
  [Arc<AtomicU32>; 3]` = the LIVE `[original, vocals, instrumental]` target gains).
  `set_mode` / `set_vocal_gain` publish `preset_gains(mode, vg)` to the triple; every
  playing `StemMixReader` holds clones of those atomics (`gain_handles()`) and ramps
  toward them — so a preset / fader change is heard mid-song with NO reopen.
  `preset_gains`: FullMix `(1,0,0)` (bit-exact original, no separation artefacts),
  KaraokeLow `(0,vg,1)`, VocalsOnly `(0,1,0)`, InstrumentalOnly `(0,0,1)`. `vg` only
  scales KaraokeLow vocals; the UI enables the fader in every stem preset and
  disables it only in Plný mix.
- **Reader seam (`crate::stems::reader::open_audio_stream`)** is MODE-INDEPENDENT
  (`stream_roles(vocals_exist, instrumental_exist)`): both stems present → open
  `[original, vocals, instrumental]` in a `StemMixReader` fed the control's three
  gain atomics; stems missing (or an incomplete pair) → a plain `SymphoniaAudioReader`
  on the original, and presets are a no-op for that song (#177 "nedostupné"). A stem
  present-but-unreadable degrades to the original mix (defence in depth). Called from
  both `pipeline.rs` and `pipeline_paced.rs`.
- **Modes (`sp_core::playback::KaraokeMode`):** FullMix / KaraokeLow / VocalsOnly /
  InstrumentalOnly. The gain table lives in `stems::control::preset_gains` (3-tuple);
  the old 2-tuple `KaraokeMode::stem_gains` was DELETED with the two-stream reader.
- **Priority regime (#162, `lyrics/heavy_plan.rs`) — supersedes the idle-ONLY
  gate.** The single operator switch is `lyrics_processing_mode` = `low-priority`
  (DEFAULT) | `idle-only` (the pre-#162 behaviour, operator option only). The old
  `lyrics_gate_when_playing` boolean is REMOVED (both values folded to
  low-priority by `MIGRATION_V25`). Owner ruling (2026-09-15): processing must
  keep running during playback at a priority that cannot disturb the wall —
  *stopping is not the solution* (the idle-only gate starved the queue because
  SongPlayer/CG-OBS always play something). `HeavyStepPlan::for_activity(mode,
  activity)` decides per step: **low-priority + wall in use → CPU-only** (the
  script gets `--force-cpu` from `HeavyStepPlan::script_cpu_args`, forcing CPU
  in-process via `_force_cpu()` — the GPU is left untouched but CUDA stays
  initialized. NOT hidden via `CUDA_VISIBLE_DEVICES="-1"`: that CRASHED the box —
  torch/onnxruntime probe the driver, find no devices, the NVIDIA user-mode DLL
  unloads and a later stray call kills the process, `nvdxgdmal64.dll_unloaded`
  0xc0000005, every isolation/separation, win-resolume 2026-09-15) **+ BELOW_NORMAL_PRIORITY_CLASS (NOT IDLE: a child created in IDLE class is starved by working-set trimming — 0.02 cores, measured 2026-09-15) + thread cap
  `OMP/MKL/TORCH_NUM_THREADS = max(1, cores/4)`** (#162 07:40 ruling — MINIMAL
  load, not speed; 3 threads on the 12-core box) → the GPU is never touched, so
  no fps drop / TDR; **low-priority + wall idle → GPU + BELOW_NORMAL** (fast);
  **idle-only → GPU** (it defers instead of running on a busy wall).
- **CPU-plan timeouts are ×4 the GPU-sized base (#162, `heavy_step_timeout` in
  `heavy_plan.rs`).** Every heavy timeout (`aligner::isolation_timeout` for
  isolation + stem separation, mtl's fixed `TIMEOUT_SECS` 15 min) was sized for
  GPU speed (~8× realtime). On the win-resolume CPU a step measures ~3× realtime
  with 6 threads and ~5–6× realtime under the cpu-idle 3-thread cap (measured
  2026-09-15). A GPU-sized ceiling therefore KILLS a CPU job mid-run — it is
  deferred with backoff, retried, killed again forever (the live 10.5-min stem
  case: `separate-stems timed out after 1280 s`). So `heavy_step_timeout(plan,
  base)` keeps `base` for a GPU plan and returns `base * CPU_TIMEOUT_MULTIPLIER`
  (=4, saturating) for a cpu-idle plan (1280 s → 5120 s ≈ 85 min). Applied at
  every heavy spawn (isolation via `idle_gate_abort::isolation_step_timeout`,
  stems via `stems::worker::separation_timeout`, mtl via `mtl_aligner::mtl_timeout`
  threaded into `run_once`); the per-step INFO line logs the chosen `timeout=…s`.
  The abort→CPU re-run recomputes its OWN cpu-idle (×4) timeout, never the
  GPU-sized one it aborted under.
- **#171 SUPERSEDES the ×4 whole-song ceiling for ISOLATION + SEPARATION with a
  RESUMABLE + STALL-based timeout (mtl KEEPS the ×4 `heavy_step_timeout`).** The
  ×4 ceiling still killed ordinary 3–10 min songs mid-run and DISCARDED 40–77 min
  of work (a whole-song CPU run is 8–14× realtime, above the 8× budget). Fix, in
  two coupled halves — do NOT ship one without the other:
  1. **Resumable segments (Python).** `lyrics_worker.py preprocess-vocals` and
     `stem_worker.py separate` take `--work-dir` and split the input into fixed
     30 s / 2 s-overlap windows (`_segment_bounds`), isolate/separate each into
     `<cache>/<id>_isolation|_stemsep/seg_*.wav`, SKIP windows already present on
     start (logs `isolation resumed from chunk N/M`), load models ONCE, then
     stitch (`_stitch_segments`, weight-normalised linear crossfade) + atomic
     `os.replace` to the final output + remove the work dir. Stems stitch BOTH
     stems with IDENTICAL crossfade weights so `vocals+instrumental==mix`
     additivity holds by linearity. audio-separator exposes NO per-chunk resume
     hook (verified on box — `Separator.separate()` returns whole stems only), so
     the split/stitch is self-managed. The stitch/additivity math is pure numpy —
     **unit-test it LOCALLY on dev1** (`numpy` is present even though `soundfile`
     isn't): identity reconstruction + `v+i==mix` must be ~0.0 error before shipping
     to the no-compile box.
  2. **Stall timeout (Rust, `heavy_plan::stall_timeout`/`stall_limit`/`wait_with_stall_timeout`).**
     The per-segment progress files ARE the stall signal: `preprocess_vocals` /
     `separate_stems` no longer use `timeout(child.wait())` — they kill the child
     only when NO new segment has been written to the work dir for `stall_timeout`
     (CPU 900 s / GPU 300 s), NOT on total wall time; the whole-song figure
     survives only as an ETA log. A **model-load startup grace** (`STALL_STARTUP_GRACE_SECS`
     300 s, added by `stall_limit(plan, first_progress_seen)` until the first NEW
     segment of the run) prevents killing a slow cold start (esp. GPU) before its
     first segment → the kill-loop this ticket fixes. On a STALL the work dir is
     LEFT INTACT (never delete it — that is the resume state; only the final
     output write is atomic). Wiring the stall waiter into a child needs
     concurrent `drain_pipe` tasks so the pipe never deadlocks the child —
     BOTH `separate_stems` AND `preprocess_vocals` (isolation) now pipe+drain
     their stdio (#171 fixed the latter; it used to inherit stdio and LOSE the
     Python traceback). The shared draining/tail helpers live in
     `lyrics/child_output.rs` (`drain_pipe`/`tail_lines`/`failure_tail`).
  3. **Atomic segment write MUST pass `format="WAV"` (#171 — the exit-1 bug).**
     The resumable path writes each segment (and the final stitch) via a
     `<name>.wav.tmp` scratch + `os.replace`. `soundfile.write` infers the format
     from the file EXTENSION, and `.tmp` is UNKNOWN → `TypeError: No format
     specified and unable to get format from file extension` — so isolation ran
     the full ~14.5 min mel+dereverb inference for segment 0 and then died at the
     write, 0 segment files, EVERY song → full-mix base tier. `stem_worker` was
     unaffected only because `_separate_one_segment` already passed
     `format="WAV"`; the pre-#171 whole-song isolation wrote straight to a `.wav`
     output (no `.tmp`). Fix: the shared `_atomic_write_wav(path, audio, sr)`
     helper in `lyrics_worker.py` passes `format="WAV"`. Committed pytest:
     `scripts/tests/test_atomic_wav_write.py` (needs `soundfile`, added to the
     `eval-checks` CI deps) + `test_segment_stitch.py` (numpy-only).
  4. **A full-mix base-tier row must re-attempt ★ (#171 gap-1,
     `reprocess::fetch_bucket_fullmix_upgrade`).** A `gemini-3-5-transcribe/fullmix`
     row is `has_lyrics=1` at the CURRENT version, so it matches neither the null
     (has_lyrics=0) nor the stale (version<current) bucket — without a dedicated
     bucket it would never upgrade once isolation works. The lowest-priority 4th
     selector bucket re-picks it at most once/day (gated on `lyrics_processed_at`);
     a successful ★/vocal result overwrites `lyrics_source` and it leaves the bucket.
- **Sequential heavy-step guard (#162 07:40 crash, `lyrics/heavy_slot.rs`) — the
  box must NEVER be overloaded.** The lyrics worker (isolation + mtl) and the
  stem worker (separation) are two independent loops; once #162 removed the
  idle-only gate they each spawned a ~3.2 GB CPU RoFormer child in the same
  second → Windows low-virtual-memory → SongPlayer abort `0xc0000409` + OBS died
  (dump `SongPlayer.exe.4892.dmp`). Owner ruling (verbatim): *"spracovanie na
  pozadí je VŽDY sekvenčné … nikdy paralelne, a nikdy nesmie preťažiť PC"*.
  Three layers funnel EVERY heavy child spawn: (1) a **process-global
  `tokio::sync::Semaphore(1)`** (`acquire_slot`) — at most ONE heavy child
  (isolation / mtl / separation) runs process-wide; fair FIFO, so the two workers
  alternate; (2) a **`GlobalMemoryStatusEx` headroom check BEFORE the slot**
  (`heavy_step_memory_ok`) — both free physical RAM and free commit must be
  ≥ 4 GiB (`HEAVY_STEP_MIN_FREE_BYTES`), else the tick defers with NO backoff
  (lyrics `SongOutcome::WaitingForMemory`; stems leave the row pending, no
  `record_stem_deferral`) and re-checks next tick; (3) a **per-child Windows Job
  Object** (`assign_child_job`, `JOB_OBJECT_LIMIT_PROCESS_MEMORY` 6 GiB +
  `KILL_ON_JOB_CLOSE`) so an OOM kills the child, never the host. g35t /
  translation HTTP steps take NONE of these (not heavy). windows-sys is a
  cfg(windows) sp-server dep for the OS calls.
- **Heavy-slot priority — a dub job preempts a background separation (#184
  G0.1, `lyrics/heavy_slot.rs` + `stems/worker_yield.rs`).** The slot is fair
  FIFO, but a dub is an explicit operator request with a deadline while a stem
  separation is not, so a dub must NOT wait tens of minutes behind a cpu-idle
  separation. A process-global `DUB_SLOT_WANTED` flag (`dub_slot_wanted()` /
  `dub_slot_want_guard()`): the dub worker publishes it TRUE via the RAII guard
  right before `run_live_translate` (its Drop is the early-return safety net),
  and `acquire_on` clears it FALSE the instant the DUB step acquires the slot
  (gated by the pure `acquire_clears_dub_want(name)` — ONLY `"dub
  live-translate"`; a stem/isolation/mtl acquire never touches it), so the flag
  means precisely "a dub is queued behind the slot". While it is set: (a) the
  stem worker AND the lyrics worker DEFER their next heavy tick
  (`stem_tick_defers_to_dub` / a direct flag read) — start no new heavy step, no
  backoff, no DB write; (b) a RUNNING stem separation YIELDS between segments —
  `run_with_dub_yield` (reusing the #161 `AbortPolicy` 1 s poll) kills the child
  (`kill_on_drop`) and returns `StemStepResult::YieldedToDub`, which leaves the
  #171 resumable work dir + partial stems INTACT and the DB row pending (no
  backoff, `stem_attempts` unchanged) — so the dub acquires within ~1 s and the
  separation resumes from its segments later. The pure `yield_reason(dub_wanted,
  wall_busy, plan)`: a dub wins for ANY plan; a busy wall still wins for a GPU
  plan only (the #161 rule — a cpu-idle separation is never wall-yielded). mtl is
  NOT mid-run yielded (not resumable — it defers at the tick and finishes within
  its bound). No schema change, no `LYRICS_PIPELINE_VERSION` bump.
- **In-use-first tiered queue (#195, `db/models_stems_priority.rs` +
  `stems/queue_tiers.rs`).** The stem worker no longer picks by
  `stem_manual_priority DESC, id ASC` alone — with ~110 songs queued and
  ~40–60 min each, the playlist ON PROGRAM could wait days behind low-id videos
  of unused playlists. `get_next_stem_job(pool, on_program, recent)` consults
  tiers, first hit wins: **tier 0** manual/dub priority (`stem_manual_priority
  > 0`, ANY playlist — an explicit ask always wins) → **tier 1** the on-program
  playlist(s) → **tier 2** playlists played in the last `stems_recent_days` days
  → **tier 3** today's unrestricted oldest-first query (`get_next_video_for_stems`).
  Inner order inside every tier is unchanged (`stem_manual_priority DESC, id
  ASC`); an **empty id list SKIPS its tier (never `IN ()`)**. The tier inputs are
  built by the worker-agnostic free fns in `queue_tiers.rs`:
  `on_program_playlists` (snapshots with `state == Playing` — already reconciled
  to "playing AND on program", see the health-snapshot section in
  `obs-ndi-health.md`); `tier_inputs` blanks tier 1 to EMPTY during the #167
  startup grace (`!activity_known` or the heavy-step startup floor) so the first
  ~60 s never mis-tier; `recent_playlists` = `SELECT DISTINCT playlist_id FROM
  play_history WHERE played_at >= datetime('now', ?)`. The recency window is the
  `settings` key **`stems_recent_days` (default 7)**, read like
  `stem_worker_enabled` and parsed by the pure `recent_days_from` (default 7 on
  absent/non-integer, clamped to ≥ 0). `queue_position` (the panel's "vo fronte
  (N.)") gains the SAME tier rank (`(tier ASC, stem_manual_priority DESC, id
  ASC)`) via the inlined `tier_case_sql` CASE, so the chip stays truthful; the
  legacy 2-arg `models_stems::queue_position` delegates to it with empty tiers
  (the unrestricted oldest-first order). RED-on-a-no-compile-box: the whole
  feature ships behind the real const `RESTRICTED_TIERS` (RED `= 0` skips tiers
  1+2, GREEN `= 2`) — the `idle_gate_abort` wrong-constant pattern. No schema
  change, no `LYRICS_PIPELINE_VERSION` bump, no change to the separation itself.
- **Worker (`crate::stems::StemWorker`)** mirrors the lyrics worker: a 10 s tick
  that separates the next normalized song (tiered `get_next_stem_job`, #195
  in-use-first — was `get_next_video_for_stems` oldest-first — backoff-gated)
  under the priority regime. In `low-priority` it
  NEVER defers — it runs `stem_worker.py` on CPU-idle while the wall plays, GPU
  when idle. In `idle-only` it defers on a busy wall (reuses
  `idle_gate::wall_activity_from` + `GateLog::defer_settled`). VRAM cap
  (`LYRICS_GPU_MEM_FRACTION`) + CUDA-OOM→CPU fallback (`gpu_policy`) unchanged.
  Kill switch: `stem_worker_enabled` (default ON).
- **Idle-settle hysteresis (2026-09-14 incident, `idle_gate.rs`) — IDLE-ONLY mode
  only (#162):** a SINGLE idle sample was the bug — OBS scene re-evaluation (E2E
  `afterAll`), an operator scene switch or a song change flips every pipeline off
  `Playing` for a few seconds, and both workers resumed a multi-minute GPU job
  during that gap. In idle-only mode heavy work now resumes only after the wall
  reads idle continuously for `WALL_IDLE_SETTLE = 30 s` (shared clock via
  `GateLog::defer_settled`). Low-priority mode has no settle — it never defers.
- **Mid-job abort watcher (#161, `idle_gate_abort.rs`) — GPU jobs only (#162):**
  once a GPU separation is running (2–5 min child) `run_with_wall_abort` races it
  against a 1 s wall poll and kills the child (`kill_on_drop`) after 2 consecutive
  busy samples (~2 s debounce — NOT the 30 s settle). A CPU-idle job is NEVER
  aborted (it cannot disturb the wall). On abort: in `low-priority` the step
  re-runs IMMEDIATELY on CPU (no defer); in `idle-only` it re-queues with NO
  penalty (`StemStepResult::WallAborted` — partial stems deleted, DB row left
  pending: `stem_status` NULL, `stem_attempts` unchanged, re-picked when idle). A
  genuine separation failure still records the backoff deferral. The lyrics worker
  wraps its isolation + mtl steps the same way.
- **Duration cap (`STEM_MAX_DURATION_MS`, `stems/worker.rs`) — cap history
  15 → 120 min.** `process_next` checks `stem_duration_too_long(job.duration_ms)`
  right after picking the job, before the heavy-slot/memory-guard/spawn, and
  marks an over-cap row terminal `stem_status = 'unsupported'` (no retry, no
  backoff).
  - **2026-09-15: 30 → 15 min.** A 10-minute "warm-up" file pinned the heavy
    child's private bytes at ~5.0 GB against the (then) 6 GiB Job Object ceiling
    (`heavy_slot.rs::CHILD_JOB_MEMORY_LIMIT_BYTES` — CUDA context + several
    float32 copies of the whole mix), so allocations failed and the child crawled
    at 0.2 cores / ~200k page faults/s for 20+ min before timing out. Fixed in
    two parts: the ceiling went 6 → 10 GiB, AND the worker skipped anything over
    15 min. Rationale then: whole-file separation (memory pinned per file) + a
    single GPU-sized timeout that killed a long CPU run mid-way.
  - **Round G0 (#184, 2026-09-21): 15 → 120 min.** Owner ruling
    ("na vsetko sa dava rozdelenie podklady a vocaly co davame aj na songy") —
    EVERY video, incl. long dub videos, gets `podklad`/`vokály` stems. The old
    15-min rationale no longer holds: separation is now SEGMENTED (30 s
    resumable windows, memory is per-segment not per-file) and the timeout is
    DURATION-SCALED (×4 on a CPU plan) with heavy work at reduced priority
    during playback — so a 36-min video is ~5-12 min of low-priority, resumable
    work. 120 min is now a SANITY ceiling (a multi-hour livestream
    stays excluded), not a "songs only" limit. The literal stays a literal
    (`7_200_000`) so the mutation runner sees it. A boot one-shot
    `startup::requeue_unsupported_stems` → `models_stems::requeue_unsupported_within_cap`
    re-opens every `unsupported` row now within the cap (the SAME reset
    `enqueue_stems` uses; over-cap / unknown-duration / non-unsupported rows are
    left alone). NB the ONLY production caller of `mark_stems_unsupported` is this
    duration gate, so every `unsupported` row is a too-long row — there is no
    per-row error text to tell "too long" from "no vocals" apart.
- **DB (V24):** `videos.{vocals_file_path, instrumental_file_path, stem_status,
  stem_attempts, stem_next_attempt_at}`. `stem_status` NULL=pending → done /
  failed (retryable) / unsupported (terminal). Paths are also derived
  deterministically by `stems::stem_paths` so the reader needs no DB round-trip.
- **API:** `GET/POST /api/v1/karaoke` → `EngineCommand::SetKaraoke` →
  `PlaybackEngine::set_karaoke` (write the live gain atomics via the control +
  persist + broadcast `KaraokeStateChanged`; NO reload on a mode change since
  #186). Dashboard: `components/karaoke_control.rs`.

## Per-song stems state contract (#177) — #181 D2 MUST keep it

The dashboard karaoke panel binds to the SELECTED playlist's now-playing song and
shows whether ITS stems are ready; the D2 modern mixer (#181) replaces the
component's visuals but MUST preserve this state contract.

- **State enum (`db::models_stems::StemsState`, pure `stems_state_of`):**
  `Ready | Queued | Processing | Unavailable | Failed`, wire strings
  `ready/queued/processing/unavailable/failed`. Precedence: **Processing**
  (live) → **Ready** (both stem files on disk, regardless of recorded status) →
  **Unavailable** (`stem_status='unsupported'`) → **Failed** (`'failed'`) →
  **Queued** (NULL/pending). `stems_state_of` takes `is_processing` — NOT
  `stem_next_attempt_at`: there is deliberately no DB `'processing'` status (a
  crash would strand it), so the only honest source of ⚙ is the worker's live
  in-flight id (`stems::progress` — a process-global set/cleared around the
  separation child, mirror of `stems::control::global()`).
- **Now-playing source:** `now_playing::global()` (a process-global registry the
  engine writes on `Started`, clears on Stop — a Pause KEEPS the entry, since a
  paused song is still the panel's current song) — read by
  `GET /api/v1/karaoke`, which returns `now_playing: [{playlist_id, video_id,
  title, stems_state, stems_error, queue_position}]`. `stems_error` is DERIVED
  (there is no per-song stem error column) — a followup could add
  `stem_last_error` if the owner wants the real text.
- **Videos payload:** `Video.stems_state` is additive + `#[serde(default)]`;
  populated by `api/videos.rs` (NOT `row_to_video`) from
  `models_stems::stems_state_map`, which only marks stem-relevant rows
  (normalized+audio, or already has a stem file).
- **Enqueue:** `POST /api/v1/stems/{id}/enqueue` (`models_stems::enqueue_stems`)
  resets the row to eligible (`stem_status=NULL, attempts=0, next_attempt=NULL`)
  so the oldest-first worker picks it next tick. It does NOT jump the queue
  (selector is oldest-first by id; manual priority is #182), so the UI button is
  labelled **"Zaradiť do fronty"**, not "…teraz".
- **UI gate:** the mode `<select>` + vocal-gain slider are `disabled` unless the
  selected song's `stems_state == "ready"`; the enqueue button shows only for
  `unavailable`/`failed`. The E2E mock drives every state via
  `POST /__mock/karaoke-now-playing`.

## TIER-0 mutation gotchas for the mixer (learned #186)

The diff-scoped mutation gate caught two classes the no-compile box can't:

- **A reader CHOICE that only differs by TYPE is not observable through
  `AudioStream`** — `StemMixReader` and `SymphoniaAudioReader` both report 48 kHz
  stereo, and the box fixtures are SILENT, so a test that opens a reader and reads
  `sample_rate`/`channels`/output cannot tell which was chosen. Mutants on the
  choice (`roles.len() < 3`, `delete match arm`) then SURVIVE. Fix: extract the
  decision into a pure fn returning an OBSERVABLE enum
  (`reader.rs::audio_source_kind -> {StemMix,PlainMix}`), unit-test it
  exhaustively (kills delete-arm + fn-replacement — the enum return is
  defaultable to a wrong variant, so the test catches it), and consume it via a
  2-variant match with **no `_` wildcard** (deleting an arm → non-exhaustive →
  unviable; a `_` would keep the delete viable). Never gate on a NUMERIC
  comparison against a value whose domain is only 2 points (`len` ∈ {1,3}) —
  `<`/`==`/`<=`/`>=` against 3 are equivalent mutants there.
- **An even-division ramp can't distinguish snap-comparison mutants.** A 0→1 ramp
  with step = 0.1 hits exact multiples, so `<=`/`<`/`>=` in the snap test read
  identically. Add a PARTIAL-target test (0→0.25, step 0.1 → 0.1, 0.2, snap 0.25,
  never 0.3). And `cur + step.copysign(tgt-cur)` (one branch) instead of
  `if tgt > cur {…} else {…}` removes the `>` equivalent mutant for `>=`.
- **A sentinel-value mutant needs a test at the COLLIDING value (#177).** A
  `const NONE: i64 = -1` in-flight signal has `delete -` → `NONE = 1`. A test
  with a big distinctive id (`begin(4242)`) SURVIVES it (4242 ≠ both sentinels).
  Kill it with the value that COLLIDES with the mutated sentinel: `begin(1)` must
  report `Some(1)` — under `NONE = 1` the real id 1 is swallowed as "nothing".
  Serialize such process-global tests with a module `static SERIAL: Mutex<()>`
  (lock both the old and new test) so a parallel test can't stomp the exact read.
- **A boolean-operator mutant in a FILTER predicate needs a row where inclusion
  hinges on THAT operator (#177).** `stems_state_map`'s
  `if !(normalized && has_audio) && !has_stem { return None }` had three mutants
  (`||`→`&&` on `has_stem`, `&&`→`||` on `normalized && has_audio`, `delete !`)
  survive because the existing test inserted only normalized+audio rows (the
  `!(true)=false` short-circuit hid the has_stem clause). Fix: rows where the
  operator decides — a one-stem-file / not-normalized / no-audio row must be
  INCLUDED (kills `||`→`&&` and `delete !has_stem`); a normalized / no-audio /
  no-stem row must be OMITTED (kills `&&`→`||`).

## Reading the live stems queue on win-resolume (no sqlite3 on the box)

The box has **no `sqlite3.exe`**, and `songplayer.db` is locked by the running
process — so verify the stems queue through the HTTP API (SongPlayer serves on
port **8920**, `sp_core::config::DEFAULT_API_PORT`), never by opening the DB:

- On-program playlist: `GET /api/v1/ndi/health` → the pipeline whose
  `state == "Playing"` (already reconciled to "playing AND on program"; all
  others read `Paused`/`Idle`). That id is tier 1.
- Queue counts + now-playing positions: `GET /api/v1/karaoke` →
  `stems_pending`/`stems_done` and `now_playing[].queue_position` (the tiered
  position once #195 is deployed).
- Per-playlist pending list: `GET /api/v1/playlists/{id}/videos` → filter
  `stems_state == "queued"`; the lowest-id such row on the on-program playlist is
  what the #195 tier-1 selector picks next.

Run these from the box via `mcp__win-resolume__Shell`
(`Invoke-WebRequest -UseBasicParsing http://127.0.0.1:8920/...`). Reading the DB
file directly is not an option here.

## Re-measuring a separator candidate (dev2)

Never run heavy separation on win-resolume while anything plays (GPU contention
crashes the box). Copy FLACs off the box via a temporary `python -m http.server`
in the cache dir, curl them to dev2, and run in a `--system-site-packages` venv
(reuse dev2's CUDA torch) with `audio-separator` + `audioread`. Match stems by the
parenthesized token; resample to 48 kHz for any additivity metric.
