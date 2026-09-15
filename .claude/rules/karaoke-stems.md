---
paths:
  - "crates/sp-server/src/stems/**"
  - "crates/sp-decoder/src/audio/karaoke*.rs"
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

- **Mixing (`sp_decoder::KaraokeAudioReader`)** wraps the two stem readers behind
  ONE `AudioStream`, so `SplitSyncedDecoder` / pacer / genlock / NDI submit are
  UNTOUCHED. `out = clamp(v*vg + i*ig, -1, 1)`, gains read from `Arc<AtomicU32>`
  per chunk (slider is live mid-song). Buffers each stem (FLAC packet boundaries
  differ); timestamps from a cumulative sample counter.
- **Live control (`crate::stems::control`)** is a process-global `KaraokeControl`
  (mode = `AtomicU8`, vocal_gain = shared `Arc<AtomicU32>`), seeded from the
  `karaoke_mode` + `karaoke_vocal_gain` settings at startup. The pipeline reads it
  at song open (mode) + per chunk (gain) — same spirit as the per-output `burn_on`
  atomic, but global (one wall, one operator).
- **Reader seam (`crate::stems::reader::open_audio_stream`)** picks a plain
  `SymphoniaAudioReader` (FullMix, or a non-FullMix mode whose stems are missing —
  the safe fallback) or a `KaraokeAudioReader`. Called from both `pipeline.rs` and
  `pipeline_paced.rs` in place of the bare audio open.
- **Modes (`sp_core::playback::KaraokeMode`):** FullMix / KaraokeLow / VocalsOnly /
  InstrumentalOnly, with `stem_gains(vocal_gain) → (vg, ig)`.
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
  0xc0000005, every isolation/separation, win-resolume 2026-09-15) **+ IDLE_PRIORITY_CLASS + thread cap
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
- **DB (V24):** `videos.{vocals_file_path, instrumental_file_path, stem_status,
  stem_attempts, stem_next_attempt_at}`. `stem_status` NULL=pending → done /
  failed (retryable) / unsupported (terminal). Paths are also derived
  deterministically by `stems::stem_paths` so the reader needs no DB round-trip.
- **API:** `GET/POST /api/v1/karaoke` → `EngineCommand::SetKaraoke` →
  `PlaybackEngine::set_karaoke` (persist + broadcast `KaraokeStateChanged` + reload
  playing pipelines at position on a mode change). Dashboard:
  `components/karaoke_control.rs`.

## Re-measuring a separator candidate (dev2)

Never run heavy separation on win-resolume while anything plays (GPU contention
crashes the box). Copy FLACs off the box via a temporary `python -m http.server`
in the cache dir, curl them to dev2, and run in a `--system-site-packages` venv
(reuse dev2's CUDA torch) with `audio-separator` + `audioread`. Match stems by the
parenthesized token; resample to 48 kHz for any additivity metric.
