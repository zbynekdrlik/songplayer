# Program audio outputs: a list of outputs, ASIO with a drift-compensating resampler, one network sample rate

Issue: #233. Owner decisions on #233: 6033483765 (ASIO is the core path, VBAN the fallback, several outputs of each type), 6034292526 (option 1: one output list), 6034307216 (design section 1), 6034341232 (design section 2). Research with file:line and URL citations: #233 comment 6033159389, and dev1 `~/.claude/work-products/songplayer/233-asio-research/`.

## Goal

SongPlayer's program audio reaches every destination in the transport that destination supports, at the destination's own sample rate and format, all at the same time:

- ASIO devices, the core path: Dante Virtual Soundcard (DVS) at SNV and PP, or a Waves SoundGrid ASIO virtual card. Each device's clock is the audio network's clock, not SongPlayer's, so a drift-compensating resampler keeps the output glitch-free.
- VBAN destinations, the fallback, each at its own rate.
- Later transports (AES67, Dante API), added as new output types.

The whole audio network works at one sample rate by default (SNV Dante: 96 kHz). SongPlayer adapts to the network, never the other way round.

## Today (7.10.2026)

- The program bus carries 48 kHz stereo f32, one 1600-frame block per 1/30 s boundary (`program_output.rs:104-113`). Media is made 48 kHz offline (`normalize.rs`, `-ar 48000`), and `split_sync.rs:95` refuses any other rate.
- Every block takes ONE path: `ProgramOutput::serve` → peak limiter → VBAN hand-off → MAX offer → NDI submit (`program_output.rs:353-445`).
- VBAN: one stream name and one format (48 kHz INT24, rate index 3 hard-coded at `vban_packet.rs:38`) for up to 8 targets (`vban_targets`, `VBAN_MAX_TARGETS = 8`). The paced 1/240 s `vban-output` thread is an MMCSS Pro Audio thread. SNV sends to `fohabl.lan:6980,lv1.lan:6980`, stream `sp-program`.
- On fohabl, VB-Matrix converts the 48 kHz VBAN to its 96 kHz master. Its manual warns of "more or less hearable artifact", and its method is undocumented.
- There is no ASIO code and no resampler crate in `Cargo.lock`.
- Devices: SNV win-resolume has DVS 4.5.2.3 (repaired 7.10, camera-box issue 1381; its settings need the owner, see Open points), VB-Matrix VASIO and Blackmagic ASIO. PP has DVS 4.5.2.3 and Blackmagic ASIO. dantesync runs at both. SNV follows the Dante PTP leader; PP is on NTP fallback.

## Design

### 1. The output list

One setting, `audio_outputs`: a JSON list. Each entry has these fields:

| Field | Meaning |
|---|---|
| `id` | stable string id (generated on create) |
| `name` | operator label |
| `type` | `vban` \| `asio` (later types extend the enum) |
| `enabled` | bool |
| `rate` | `"network"` (default) or a fixed rate in Hz (44100, 48000, 88200, 96000, 192000) |
| `delay_ms` | extra delay, 0..=2000, default 0, for lip-sync alignment of this destination |
| `vban` | `{host, port, stream_name, format: int16 \| int24 \| float32}` (type `vban`) |
| `asio` | `{driver: "<registered ASIO driver name>", channels: [left_index, right_index]}` (type `asio`) |

A second setting, `audio_network_rate`, holds the network's sample rate: 48000 by default, 96000 at SNV. An output with `rate: "network"` uses it. An ASIO output ignores `rate`, because the driver's current rate is the truth (section 3); a mismatch with `audio_network_rate` is a WARN and a status note.

The settings API validates the list typed, never through `serde_json::Value`'s own Deserialize. A bad list refuses the PATCH with 400 and names the entry and the field. The list is re-read live by the outputs' settings task (as `vban_*` is today, every 5 s); an entry that changed is rebuilt, and the others keep running.

