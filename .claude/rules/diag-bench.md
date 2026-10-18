---
paths:
  - "crates/sp-server/src/diag/**"
  - "crates/sp-server/src/api/diag*.rs"
  - "crates/sp-server/src/playback/decode_thread.rs"
  - "crates/sp-decoder/src/subtype.rs"
---

# Diagnostic benches: `/api/v1/diag/*` (#223 S0, S3b; #279 P0, P3a)

These are measurement routes. The main session runs them on the box to decide
a design gate. Playback never calls them, and the dashboard has no button for
them.

## `POST /api/v1/diag/decode-bench`: what one picture costs the real decoder

### Samples

- They live in `<data dir>/bench/`, which is `C:\ProgramData\SongPlayer\bench\`
  on win-resolume.
- The dir sits beside `songplayer.db` (`ServerConfig::data_dir`), not under
  `cache\`, because the startup self-heal owns the media cache.
- The main session puts the samples there itself (for S0, a 4K AV1 file and a
  4K VP9 file) and creates the dir if it is missing.

### Running it

Port 8920 (`sp_core::config::DEFAULT_API_PORT`). From a Linux box on the
LAN:

```bash
curl -s -X POST http://10.77.9.201:8920/api/v1/diag/decode-bench \
  -H 'content-type: application/json' -d '{"file":"av1_2160p.mp4","seconds":15}'
