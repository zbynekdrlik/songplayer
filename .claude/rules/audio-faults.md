---
paths:
  - "crates/sp-server/src/audio_faults/**"
  - "crates/sp-server/src/api/audio_faults*.rs"
  - "crates/sp-server/src/db/mod_tests_v38.rs"
  - "crates/sp-server/src/playback/program_bus_window*.rs"
  - "crates/sp-decoder/src/peak_limiter*.rs"
  - "e2e/audio-faults-gate*.ts"
  - "e2e/post-deploy-audio-*.spec.ts"
  - "crates/sp-core/src/audio_faults*.rs"
  - "sp-ui/src/components/audio_faults.rs"
  - "sp-ui/src/components/health_bar.rs"
  - "e2e/health-audio-faults.spec.ts"
  - "e2e/mock-api.mjs"
---

# The audio fault ledger (#257)

The owner (12.10.2026): "akékoľvek puknutie, strata samplu je
neakceptovateľná" — every audio path, not only SoundGrid. Design: the
"## Design (before code)" comment on #257 (one persisted ledger fed by the
deltas of the counters that already exist; the rejected alternative was a
history per stage). Lane 1 = the ledger, the API, the gate; lane 2 = the UI
(HealthBar segment + per-path rows); lane 3 = more loops (Dante DVS tx→rx,
VBAN receive side), box work.

## The table (V38) and the writer

- `audio_faults(ts_ms, path, kind, severity, amount, detail)`, indexes on
  `(ts_ms)` and `(path, ts_ms)`. `severity` = `loss` | `risk` | `info` (a
  path's start) | `meter` (a loop check's checked / blind ms). Kept 90 days
  (`RETENTION_MS`), pruned at the first tick and then once a day.
- ONE task (`audio_faults::run`, started in `start_program`, every
  `TICK` = 5 s, a tick at once) reads every path IN PROCESS
  (`probe::read`: `ProgramBus::status()`, `outputs().status()`,
  `outputs().limiter_overs()`, the trace, `on_air_now()`, the NDI health
  registry) — never HTTP, nothing on a real-time thread — and writes the
  ledger's rows in one transaction. Rows the store refuses are kept for the
  next tick (the newest `MAX_UNWRITTEN`), with a WARN.
- The decisions are pure (`ledger.rs`, `Ledger::tick`), pinned exactly in
  `ledger_tests.rs`:
  - a path seen for the first time (in this process, or again after it was
    gone) or REBUILT (its run counter, or any counter or meter, went DOWN)
    writes a `start` row, and its counters count from 0: every counter
    counts since its path was built, so faults between the build and the
    first tick are recorded too;
  - a counter that grew writes one row, the step as `amount`;
  - an event with its own UTC (a loop event, a VBAN late packet) is written
    once, AT ITS OWN TIME, deduplicated by `(path, kind, utc_ms)`
    (`SEEN_KEPT` = 4096 keys, oldest dropped);
  - a state (`host_down`, `no_return`) writes one row when it starts to hold;
  - meters are summed and written at most once a minute per path
    (`METER_FLUSH_MS`), a rebuilt path's sum first;
  - a probe that is not gated in (a pipeline OFF program, the authority's
    on-air set) writes no fault row unless it was gated in at the tick
    before: what grew meanwhile is taken, so it never counts later.
- One INFO per tick that recorded a loss, a risk or a start
  (`audio faults: rows recorded`, `losses`, `risks`, `starts`).

## Paths, kinds, severities (`probe.rs`)

