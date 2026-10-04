---
paths:
  - "crates/sp-server/src/diag/**"
  - "crates/sp-server/src/api/diag*.rs"
  - "crates/sp-server/src/playback/decode_thread.rs"
  - "crates/sp-decoder/src/subtype.rs"
---

# Diagnostic benches: `/api/v1/diag/*` (#223 S0)

These are measurement routes. The main session runs them on the box to decide
a design gate. Playback never calls them, and the dashboard has no button for
them.

## `POST /api/v1/diag/decode-bench`: what one picture costs the real decoder

### Samples

- They live in `<data dir>/bench/`, which is `C:\ProgramData\SongPlayer\bench\`
  on win-resolume.
- The dir sits beside `songplayer.db`, not under `cache\`, because the startup
  self-heal owns the media cache.
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

The body has two fields, both required:

- `file`: a bare file name. No `/`, `\` or `:`, no `..`, no control
  characters, at most 255 bytes. No trailing `.` or space (Windows strips
  them) and no Windows device name (`CON`, `NUL`, `COM1`, `lpt9.mp4`, …).
- `seconds`: 1 to 15.

Answers:

- **200:** the report.
- **500:** the report of a run the decoder ended, at open or mid-run. It
  carries `error` and the pictures decoded so far.
- **400:** a bad name, or `seconds` out of range.
- **422:** a body that is not the two fields (one missing, a negative or a
  fractional `seconds`): axum's JSON rejection.
- **404:** no such sample. The body names the path it looked at.
- **409:** a run is in progress. One run at a time per process.
- **501:** a non-Windows build.

Every answer without a report is logged at INFO (`decode-bench: no report`,
with the file, the status and why).

### What it measures

- The file is opened with `sp_decoder::MediaFoundationVideoReader`: the same
  open, configure and NV12 path as the paced producer.
- It runs on its own thread, `decode-bench`. That thread is started by
  `playback::decode_thread::spawn_decode_thread`, exactly like the producer's
  `paced-decode-<pid>`, so both get `THREAD_PRIORITY_NORMAL` inside
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
  start-up, so `decode_us.max` is often this one; `p99` is the steady-state
  spike.
- `wall_ms`: the decode loop only.
- `open_ms`.
- `ended`: `end_of_stream`, `time_limit` or `error`.
- `error`.
- `thread_priority`.
- `decode_us {mean, p50, p99, max}`: one sample per `next_frame` call that
  returned a picture.
- `budget {frame_period_us, mean_over_half_period}`.

**The gate is revision 2's D2.**

- `budget.mean_over_half_period` is D2's gate WITHOUT the stems child: `true`
  means the mean is over 50 % of 1/f. `false` means software decode keeps
  up.
- WITH the stems child resident, D2's gate is 75 %. Check
  `decode_us.mean <= 0.75 * budget.frame_period_us` by hand; the flag does
  not encode it.
- A failed gate means a hardware-decode slice comes before any 4K download
  (revision 3, R3-4).
- Record the result on #223.

**Logs:**

- one INFO `decode-bench: start` (file, bytes, size, codec, fps, open_ms,
  priority);
- at the end, one INFO `decode-bench: done: <summary>`, or a WARN when a
  decoder error ended the run;
- one INFO `decode-bench: no report` for every 400 / 404 / 409 / 500 / 501
  answer that carries no report.

## Code map

- `diag/decode_bench.rs`: the cross-platform core.
  - It holds the gate, the name check, `measure` (generic over
    `VideoStream`), `DecodeUs`, `Budget`, `BenchReport` and
    `run_on_decode_thread`. That last one owns the thread: the slot is
    dropped BEFORE the report is sent, so the bench is free when the route
    answers, and a panicking body comes back as a 500 with the bench free.
  - Linux tests drive it with a scripted stream and a scripted clock, with no
    wall-time thresholds.
- `diag/decode_bench_mf.rs`: the `cfg(windows)` thread body (open, facts,
  log).
  - The Windows job's router test covers it on the decoder crate's
    `tests/fixtures/black_3s.mp4`. It expects `codec` H264, `ended`
    `end_of_stream` and `thread_priority` 0.
- `api/diag.rs`: the handler.
  - `AppState.decode_bench` holds the dir and the gate.
  - Every test state builds its own, so the 409 test cannot race another
    test.
- `playback/decode_thread.rs`: the ONE place a decode thread is spawned,
  for both the producer and the bench. A change to the decode thread's
  priority goes THERE, so the bench follows it.
- `sp_decoder::subtype` and `MediaFoundationVideoReader::codec()`: the codec
  text.