```

On the box, in PowerShell, keep the `catch`. `Invoke-RestMethod` throws on
a 4xx/5xx, and a 500 still carries the report (`error` and the pictures
decoded so far):

```powershell
$body = '{"file":"av1_2160p.mp4","seconds":15}'
try {
  Invoke-RestMethod -Method Post http://127.0.0.1:8920/api/v1/diag/decode-bench `
    -ContentType 'application/json' -Body $body | ConvertTo-Json -Depth 4
} catch { $_.ErrorDetails.Message }
```

A decode run's body has two required fields and one optional (#279: the
handler, not axum, requires `file` and `seconds` unless `"probe": true`,
below):

- `file`: a bare file name. No `/`, `\` or `:`, no `..`, no control
  characters, at most 255 bytes. No trailing `.` or space (Windows strips
  them) and no Windows device name (`CON`, `NUL`, `COM1`, `lpt9.mp4`, …).
- `seconds`: 1 to 15.
- `hw` (#223 S3b, optional, default `false`): `true` opens the reader in
  `Hardware` mode, Media Foundation's decoder on the GPU
  (`.claude/rules/video-decode.md`). Run the same sample with and without
  it to measure hardware against software:

  ```bash
  curl -s -X POST http://10.77.9.201:8920/api/v1/diag/decode-bench \
    -H 'content-type: application/json' \
    -d '{"file":"av1_2160p.mp4","seconds":15,"hw":true}'
  ```
- `resident` (#279 P3a, optional, default `false`, never with `hw`): `true`
  makes a producer device of the run's own on the production adapter
  (`sp_gpu::ProducerGpu::new`, like a pipeline's) and opens the reader
  GPU-resident on it (`video-decode.md` "GPU-resident decode"): every
  GPU-decoded picture is copied into the producer's ring and never read
  back, so the timings are what a GPU-resident song pays (the decode and
  the ring copy). The box gate "decode bench gpu_resident": each 4K
  sample (AV1, VP9) within D2.

Answers:

- **200:** the report.
- **500:** the report of a run the decoder ended, at open or mid-run. It
  carries `error` and the pictures decoded so far.
- **400:** a bad name, `seconds` out of range, (#279) a missing `file`
  / `seconds`, or (#279 P3a) `hw` and `resident` both.
- From axum, before the handler runs (NOT logged): **400** for a body that is
  not JSON, **422** for a field of the wrong type (a negative or a
  fractional `seconds`, a `hw`, `resident` or `probe` that is not a
  bool), **415**
  without `Content-Type: application/json`. A missing `file` or `seconds`
  is the handler's 400 (#279).
- **404:** no such sample. The body names the path it looked at.
- **409:** a run is in progress. One run at a time per process. A 409 that
  never clears means the decoder hangs inside `open` or `next_frame`: the
  run's thread holds the bench until SongPlayer restarts.
- **501:** a non-Windows build.

Every refusal (400, 404, 409, 501) is logged at INFO (`decode-bench: no
report`, with the file, the status and why). A run that ends with no report
(its thread did not start or panicked, 500) is a WARN (`decode-bench: the run
failed`).

### What it measures

- The file is opened with `sp_decoder::MediaFoundationVideoReader`: the same
  open, configure and NV12 path as the paced producer, in the request's
  mode (`hw`). The paced producer's mode is the setting `video_hw_decode`,
  so a bench with `hw` set to that setting measures what playback runs.
- #282: it opens under the decode cap in force (`max_resolution`, the
  start line's `max_height`), as the producer does: a sample larger than
  the cap's 16:9 box is decoded scaled, and `width` / `height` report the
  scaled size. The cap is computed with the SETTING's `video_hw_decode`,
  not the request's `hw` (on a box with the setting off, automatic is
  1440 even for a `"hw": true` run). Set `max_resolution` back to
  automatic to measure a 4K sample at 4K (`video-decode.md` "The decode
  cap"). The report carries `max_height` (the cap the run opened under)
  and `scaled_from` (`[w, h]`, the stream's own size, when the run decoded
  it scaled; `null` otherwise); a scaled run's end line adds
  ` scaled_from=WxH max_height=N`. A scaled run's `decode_path` reads
  `"hardware"` for any DXGI surface (the video processor's texture tells
  nothing of the decoder): judge NVDEC by the timings, and compare
  `frames` / `source_fps` with an uncapped run of the same file (a
  difference = the processor converted the rate).
- It runs on its own thread, `decode-bench`. That thread is started by
  `playback::decode_thread::spawn_decode_thread`, exactly like the producer's
  `paced-decode-<playlist id>`, so both get `THREAD_PRIORITY_NORMAL` inside
  SongPlayer's `HIGH_PRIORITY_CLASS`. The report's `thread_priority` shows it
  (0 = NORMAL).
- The run is unpaced: it decodes as fast as it can, for `seconds` of wall
  time or to the end of the stream. Video only.
- Each picture's buffer goes back to the frame pool at once, as playback's
  last owner returns it.
- It competes with live playback for CPU, as a real decoder does. Measure in
  the state you mean to judge.

### The report

- `file`.
- `width`, `height` and `stride`: from the first picture. MF may pad the
  height to a multiple of 16.
- `codec`: the native subtype's FourCC (`AV01`, `VP90`, `H264`, `HEVC`). Check
  that it is the codec the sample is named after.
- `source_fps`: `null` when MF reports no rate. The reader then plays at a
  29.97 fps guess, and there is no `budget`.
- `frames`.
- `first_us`: the first picture's call. It carries Media Foundation's
  start-up, so `decode_us.max` is often this one. From 100 pictures on,
  `decode_us.p99` is the steady-state spike; below 100 it is the max.
- `steady_mean_us`: the mean of every picture after the first, so without
  that start-up. `null` below two pictures.
- `wall_ms`: the decode loop only.
- `open_ms`.
- `ended`: `end_of_stream`, `time_limit` or `error`.
- `error`.
- `thread_priority`.
- `decode_us {mean, p50, p99, max}`: one sample per `next_frame` call that
  returned a picture. On the hardware path it includes the GPU → CPU copy
  of each picture (`Lock2DSize` read lock) and its packing: what playback
  pays.
- `budget {frame_period_us, mean_over_half_period}`.
- #223 S3b, the decode path:
  - `hw_requested`: the request's `hw`.
  - `decode_path`: the path of the LAST picture: `"hardware"` (it came
    out of the GPU decoder: a DXGI surface in a `D3D11_BIND_DECODER`
    texture), `"software"`, or `null` (no picture). It is read from the
    pictures, not from the mode: a `hw` run that reads `"software"` either
    fell back at open (`hw_fallback` says why: no hardware adapter, no
    device) or Media Foundation's decoder found no decoder on the GPU for
    the stream (no `hw_fallback`; the reader's WARN `the D3D11 path is set
    up, but Media Foundation decodes this file in software` names it).
  - `path_changes`: how often a picture's path differed from the one
    before it during the run (a mid-stream fall back, `hw_fallback`
    `mid-stream: …`, or Media Foundation's decoder changing its mind).
    The timings are ONE path's only when it is 0; judge the gate only on
    such a run.
  - `adapter`: the GPU's name, only when `decode_path` is `"hardware"`
    (the box: `NVIDIA GeForce RTX 3070 Ti`).
  - `hw_fallback`: `open: <why>` / `mid-stream: <why>`, else `null`.
- #279 P3a, a `resident` run (`hw_requested` is true for it too):
  - `gpu_resident`: `true`; its `decode_path` `"hardware"` = the pictures
    were copied into the ring (a `"software"` run fell back, as for `hw`);
  - `ring` (`null` for any other run): the producer's ring after the run,
    `{slots, in_use, waits, full, dead, copy_us_p99, vram_mb}`. Every
    picture goes back at once, so `in_use` reads 0; `waits` > 0 = the
    copy found every slot in use;
  - a producer device that cannot be made (no production adapter, as on
    `windows-latest`) is a 500 with the report, `error` `producer device:
    <why>`, no ring;
  - the end line adds ` gpu_resident ring_slots=… ring_waits=… ring_full=…
    ring_dead=… copy_us_p99=… vram_mb=…` (` gpu_resident ring=-` with no
    producer).

**The gate is revision 2's D2.**

- `budget.mean_over_half_period` is D2's gate WITHOUT the stems child: `true`
  means the mean is over 50 % of 1/f. `false` means software decode keeps
  up.
- WITH the stems child resident, D2's gate is 75 %. Check
  `decode_us.mean <= 0.75 * budget.frame_period_us` by hand; the flag does
  not encode it.
- The flag judges the mean of EVERY picture, start-up included, as D2 states
  it. Playback's pre-roll absorbs the start-up, so re-judge a borderline
  verdict on `steady_mean_us` (for example a 500 ms first call over 450
  pictures adds about 1.1 ms to the mean).
- A failed gate means a hardware-decode slice comes before any 4K download
  (revision 3, R3-4). S0 failed it (95 % AV1, 93 % VP9 at 4K, comment
  5981771378), so S3b added `hw`. Its gate: a `hw` run with `decode_path`
  `"hardware"` and `path_changes` 0 on each 4K sample passes D2 (mean ≤
  50 % of 1/f), and the
  1440p baseline's `hw` mean is not above its software mean. Only then may
  `video_hw_decode` default to on.
- The `"hardware"` path is INFERRED from the texture's
  `D3D11_BIND_DECODER` (Microsoft's DXVA decoders write into such
  textures; CI never sees a real decoder texture). Cross-check on the box:
  a `hw` run that reads `"software"` while its mean is far below the
  software run's means the inference missed (a decoder post-processing
  into another texture, e.g. AV1 film grain, or a vendor's own decoder
  MFT): record it on #223 rather than trusting the label.
- Record the result on #223.

**Logs:**

- one INFO `decode-bench: start` (file, bytes, `max_wall_s`, priority,
  `mode` = `software` / `hardware` / `gpu_resident`, `max_height`), logged
  BEFORE the open, so a run that wedges inside it still names its file;
- at the end, one INFO `decode-bench: done: <summary>` (size, codec, fps,
  `open_ms`, the stats, the gate; a `hw` run adds `hw=requested
  decode_path=… adapter=… hw_fallback=…`, a software run's line is S0's),
  or a WARN when a decoder error ended the run (a file that does not open:
  WARN `decode-bench: the file did not open`);
- one INFO `decode-bench: no report` for every 400 / 404 / 409 / 501
  refusal, and one WARN `decode-bench: the run failed` for a 500 without a
  report.

### `{"probe": true}`: the GPU capability probe (#279 P0)

- The same route, with `{"probe": true}` and NO other field
  (`diag/gpu_probe.rs::ask`: a probe with `file`, `seconds`, `hw` or
  `resident` is a 400; a request that is no probe and lacks `file` or `seconds` is a 400
  too: both are optional in the body since #279, so no longer axum's 422;
  a `probe` that is not a bool is axum's 422).
- It runs `sp_gpu::probe_gpu()` (`gpu-max.md` "The capability probe") on a
  blocking thread holding the bench's slot (409 while any bench runs): two
  devices on the compositor's adapter (A with the video API like the decode
  device, B BGRA like the compositor), five steps, the keyed-mutex step
  reading A's bytes back on B; read-only, nothing of the program path
  touched.
- 200 `{verdict {kind: gpu_resident_ok | needs_fallback, step, error},
  probe {<step>: {ok, error?}}, adapter, adapters [{name, vendor_id,
  device_id, vram_mb, software}], vram {budget_mb, used_mb} | null,
  vram_error}`; an error is `{call, hresult: "0x…", detail}`. 500 when no
  adapter or device could be had (the text names it); 501 off Windows.
  INFO `decode-bench: GPU probe start` / `done: <verdict, adapter, steps>`,
  INFO `decode-bench: no probe` for a 409 / 501, WARN `the GPU probe failed`.
- Record the SNV and PP answers on #279 (P0's gate).

```bash
curl -s -X POST http://10.77.9.201:8920/api/v1/diag/decode-bench \
  -H 'content-type: application/json' -d '{"probe":true}'
