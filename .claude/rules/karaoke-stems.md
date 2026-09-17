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
- **Worker (`crate::stems::StemWorker`)** mirrors the lyrics worker: a 10 s tick
  that separates the next normalized song (`get_next_video_for_stems`,
  oldest-first, backoff-gated) under the priority regime. In `low-priority` it
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
- **Duration cap (2026-09-15) — stems only up to 15 min
  (`STEM_MAX_DURATION_MS`, `stems/worker.rs`).** A 10-minute "warm-up" file
  pinned the heavy child's private bytes at ~5.0 GB against the (then) 6 GiB
  Job Object ceiling (`heavy_slot.rs::CHILD_JOB_MEMORY_LIMIT_BYTES` — CUDA
  context + several float32 copies of the whole mix), so allocations failed
  and the child crawled at 0.2 cores / ~200k page faults/s for 20+ minutes
  before timing out. Fix was two-part: the ceiling went 6→10 GiB (clears a
  normal long-song working set with margin), AND the stem worker now skips
  separation entirely for anything over 15 min — `process_next` checks
  `stem_duration_too_long(job.duration_ms)` right after picking the job,
  before the heavy-slot/memory-guard/spawn, and marks the row terminal
  `stem_status = 'unsupported'` (no retry, no backoff). Such long files are
  not songs; karaoke stems for them are pointless regardless of ceiling size.
- **DB (V24):** `videos.{vocals_file_path, instrumental_file_path, stem_status,
  stem_attempts, stem_next_attempt_at}`. `stem_status` NULL=pending → done /
  failed (retryable) / unsupported (terminal). Paths are also derived
  deterministically by `stems::stem_paths` so the reader needs no DB round-trip.
- **API:** `GET/POST /api/v1/karaoke` → `EngineCommand::SetKaraoke` →
  `PlaybackEngine::set_karaoke` (write the live gain atomics via the control +
  persist + broadcast `KaraokeStateChanged`; NO reload on a mode change since
  #186). Dashboard: `components/karaoke_control.rs`.

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

## Re-measuring a separator candidate (dev2)

Never run heavy separation on win-resolume while anything plays (GPU contention
crashes the box). Copy FLACs off the box via a temporary `python -m http.server`
in the cache dir, curl them to dev2, and run in a `--system-site-packages` venv
(reuse dev2's CUDA torch) with `audio-separator` + `audioread`. Match stems by the
parenthesized token; resample to 48 kHz for any additivity metric.