**Migration (first start of the new version):** today's `vban_enabled`, `vban_stream_name` and `vban_targets` become one `vban` entry per target, with the same stream name, `format: int24`, `rate: 48000` (fixed, NOT "network") and `enabled` as today. The old keys are then deleted. FOH therefore hears exactly what it hears today. Moving fohabl to 96 kHz is a separate, owner-agreed step (Open points).

### 2. The fan-out

After the peak limiter, where the VBAN hand-off is today, every program block is offered to each enabled output's own queue. Every output has its own thread. A slow, blocked or failed output can never delay the program boundary, the NDI submit, MAX or another output. Queues drop the oldest block on overflow and count it, like VBAN today.

The fan-out lives in its own module, not in `program_bus.rs` (971/1000 lines).

### 3. Per-output conversion

Each output converts the 48 kHz stereo f32 program to its target rate, channel layout and sample format.

- **VBAN.** It stays paced from the genlock wall clock: SongPlayer is the clock master, so there is no drift servo.
  - The rate conversion is a fixed ratio (48→96 kHz is exactly 2×), run with `rubato`.
  - Packets carry the destination's rate index (48 kHz = 3, 96 kHz = 4) and format.
  - At 96 kHz with 200-frame packets the spacing is 1/480 s: 4.9 Mbit/s for 24-bit stereo per destination, against 2.45 Mbit/s at 48 kHz.
  - Packet size stays within VBAN's limits (≤ 256 frames per packet, payload ≤ 1436 bytes).
- **ASIO.** The driver is opened with `azo` (pure-Rust ASIO, MIT; the iemmixer precedent), with no Steinberg SDK, so there is no GPLv3/proprietary licence question for SongPlayer (MIT).
  - On open, read the driver's CURRENT sample rate, preferred buffer size, output channel count and sample type. Never set the rate: Dante Controller or SoundGrid owns it.
  - Never change the buffer size while the driver is open; iemmixer saw a reset and a fault doing that.
  - The program's L/R go to the two configured output channels, and every other channel is filled with silence.
  - The sample type is converted from f32 to the driver's type (Int32LSB, Float32LSB, Int24LSB, Int16LSB; an unknown type refuses the open with a clear reason).
  - The driver's buffer-switch callback copies only, from a lock-free single-producer/single-consumer ring (`rtrb`, as in iemmixer): no allocation, no lock, no logging in the callback. An empty ring writes silence and counts an underrun.
  - A per-device worker thread runs the resampler (below) and fills the ring.

### 4. The drift-compensating resampler (ASIO)

The device's clock is the audio network's. SongPlayer's program runs on the genlock wall clock (dantesync). The ASIO worker keeps the ring at its target fill while converting 48 kHz to the device rate in ONE `rubato` asynchronous sinc stage (`Async::new_sinc`, ~256 taps, ~2.7 ms latency), with the ratio adjusted by `set_resample_ratio_relative`.

The servo follows camera-box's `asrc-compensator` (`libobs/media-io/asrc-compensator.c`; Rust model `camera-box/src/asrc_bench.rs`, MIT):

- **Rate estimate:** a least-squares slope of the ring position against time over a long window (minutes; first applied after 60 s). This is the card's true ppm offset.
- **Fill hold:** a PI loop on the ring fill against its target (2–3 driver buffers plus a few ms margin), bounded at ±50 ppm proportional and ±3 ppm integral.
- **Limits:** the total correction is ±300 ppm, and it changes by at most 5 ppm/s, which is inaudible.
- **Clock steps:** a dantesync step (forward or backward) or a buffer jump beyond a threshold re-centres the ring once: drop or insert samples under a short crossfade. The rate estimate is not disturbed; no step ever reaches the audio as a click.

All servo decisions are a pure module, Linux-tested and mutation-gated. Only the `azo` glue is Windows-only.

### 5. Latency and lip-sync