```

## `POST /api/v1/diag/fit-bench`: what the program canvas fit costs (#223 S10a)

- `{"width", "height", "frames"}`: an even size from 2×2 to 3840×2160, 1 to
  600 frames. It fits a synthetic NV12 picture of that size into SP-program's
  1920×1080 canvas `frames` times (`program_canvas::Canvas::fit`), then fades
  it into itself `frames` times (`Canvas::fade`, both sides fitted), on a
  `BandPool` of the sender's band count (`mix_bands` of the logical
  processors) built for the run, on a blocking thread at normal priority
  while SongPlayer plays: an upper bound of the sender's own cost.
- Answers 200 `{width, height, canvas_width, canvas_height, bands, frames,
  fit_us, fade_us {mean,p50,p99,max}, budget_us: 16666, over_budget}`;
  400 out of range (axum's 400/415/422 before the handler as for the decode
  bench); 409 while ANY bench runs (it takes the decode bench's slot: one
  bench at a time); 500 when its thread failed. INFO `fit-bench: start` /
  `done`, INFO `fit-bench: no report` for a refusal.
- G3 (#223 S10): `over_budget` false at 3840×2160 = a p99 of both the fit and
  the fade within half a 30 fps slot. Record the result on #223.
- Passed on SNV 11.10.2026 (comment 6102650616): 4K fit p99 3.2 ms, fade
  p99 6.5 ms; 1440p 3.1 / 5.8 ms. The fused kernel's work follows the
  1920×1080 canvas it writes, so a 4K source costs about what a 1440p one
  does. PP (a laptop CPU) is measured once a release brings it the bench.

```bash
curl -s -X POST http://10.77.9.201:8920/api/v1/diag/fit-bench \
  -H 'content-type: application/json' -d '{"width":3840,"height":2160,"frames":300}'
