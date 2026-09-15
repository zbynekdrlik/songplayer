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
- **Worker (`crate::stems::StemWorker`)** mirrors the lyrics worker: a 10 s tick
  that, ONLY while the wall is idle (reuses the #154 gate via
  `idle_gate::wall_activity_from` + `GateLog::defer_settled`), separates the next
  normalized song (`get_next_video_for_stems`, oldest-first, backoff-gated). Runs
  `stem_worker.py` at BELOW_NORMAL WDDM priority + the `LYRICS_GPU_MEM_FRACTION`
  VRAM cap + CUDA-OOM→CPU fallback (`gpu_policy`). Kill switch: `stem_worker_enabled`
  (default ON, idle-gated anyway).
- **Idle-settle hysteresis (2026-09-14 incident, `idle_gate.rs`):** a SINGLE idle
  sample was the bug — OBS scene re-evaluation (E2E `afterAll`), an operator
  scene switch or a song change flips every pipeline off `Playing` for a few
  seconds, and both workers resumed a multi-minute GPU job during that gap. Heavy
  work now resumes only after the wall reads idle continuously for
  `WALL_IDLE_SETTLE = 30 s`. Both workers share the clock through
  `GateLog::defer_settled` (the `GateLog` each already holds in a `Mutex`), so no
  worker constructor changed; the pure `should_defer` stays for its own tests.
- **Mid-job abort watcher (#161, `idle_gate_abort.rs`):** the settle gate only
  decides BEFORE a heavy step; once separation is running (2–5 min GPU child)
  it does nothing. `run_with_wall_abort` races the separation future against a
  1 s wall poll and kills the child (`kill_on_drop`) after 2 consecutive busy
  samples (~2 s debounce — NOT the 30 s settle, which guards resume, not run).
  A wall abort re-queues with NO penalty (`StemStepResult::WallAborted`): the
  partial stems are deleted and the DB row is left pending (`stem_status` NULL,
  `stem_attempts` unchanged), so `get_next_video_for_stems` re-picks it the
  moment the wall idles. A genuine separation failure still records the backoff
  deferral. The lyrics worker wraps its isolation + mtl steps the same way.
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