| path | loss | risk | other |
|---|---|---|---|
| `program` (`SP-program`) | `filled` (= `health.filled_owned`), `resync` | `late_dropped`, `coalesced`, `vban_feed_late` (`health.timing.vban_feed_late_over_budget`), `limited` (`outputs().limiter_overs()`), `ndi_late` (trace records taken over a slot late) | run = `health.submitted` |
| `out:<id>` (enabled outputs) | `blocks_dropped`; VBAN `blocks_substituted`, `send_error`; ASIO `underrun`, `hard_recentre`, `reset`, `resync`, `overflow` (frames), `host_down` (state: `asio.reason_code == "host_down"`) | VBAN `late_packet` (each `late_events` entry over 10 ms, amount µs); ASIO `late_callback` | run = `blocks_sent` |
| `loop:<id>` (an ASIO output whose loop check ran: not `off`, or it has time) | each recent `mismatch` / `dropout` / `slip` event (amount = frames, a slip signed) | `no_return` (state: the check cannot tell) | meters `checked`, `blind`; run = checked + blind ms |
| `pipeline:<playlist id>` | `underrun` (`audio.underruns`), `resync` (`pacing.resyncs`), `unserviced` (`pacing.song_change_unserviced_slots`) — only while on program | — | run = `pacing.seq` |

Not faults, by design: `pacing.repeats` (a 24/25 fps song's conversion, a
paused standby), `pacing.dropped` (60→30 decimation), `late_frames`, an
ASIO `no_clock` calm wait and its `clock_waits`, a Blank / nothing-selected
fill.

### Owned fills (`program_bus_window.rs`)