Each output reports its real latency: the ring fill plus the driver's output latency (`ASIOGetLatencies`) plus the resampler group delay, or the VBAN send budget. `delay_ms` adds a fixed delay per output, so every destination can be aligned with the picture on the wall.

### 6. Failures

- **When an ASIO output closes:** on a driver reset request (`kAsioResetRequest`), a vanished device (a DVS reinstall, a crash), or "device busy" (DVS takes ONE ASIO client).
- **The reopen backoff:** 2 s, 10 s, 30 s, then every 60 s.
- **Status:** the reason shows as the output's status.
- **Never affected:** the program, VBAN and the other outputs.
- **A start backoff after release:** some virtual drivers keep the device busy for a few seconds after release.

### 7. Telemetry, API, UI

- **API:** `GET /api/v1/program` gains `outputs[]`. Each entry carries id, type, name, state (running / opening / waiting with a reason / disabled), rate, format, channels, latency_ms, and blocks sent/dropped. ASIO entries add ppm, ring fill_ms, underruns and resets. VBAN entries add packets, send errors and late events, as today's `vban` telemetry.
- **UI:** Nastavenia gains the "Zvukové výstupy" section: the list, add, remove, enable, per-type fields, the network rate, and each output's live state. The ASIO driver is a dropdown of the drivers the box has registered (`GET /api/v1/audio/asio-drivers`, Windows; empty elsewhere).
- **Tests:** Playwright on the mock with zero console errors.

### 8. Testing

- **Pure Linux tests with the mutation gate:**
  - the output list parse and validation;
  - the migration from `vban_*`;
  - the rate plan (network vs fixed vs the driver's);
  - VBAN packet headers for each rate and format;
  - the f32 → driver sample-type conversion;
  - the channel map;
  - the servo, as a simulation: a card at −50 / 0 / +50 ppm, clock steps of ±1 ms and ±44 ms, a dropped buffer. Assert no underrun after settling, |correction| ≤ 300 ppm, slew ≤ 5 ppm/s, and the fill held within bounds.
- **The ASIO sink through a fake driver trait,** because CI runners have no ASIO driver: open at a reported rate, a reset request, a vanished device, a busy device, and the backoff schedule.
- **Live post-deploy gates (box E2E):**
  - **VBAN:** every migrated destination still sends 48 kHz `sp-program` (the FOH path is unchanged).
  - **ASIO at SNV and at PP, once DVS is configured:** the output is running at the driver's rate, 0 underruns over 60 s, |ppm| within bounds, latency reported.
  - **A VBAN destination at 96 kHz:** a receiver on the box reads rate index 4 and continuous packets.

### 9. Lanes (serial)

1. The output list + migration + fan-out + VBAN per destination (rate, format, stream name), with the UI section for VBAN entries.
2. The resampler and drift servo as a pure module (`rubato`), with the simulation tests.
3. The ASIO output (`azo`), the driver list route, the ASIO UI fields, telemetry and the live gates at SNV and PP.

## Out of scope

- Running the program itself at 96 kHz. Media is 48 kHz; converting once at each output is cheaper and keeps every input path unchanged.
- ASIO input (capture); the NDI input "OBS manuál" stays as is.
- Changing fohabl, VB-Matrix or Ableton. Their configuration is the owner's, and fohabl is critical production.

## Open points (for the owner, not blocking lanes 1–2)

- DVS at SNV lost its settings in the 7.10 repair. The owner re-sets interface, mode, channels and latency in the DVS app (camera-box issue 1381). The ASIO live gate at SNV waits for it.
- `audio_network_rate` at SNV = 96000 is set when lane 1 deploys. Migrated VBAN entries keep 48 kHz until the owner agrees to move fohabl's VBAN input to 96 kHz (then VB-Matrix no longer converts).
- Which DVS output channels SongPlayer uses at SNV and PP (default: 1/2), and which Dante receivers subscribe to them, is the owner's routing in Dante Controller.
