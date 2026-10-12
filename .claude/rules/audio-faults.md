---
paths:
  - "crates/sp-server/src/audio_faults/**"
  - "crates/sp-server/src/api/audio_faults*.rs"
  - "crates/sp-server/src/db/mod_tests_v38.rs"
  - "crates/sp-server/src/playback/program_bus_window*.rs"
  - "crates/sp-decoder/src/peak_limiter*.rs"
  - "e2e/audio-faults-gate*.ts"
  - "e2e/audio-faults-read.ts"
  - "e2e/post-deploy-audio-*.spec.ts"
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
  `outputs().limiter_overs()`, the trace, `input().status(..)`, the NDI
  health registry) — never HTTP, nothing on a real-time thread — and writes
  the ledger's rows in one transaction. Rows the store refuses are kept for
  the next tick (the newest `MAX_UNWRITTEN`), with a WARN.
- The decisions are pure (`ledger.rs`, `Ledger::tick`), pinned exactly in
  `ledger_tests.rs`:
  - a path seen for the first time (in this process, or again after it was
    gone) or REBUILT (its run counter, or any counter or meter, went DOWN)
    writes a `start` row, and that tick is its BASELINE: its counters,
    events and states are taken, none is written (review round 1, 🟡 4: an
    ASIO output's start-up drops wrote a loss at every SongPlayer start);
  - a step (a counter's growth, a new event, a state that starts to hold)
    is RECORDED only when the path was gated in at the tick before and was
    not rebuilt since (`was_gated && !restarted`): the tick a path comes
    in (a playlist cut to program, an output that starts running) is its
    baseline, the tick it goes out still records what grew until then (a
    host gone down while its output ran: `host_down` with the output now
    waiting), and what happens while it is out is taken, never written
    later;
  - a path rebuilt while it was gated in at the tick before writes one
    `interrupted` loss (amount 1) next to its start: an ASIO host restarted
    between two ticks (its output reads running, or opening, with
    `blocks_sent` from 0), an entry rebuilt by the outputs task while it
    ran (a delay / loop-check / network-rate change: the driver's reopen is
    a gap), a pipeline rebuilt while on program;
  - a counter that grew writes one row, the step as `amount`;
  - an event with its own UTC (a loop event, a VBAN late packet) is written
    once, AT ITS OWN TIME, deduplicated by `(path, kind, utc_ms, amount)`
    (`SEEN_KEPT` = 4096 keys, oldest dropped);
  - a state (`host_down`, `no_return`) writes one row when it starts to hold;
  - meters are summed and written at most once a minute per path
    (`METER_FLUSH_MS`), a rebuilt path's sum first; meters are never gated
    and count from the path's build.
- One INFO per tick that recorded a loss, a risk or a start
  (`audio faults: rows recorded`, `losses`, `risks`, `starts`).

## Paths, kinds, severities (`probe.rs`)

| path | loss | risk | other |
|---|---|---|---|
| `program` (`SP-program`) | `filled` (= `health.filled_owned`), `resync` (= `health.resyncs_owned`), `coalesced` (a boundary the sender's full queue dropped: never served, VBAN has no send-side gap detection) | `late_dropped`, `vban_feed_late` (`health.timing.vban_feed_late_over_budget`), `limited` (`outputs().limiter_overs()`), `ndi_late` (trace records taken over a slot late) | never gated; run = `health.submitted` |
| `out:<id>` (enabled outputs, never the post-deploy probe's `e2e-…`) | `blocks_dropped`; VBAN `blocks_substituted`, `send_error`; ASIO `underrun`, `hard_recentre`, `reset`, `resync`, `overflow` (frames), `host_down` (state: `asio.reason_code == "host_down"`) | VBAN `late_packet` (each `late_events` entry over 10 ms, amount µs); ASIO `late_callback`, `fast_callback`, `overload` | gated in while `state == "running"`; run = `blocks_sent` |
| `loop:<id>` (an ASIO output whose loop check ran: not `off`, or it has time) | each recent `mismatch` / `dropout` / `slip` event (amount = frames, a slip signed) | `no_return` (state: the check cannot tell) | gated like its output; meters `checked`, `blind`; run = checked + blind ms |
| `input` (the NDI input "OBS manuál", while its thread runs) | `resync`, `no_source` (standby boundaries: no receiver, disconnected, no video yet), `unsupported` (standby: the format) — each a silent block | — | gated in while -1 is on program; run = `boundaries` |
| `pipeline:<playlist id>` | `underrun` (`audio.underruns`), `resync` (`pacing.resyncs`), `unserviced` (`pacing.song_change_unserviced_slots`), `overflow` (`audio.overflows`: times its audio grid dropped its oldest samples over the 2 s cap) | — | gated in while on program; run = `pacing.seq` |

**On program** (`probe::on_program`, from the bus's status, no longer the
authority's on-air set) = the selected source and the outgoing side of a
running fade (`transition.active.from`: mixed until the fade ends, up to
~10.5 s with its cue wait).

`overflow` counts frames on an ASIO output and events on a pipeline:
`counts` are per path, so the units never mix.

Not faults, by design: `pacing.repeats` (a 24/25 fps song's conversion, a
paused standby), `pacing.dropped` (60→30 decimation), `late_frames`, an
ASIO `no_clock` calm wait and its `clock_waits` (the output is not
running), a Blank / nothing-selected fill or resync, an output's start-up
and driver open (not running), the post-deploy probe's own outputs (their
receiver closes after the probe; `PROBE_OUTPUT_PREFIX`).

### Owned fills and resyncs (`program_bus_window.rs`)

ONE predicate, `ProgramCore::owned(stamp, shows)`: inside an OPEN window
either side's source is real (`real_source`: a playlist or the NDI input
-1; never Blank -2 or none), so a fade INTO Blank whose playlist missed a
boundary is an owned fill (review round 1, 🟡 2: the fill shows Blank);
anywhere else the source the boundary shows. `count_fill(stamp, shows)`
(the sender's wall fill and a held window boundary) → `filled_owned`;
`count_resync(stamp)` (the release path's > 8-slot skip, from its first
skipped boundary) → `resyncs_owned`; a fade's missing real side
(`commit_mix`, at its weight) → `filled_owned`. `filled` / `resyncs` keep
their meaning (every fill, every resync). `program_bus.rs` is at the
1000-line cap: the counting lives in the window child; each call site is
one line. Tests: `program_bus_window_tests.rs`.

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
- the post-deploy probe's own outputs (`out:e2e-…` / `loop:e2e-…`) are in
  no answer and never the worst: the ledger never watches them
  (`probe::watched`), so they have no row and no live path (review round
  1, 🔵 13; one predicate at the source instead of a filter in three
  queries);
- `clean_since_ms` = the path's last `loss` row in the window, else its
  latest `start` row in it, else the window's start (#257 acceptance: after
  a deploy it still reads the last REAL fault); `last_loss_ms` is what the
  gate reads;
- `counts` = loss and risk faults per kind: a counter's steps summed, ONE
  per event or state row (`model::row_counted`: `late_packet`, `host_down`,
  `mismatch`, `dropout`, `slip`, `no_return` — their amount is a
  magnitude);
- `events` = the path's rows that are no meter, oldest first; at most
  `MAX_EVENTS` = 500 in the whole answer, the newest (`truncated`); the
  counts and `last_loss_ms` hold every row;
- `loop` on every `loop:` path (0 with no meter);
- `worst` = the path with the most recent loss (`null`: none).

The mock (`e2e/mock-api.mjs`) serves a FIXED fixture (program and
`pipeline:1` clean, FOH one `late_packet`, SoundGrid `out:out-2` two
underruns = worst, `loop:out-2` 3540 / 60 s) with the server's `hours`
rule and refusal text, for lane 2's UI.

## The gate (`e2e/audio-faults-gate.ts` pure, `e2e/audio-faults-read.ts` the box reads)

`post-deploy-audio-asio.spec.ts` (SNV + PP's subset) and
`post-deploy-audio-outputs.spec.ts` (SNV):

- a fallback start in `beforeAll` (`fileStartOrNull`: the server's clock,
  `null` when the ledger does not answer; never a failure itself);
- the window opens once their outputs run (every gated ASIO output
  running; FOH listed), ONE settle LATER (`openFaultWindow`:
  `LEDGER_SETTLE_MS` = 11 s — a pipeline's counters reach the registry with
  its 5 s heartbeat, the ledger writes at its next 5 s tick — then the
  answer's `now_ms`), so a fault from before is stamped before the window
  (review round 1, 🔵 6-7);
- the file ends with "no audio path lost a sample in this file's window"
  (`expectNoLossSince`): one settle, a read of `?hours=1`, every path
  logged (`audioFaultSummary`) and its risks (`audioFaultRisks`), then
  `audioFaultFailures` (any path whose `last_loss_ms` ≥ the window's start;
  its loss rows named when still listed). A window that never opened (an
  earlier test failed and the restarted worker lost it) still reads and
  logs the answer from the fallback start, then fails with those losses in
  its message (🔵 8: never a failure without the ledger's evidence).

Unit-tested in the mock suite (`audio-faults-gate.spec.ts`, which also
reads the mock route).

A spec that adds an output inside the window must take it away before its
receiver goes: the 96 kHz probe (`e2e-96k` to a loopback socket) waits
until `outputs[]` no longer lists it, THEN closes its socket (the ledger no
longer watches `e2e-…` outputs, but the order costs nothing and keeps the
probe honest).

## Decisions (review round 1; the main session may revisit)

- `coalesced` is a LOSS (the sender dropped the oldest unsent boundary,
  `submit_handoff.rs`; it never reached VBAN or NDI). `late_dropped` stays
  a RISK (the boundary itself went out, filled or forwarded). `no_return`
  is a RISK (unverifiable, not a proven loss; an LV1 that is off would
  otherwise fail every PP deploy).
- A path's first, rebuilt and gate-in tick is its baseline (the
  coordinator's ruling in review round 1). What that costs: faults between
  a path's build and the ledger's first sight of it (≤ one tick; the
  ledger starts with the program) are not recorded, and the tick that
  first sees a playlist (or the input) on program, or an output running,
  records nothing of the ≤ 5 s before it — part of which was already on
  program, or running.
- An ASIO host restart (review round 1, 🔵 14): SongPlayer's
  `blocks_dropped` of an ASIO output = its own queue's drops + the host's
  latest queue drops + the blocks no session took (`AsioOut::snapshot`);
  the first and the last never reset, the host's count and `blocks_sent`
  start at 0 in a new host. The ledger reads the restart as a rebuild
  (`blocks_sent` went down), so that tick is a baseline: nothing is counted
  twice. The break is recorded either as `host_down` (a tick saw the output
  waiting with that reason; the tick it went out still counts the unsent
  blocks until then) or as `interrupted` (the host was back before the next
  tick). The blocks left unsent between the last tick and a host restart
  inside one tick are in neither row's amount.
- VBAN late packets (🔵 14): `VbanLateEvent` serves no packet index, so the
  dedup key takes the amount (the lateness, µs): a burst's packets differ
  by their spacing, so they stay separate rows; two packets late in the
  same ms by the same µs would be one row.
- The NDI `SP-program` audio row is `ndi_late` (a risk) from the trace's
  `late` flag (`MinuteLog`, every record read once).
- Loop events come from `loop.recent` (32): more than 32 events in one 5 s
  tick would lose the oldest from the ledger (their counters still grow).
  An event is recorded once its run ended (`asio_loop.rs::record`), so its
  amount never changes after it is listed.
- A rebuilt path is only seen when a counter goes down; a rebuild whose
  counters all overtake their old values within one tick is missed (the run
  counter makes that near impossible).
- The NDI input path exists while its thread runs (Windows with the NDI
  SDK), on program or not; an input switched off while on program offers
  standby with no counters (by design: the operator's choice).