```

## Code map

- `diag/decode_bench.rs`: the cross-platform core.
  - It holds the gate, the name check, `measure` (generic over
    `VideoStream`), `DecodeUs`, `Budget`, `BenchReport` and
    `run_on_decode_thread`. That last one owns the thread: the slot is
    dropped BEFORE the report is sent, so the bench is free when the route
    answers, and a panicking body comes back as a 500 with the bench free.
  - Linux tests drive it with a scripted stream and a scripted clock, with no
    wall-time thresholds.
- `diag/decode_bench_mf.rs`: the `cfg(windows)` thread body (open in the
  mode, facts, the reader's `DecodeFacts` after the run, log; #279 P3a: a
  resident run's producer made first through `decode_bench::decode_mode`,
  Linux-tested with `make` a parameter, and its ring read after the
  reader is dropped). The request's mode is `decode_bench::BenchMode`
  (`Software` / `Hardware` / `Resident`); tests of a resident run:
  `decode_bench_tests_resident.rs`.
  - The Windows job's router test covers it on the decoder crate's
    `tests/fixtures/black_3s.mp4`. It expects `codec` H264, `ended`
    `end_of_stream` and `thread_priority` 0; a software run reads
    `decode_path` `"software"`, a `hw` run `"hardware"` or `"software"`
    (no GPU on `windows-latest`: an open fall back), with `adapter` only on
    hardware.
  - `BenchReport::with_decode` (Linux-tested) applies the decode facts;
    the adapter rule (named only on a hardware path) lives there.
- `diag/fit_bench.rs`: the fit bench (`check`, `synthetic`, `report` tested;
  `run` the timed loops); route tests `api/diag_tests_fit.rs`.
- `diag/gpu_probe.rs` (#279 P0): `ask` (the request's two shapes),
  `ProbeAnswer` (+ its log `summary`) and `run` (the probe a parameter, the
  slot dropped before the answer, a panic a 500), Linux-tested in
  `gpu_probe_tests.rs`; route tests `api/diag_tests_probe.rs` (501 off
  Windows; on the Windows job the real probe answers or names why not).
- `api/diag.rs`: the handlers.
  - `AppState.decode_bench` holds the dir and the gate.
  - Every test state builds its own, so the 409 test cannot race another
    test.
- `playback/decode_thread.rs`: the ONE place a decode thread is spawned,
  for both the producer and the bench. A change to the decode thread's
  priority goes THERE, so the bench follows it.
- `sp_decoder::subtype` and `MediaFoundationVideoReader::codec()`: the codec
  text.