`ProgramHealth.filled_owned`: a fill of a REAL source's boundary
(`real_source`: a playlist or the NDI input -1; never Blank -2 or none) —
`count_fill` (the sender's wall fill and a held window boundary) — and a
fade's missing real side (`commit_mix`, at its weight). `filled` keeps its
meaning (every fill). `program_bus.rs` is at the 1000-line cap: the counting
lives in the window child. Tests: `program_bus_window_tests.rs`.

### Limiter overs (`sp_decoder::PeakLimiter::over_frames`)

Frames whose OWN peak was over the ceiling (`needed > 0`), never the release
tail. `ProgramOutput::limit` notes the overs of every boundary that is NOT a
mix (`Pair::Mix`: a fade's sum is the crossfade's own) on
`AudioOutputs::note_limiter_overs`. Tests: `peak_limiter_tests.rs`,
`program_output_tests_limit.rs`.

## The API (`api/audio_faults.rs`, `answer.rs`)

`GET /api/v1/audio/faults?hours=N` (1..=2160, default 24; anything else —
`0`, `2161`, `+5`, `1.5`, empty — is a 400 with the fixed text
`HOURS_REFUSAL`, never the value sent; a store error a 500 naming why) →
`{now_ms, from_ms, hours, worst, truncated, paths[{path, detail, live,
clean_since_ms, last_loss_ms, last_risk_ms, counts{kind: n}, events[{ts_ms,
kind, severity, amount}], loop?{checked_s, blind_s}}]}`:

- the paths: every path that runs now (`probe::live_paths`, its own name)
  and every path with a row in the window (its newest row's `detail`);
- `clean_since_ms` = the path's last `loss` row in the window, else its
  latest `start` row in it, else the window's start (#257 acceptance: after
  a deploy it still reads the last REAL fault); `last_loss_ms` is what the
  gate reads;
- `losses` / `risks` (lane 2) = the window's loss / risk ROWS: each one
  fault recorded at its time (a counter that grew in one tick, an event, a
  state that began). This is the dashboard's "N chýb", never `counts`
  (a kind measured in frames, `overflow` / `limited`, sums frames there);
- `counts` = loss and risk faults per kind: a counter's steps summed, ONE
  per event or state row (`model::row_counted`: `late_packet`, `host_down`,
  `mismatch`, `dropout`, `slip`, `no_return` — their amount is a
  magnitude); `overflow` counts frames;
- `events` = the path's rows that are no meter, oldest first; at most
  `MAX_EVENTS` = 500 in the whole answer, the newest (`truncated`); the
  counts and `last_loss_ms` hold every row;
- `loop` on every `loop:` path (0 with no meter);
- `worst` = the path with the most recent loss (`null`: none).

The answer's structs are `sp_core::audio_faults`'s (`FaultsAnswer`,
`PathFaults`, `FaultEvent`, `LoopSeconds`, re-exported by `answer.rs`):
the dashboard reads the very type the server writes. A new field takes
`#[serde(default)]` there.

The mock (`e2e/mock-api.mjs`) serves a FIXED fixture by default (program and
`pipeline:1` clean, FOH one `late_packet`, SoundGrid `out:out-2` two
underruns = worst, `loop:out-2` 3540 / 60 s) with the server's `hours`
rule and refusal text; the gate's mock test pins it. Lane 2's scenarios
(below) replace it until `/__mock/audio-faults-reset`.

## The gate (`e2e/audio-faults-gate.ts`)

`post-deploy-audio-asio.spec.ts` (SNV + PP's subset) and
`post-deploy-audio-outputs.spec.ts` (SNV) open their window with the
answer's `now_ms` once their outputs run (every gated ASIO output running;
FOH listed) — never before, so a host or an output still starting after
the deploy is not in it — and end with "no audio path lost a sample in
this file's window": one bounded settle (`LEDGER_SETTLE_MS` = 6 s, a
counter's step is written at the next tick), a read of `?hours=1`, and
`audioFaultFailures` (any path whose `last_loss_ms` ≥ the window's start;
its loss rows named when still listed); `audioFaultRisks` are logged. A
fault that happened up to one tick before the window opened can be stamped
inside it (counter rows carry the tick's time): accepted. Unit-tested in the
mock suite (`audio-faults-gate.spec.ts`, which also reads the mock route).

A spec that adds an output inside the window must take it away before its
receiver goes: the 96 kHz probe (`e2e-96k` to a loopback socket) waits
until `outputs[]` no longer lists it, THEN closes its socket — a VBAN send
to a closed port is a `send_error`, a loss that would fail the window.

## The dashboard (lane 2: `sp_core::audio_faults*`, `sp-ui` `audio_faults.rs`)

The owner's frequency rule (#248 ROZHODNUTÉ): the chain's health is used
every service, so it is at first glance; the details are one click away.

- **First glance: ONE HealthBar segment** right after WS, a `<button>`
  `health-audio` with `data-level` = `unknown` (no answer yet, or no path)
  | `ok` | `risk` | `loss` | `recent` (`sp_core::audio_faults::level`, from
  the 24 h answer, against the answer's own `now_ms`, never the browser's
  clock):
  - no loss: green `Zvuk: bez chyby 2 h 14 min` = since the OLDEST
    `clean_since` of the live paths (`chain_clean_since`), rounded down
    (`menej ako minútu`, `3 d 4 h`); clean for the whole window = `Zvuk:
    bez chyby viac ako 24 h`; risks only: the same green text and a
    `health-audio-risks` ` · riziká: N` (a risk is no loss, never hidden);
  - a loss: amber `Zvuk: 3 chyby za 24 h · posledná pred 5 min · <path>`
    (the losses' rows, the worst path = the newest loss), red when it is
    under `RECENT_MS` = 10 min old (10 min itself is amber);
  - the tooltip counts the window's losses and risks.
- **One click: the panel** `audio-faults-panel` under the bar (a full-width
  flex item), windows `audio-window` `data-hours` 1 / 24 / 168 / 720 (24 h
  = the segment's own answer, no second request):
  - a row per path (`audio-path-row`, `data-path`) in `row_order`
    (program, each output then its loop, pipelines by id, the rest): its
    name (`Program`, the output's configured name, `Slučka – <output>`, the
    playlist's name or `Playlist <id>`, ` (nebeží)` when not live), losses
    / risks in Slovak plural (`has-loss` / `has-risk`), `bez chyby od
    12.10. 14:05 (2 h 14 min)` or `… viac ako <window>`, a loop's
    `skontrolované 59 min · ticho 1 min`;
  - a timeline per row: an SVG of `BUCKETS` = 60 buckets, bucket i =
    `[from + ⌈i·span/60⌉, …)` so an instant's bucket is `⌊(t − from)·60 /
    span⌋` exactly; a bar (`audio-bar`, `data-loss` / `data-risk`) only for
    a bucket with a loss or risk ROW, loss red at the bottom, risk amber on
    it, to scale against the panel's largest bucket, a part that is not
    empty at least `MIN_BAR` = 2 of 24 units; one shared axis
    (`audio-axis-tick`) on whole LOCAL times (10 min, 1 h, 3 h, midnights,
    every 5th midnight), the zone's offset now;
  - the event list (`audio-fault-event`, `data-ts` / `data-path` /
    `data-severity`): every loss and risk row (no start, no meter), newest
    first, one instant's rows in row order; time `12.10. 14:05:07`, the
    path's name, the kind in Slovak (`kind_sk`, an unknown kind as sent),
    the amount in its unit (`×2`; a count of 1 nothing; µs as `12,5 ms`;
    frames as ms at the path's rate: the program's 48 kHz, an output's
    driver / output rate from `GET /api/v1/program` read when the panel
    opens, else `480 vzoriek`). A keyed `<For>` (the key holds every shown
    field), so a poll never resets the list's scroll.
- **ONE poll, one request in flight** (`AudioFaults::start`,
  `next_fetch`): the 24 h answer every 10 s; while the panel is open on
  another window, that window at once on a switch, then every 10 s; else a
  250 ms nap. A failed request keeps the last answer and is asked again 10 s
  later.
- **Local time comes from the browser's `Date`** (a `wasm_bindgen` extern in
  `audio_faults.rs`: day, month, hour, minute, second, the zone's offset; no
  `js-sys` dependency); sp-core formats the parts (`LocalTime`, `hm_sk`,
  `dm_sk`, `time_sk`).
- **Tests:** `audio_faults_tests.rs` / `audio_faults_timeline_tests.rs`
  (every text, level, boundary, bar, tick and poll decision; every listed
  mutant mapped), `answer_tests.rs::losses_and_risks_count_the_windows_rows_per_severity`,
  and the mock E2E `e2e/health-audio-faults.spec.ts` (Europe/Bratislava):
  `POST /__mock/audio-faults {scenario, delay_ms}` — `clean`, `risk`,
  `recent` (a loss 3 min ago on SoundGrid, losses 40 min and 5 h ago, two
  FOH risks), `old` (one program loss 3 h ago); every live path started
  2 h 14 min 30 s ago, offsets mid-minute so a minute text holds 30 s; the
  mock answers a scenario's rows as `build_answer` does (any window).
  Reset with `/__mock/audio-faults-reset` in `beforeEach` / `afterEach`.
- **Post-deploy** (`e2e/post-deploy-audio-faults-ui.spec.ts`, SNV's suite
  and PP's subset, read-only): one `?hours=24` read of the box's ledger, then
  the dashboard's `data-level` must agree (a path with a loss → `loss` |
  `recent`, its text `Zvuk: N chyb… za 24 h · posledná pred … · …`; none →
  `ok` | `risk`, `Zvuk: bez chyby …`), and one click lists a row per path
  of that answer, `program` among them; a clean console. It logs the
  segment's text: the owner's acceptance ("after the deploy the segment
  reads the time since the last real fault") is read from that line.

## Decisions made in lane 1 (main session may revisit)

- `coalesced` and `late_dropped` are RISKS (the design's list); a coalesced
  boundary never reached VBAN, so its loss shows elsewhere only when a
  receiver underruns. `no_return` is a RISK (unverifiable, not a proven
  loss; an LV1 that is off would otherwise fail every PP deploy).
- The NDI `SP-program` audio row is `ndi_late` (a risk) from the trace's
  `late` flag (`MinuteLog`, every record read once).
- Loop events come from `loop.recent` (32): more than 32 events in one 5 s
  tick would lose the oldest from the ledger (their counters still grow).
- A rebuilt path is only seen when a counter goes down; a rebuild whose
  counters all overtake their old values within one tick is missed (the run
  counter makes that near impossible).
