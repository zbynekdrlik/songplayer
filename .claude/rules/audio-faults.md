---
paths:
  - "crates/sp-server/src/audio_faults/**"
  - "crates/sp-server/src/api/audio_faults*.rs"
  - "crates/sp-server/src/db/mod_tests_v38.rs"
  - "crates/sp-server/src/db/mod_tests_v39.rs"
  - "crates/sp-server/src/playback/program_bus_window*.rs"
  - "crates/sp-decoder/src/peak_limiter*.rs"
  - "e2e/audio-faults-gate*.ts"
  - "e2e/audio-faults-read.ts"
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

## The table (V38, V39) and the writer

- `audio_faults(ts_ms, path, kind, severity, amount, detail)`, indexes on
  `(ts_ms)` and `(path, ts_ms)` (V38) and `(severity, path, ts_ms)` (V39,
  lane 2: `store::clean_marks` reads the loss and start rows only, never a
  loop check's meters). `severity` = `loss` | `risk` | `info` (a
  path's start) | `meter` (a loop check's checked / blind ms). Kept 90 days
  (`RETENTION_MS`), pruned at the first tick and then once a day — except
  each path's EARLIEST `start` row (#257 review, `store::prune`: a path's
  first start bounds its clean time for good, so a pruned deploy never
  resets it; a handful of rows).
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
| `out:<id>` (enabled outputs, never the post-deploy probe's `e2e-…`) | `blocks_dropped`; VBAN `blocks_substituted`, `send_error`; ASIO `underrun`, `hard_recentre`, `reset`, `resync`, `overflow` (frames), `host_down` (state: `asio.reason_code == "host_down"`); Dante `pump_missed` | VBAN `late_packet` (each `late_events` entry over 10 ms, amount µs); ASIO `late_callback`, `fast_callback`, `overload`; Dante `pump_late`, `pump_overrun` (#285; a Dante output also carries the ASIO counters of its seam) | gated in while `state == "running"`; run = `blocks_sent` |
| `loop:<id>` (an ASIO output whose loop check ran: not `off`, or it has time) | each recent `mismatch` / `dropout` / `slip` event (amount = frames, a slip signed) | `no_return` (state: the check cannot tell) | gated like its output; meters `checked`, `blind`; run = checked + blind ms |
| `input` (the NDI input "OBS manuál", while its thread runs) | `resync`, `no_source` (standby boundaries: no receiver, disconnected, no video yet), `unsupported` (standby: the format) — each a silent block | — | gated in while -1 is on program; run = `boundaries` |
| `pipeline:<playlist id>` | `underrun_start` (`audio.underruns_start`: a song's, seek's or resume's audio cut short before its first full block), `underrun` (`audio.underruns_mid`: cut short mid-song, the decode behind the cushion), `underrun_end` (`audio.underruns_end`: a song that ended before the decoder reached its audio's end — since #257 step 2 only audio more than 15 s past the picture, a decode error or a dead producer: a shorter tail plays on the held last frame), `resync` (`pacing.resyncs`), `unserviced` (`pacing.song_change_unserviced_slots`), `overflow` (`audio.overflows`: times its audio grid dropped its oldest samples over the 2 s cap) | — | gated in while on program; run = `pacing.seq` |

**On program** (`probe::on_program`, from the bus's status, no longer the
authority's on-air set) = the selected source and the outgoing side of a
running fade (`transition.active.from`: mixed until the fade ends, up to
~10.5 s with its cue wait).

`overflow` counts frames on an ASIO output and events on a pipeline:
`counts` are per path, so the units never mix.

**A song's natural end is no underrun (#257).** The block a song's audio
ends in, and a block after it (a picture that outlasts its audio), plays
every sample the song has and pads the rest: once the decoder handed out
the song's last sample (`SplitSyncedDecoder::audio_ended` → the pacer's
`audio_source_ended`), a short block counts nothing. Before the fix every
natural end on program wrote a `pipeline:<id>` `underrun` loss (the gate's
first SNV run, 12.10.2026, 21:35:59Z) and a tail longer than one block was
dropped; the song summary's `audio_underruns` was then the pipeline's
LIFETIME count, so per-song averages read from it (the "~9 per ended song")
were running totals. Mechanism, fix and box check: `genlock.md` "A song's
natural end". Step 2: audio that runs past the picture (a dub's tail) plays
on the held last frame up to 15 s, so its end is no `underrun_end` either
(`genlock.md` "Audio past the picture plays on the held last frame"; the dub
worker no longer writes drain silence past the content, `dabing.md`).

Not faults, by design: `pacing.repeats` (a 24/25 fps song's conversion, a
paused standby), `pacing.dropped` (60→30 decimation), `late_frames`, an
ASIO `no_clock` calm wait and its `clock_waits` (the output is not
running), a Blank / nothing-selected fill or resync, an output's start-up
and driver open (not running), the post-deploy probe's own outputs (their
receiver closes after the probe; `PROBE_OUTPUT_PREFIX`).

### Owned fills and resyncs (`program_bus_window.rs`)

ONE predicate, `ProgramCore::owned(stamp, shows)`: inside an OPEN window
either side's source DELIVERS, so a fade INTO Blank whose playlist missed a
boundary is an owned fill (review round 1, 🟡 2: the fill shows Blank);
anywhere else the source the boundary shows. `delivering(source, stamp)` =
a REAL source (`real_source`: a playlist or the NDI input -1; never Blank
-2 or none) that owes the boundary (`program_bus_window::Delivery::owes`):

- it touched or offered a boundary since it became the owner (`Delivery`'s
  set, filled by `note_progress` — `touch` and `offer` both go through it;
  a cut's new owner is taken out of it by `became_owner`, so a touch from
  BEFORE the cut does not count), OR
- `DELIVERY_GRACE_100NS` (300 boundaries, 10 s, exact on the grid) passed
  since it became the owner: the cut boundary (`became_owner`), else the
  first boundary the core counted (a fill, a resync, a mix: the start the
  restored selection owns from).

Rules:

- **A fill of a source that has delivered nothing yet is a WAIT, never a
  loss — for 10 s at most** (#257, SNV 12.10.2026, the deploy of 26a5508d:
  the restore of source 4 at 23:17:00.768, the ledger's baseline at .770,
  its Play at .830, and the next tick wrote a `program` `filled` loss — the
  bus filled the restored owner's boundaries before its pipeline offered
  anything; the HealthBar would read "Zvuk: 1 chyba za 24 h … Program" for
  a day after every deploy). The startup restore, and a cut to a source
  that has not reached the bus yet, count in the plain `filled` / `resyncs`
  only, until the grace runs out.
- **The grace bounds the wait** (review: a source cut to while its
  pipeline hangs never touches the bus; without a bound its fills were a
  wait forever, the program black and silent, the HealthBar "bez chyby").
  Rejected: keeping a source cut to in the set when its last touch is
  within `PROGRAM_LIVE_WINDOW_100NS` — a source that hung BEFORE the cut
  would still wait forever. A running fade's stalled outgoing source cut
  back to waits the same 10 s from the cut back.
- A source that delivered and then missed a boundary (its pipeline fell
  behind, a coalesce gap) still counts at once: `filled_owned`,
  `resyncs_owned`, a fade's missing side.
- In practice every paced source (and the NDI input) touches on every
  boundary, playing or not, so a source cut to is delivering again within
  about one boundary; only one that stopped reaching the bus reads as a
  wait, and only for 10 s.
- Known edge: a fade's boundaries BEFORE a cut back to its stalled
  outgoing source are released after that cut (the 3-slot grace), so that
  source's missing side there is judged under its new ownership: a wait.

`count_fill(stamp, shows)` (the sender's wall fill and a held window
boundary) → `filled_owned`; `count_resync(stamp)` (the release path's skip
of more than 8 slots, from its first skipped boundary) → `resyncs_owned`; a
fade's missing real side (`commit_mix`, at its weight) → `filled_owned`. `filled` /
`resyncs` keep their meaning (every fill, every resync). `program_bus.rs` is
at the 1000-line cap: the counting and `Delivery` live in the window child;
each call site is one line. Tests: `program_bus_window_tests.rs`
(`the_restored_selection_owes_nothing_before_it_delivered`,
`a_source_cut_to_owes_its_misses_only_once_it_delivered_since_the_cut`,
`the_restored_selection_owes_its_misses_once_the_delivery_grace_ran_out`,
`a_hung_source_cut_to_owes_its_misses_once_the_delivery_grace_ran_out`,
`a_stalled_fade_source_cut_back_to_owes_its_misses_once_the_grace_ran_out`,
`blank_owes_nothing_past_the_delivery_grace`; the older cases deliver b(1)
first).

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
`{now_ms, from_ms, hours, kept_from_ms, clean_since_ms, worst, truncated,
paths[{path, detail, live, clean_since_ms, last_loss_ms, last_risk_ms,
losses, risks, counts{kind: n}, events[{ts_ms, kind, severity, amount}],
loss_buckets[60], risk_buckets[60], starts[ts_ms], loop?{checked_s,
blind_s}}]}`:

- the paths: every path that runs now (`probe::live_paths`, its own name)
  and every path with a row in the window (its newest row's `detail`);
- the post-deploy probe's own outputs (`out:e2e-…` / `loop:e2e-…`) are in
  no answer and never the worst: the ledger never watches them
  (`probe::watched`), so they have no row and no live path (review round
  1, 🔵 13; one predicate at the source instead of a filter in three
  queries);
- `clean_since_ms` — **a planned restart is no fault** (the main
  session's ruling on #257: the owner asks how long the audio has been free
  of losses): the path's newest `loss` row the ledger KEEPS (90 days),
  looked up BEYOND the asked window (`store::clean_marks` over
  `kept_from(now)`, ONE extra grouped query that reads V39's index alone:
  `CASE WHEN severity = 'info'`, the window's own losses too), else its
  FIRST `start` row EVER (never the latest: a deploy does not reset it;
  the prune keeps it, #257 review; one older than the 90 days reads
  `kept_from_ms`, "viac ako 90 d", for the path and the chain: no loss row
  that old is left to disprove it), else `kept_from_ms` when the window
  has rows of the path, else now (the ledger has not seen it yet: no
  claim). `interrupted` stays a
  loss. The answer's own `clean_since_ms` is the whole CHAIN's: the newest
  loss of ANY path kept (a path gone since counts), else the first start
  ever (no older than `kept_from_ms`), else the same fallback. `kept_from_ms` = now − 90 d. `last_loss_ms`
  and `worst` stay the window's (the gate reads `last_loss_ms`);
- `losses` / `risks` (lane 2) = the window's loss / risk ROWS: each one
  fault recorded at its time (a counter that grew in one tick, an event, a
  state that began). This is the dashboard's "N chýb", never `counts`
  (a kind measured in frames, `overflow` / `limited`, sums frames there);
- `counts` = loss and risk faults per kind: a counter's steps summed, ONE
  per event or state row (`model::row_counted`: `late_packet`, `host_down`,
  `mismatch`, `dropout`, `slip`, `no_return` — their amount is a
  magnitude);
- `events` = the path's rows that are no meter, oldest first; at most
  `MAX_EVENTS` = 500 in the whole answer, the newest (`truncated`); the
  counts and `last_loss_ms` hold every row;
- `loss_buckets` / `risk_buckets` (#257 review) = the window cut into
  `BUCKETS` = 60 buckets by sp-core's `bucket_edges` (bucket i = [from +
  ⌈i·span/60⌉, …), the edges the timeline's `bucket_of` uses), each
  bucket's loss / risk ROWS — EVERY row of the window, never the capped
  `events` (`store::bucket_counts`: one `GROUP BY path, severity, bucket`
  whose `CASE` binds the edges, `bucket_case`); `starts` = the path's start
  rows in the window, the newest `MAX_STARTS` = 100, oldest first
  (`store::window_starts`); both put on by `answer::add_timeline`. The
  dashboard draws its bars and marks from these only, so a loss older
  than the newest 500 events keeps its bar;
- `loop` on every `loop:` path (0 with no meter);
- `worst` = the path with the most recent loss (`null`: none).

The answer's structs are `sp_core::audio_faults`'s (`FaultsAnswer`,
`PathFaults`, `FaultEvent`, `LoopSeconds`, re-exported by `answer.rs`):
the dashboard reads the very type the server writes. A new field takes
`#[serde(default)]` there.

The mock (`e2e/mock-api.mjs`) serves a FIXED fixture by default (program and
`pipeline:1` clean, FOH one `late_packet`, SoundGrid `out:out-2` two
underruns = worst, `loop:out-2` 3540 / 60 s; `kept_from_ms` and the chain's
`clean_since_ms` = that loss) with the server's `hours` rule and refusal
text; the gate's mock test pins it. Lane 2's scenarios
(below) replace it until `/__mock/audio-faults-reset`.

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

## The dashboard (lane 2: `sp_core::audio_faults*`, `sp-ui` `audio_faults.rs`)

The owner's frequency rule (#248 ROZHODNUTÉ): the chain's health is used
every service, so it is at first glance; the details are one click away.

- **First glance: ONE HealthBar segment** right after WS, a `<button>`
  `health-audio` with `data-level` = `unknown` (no answer yet, or no path)
  | `ok` | `risk` | `loss` | `recent` (`sp_core::audio_faults::level`, from
  the 24 h answer, against the answer's own `now_ms`, never the browser's
  clock):
  - no loss in the 24 h: green `Zvuk: bez chyby 3 d 4 h` = since the
    answer's own (the chain's) `clean_since_ms`, ACROSS deploys (a loss
    before the window counts, a restart never resets it), rounded down
    (`menej ako minútu`, `2 h 14 min`, `3 d 4 h`; `clean_span_sk`); clean
    for all the ledger keeps = `Zvuk: bez chyby viac ako 90 d`; risks only:
    the same green text and a `health-audio-risks` ` · riziká: N` (a risk
    is no loss, never hidden);
  - a loss: amber `Zvuk: 3 chyby za 24 h · posledná pred 5 min · <path>`
    (the losses' rows, the worst path = the newest loss), red when it is
    under `RECENT_MS` = 10 min old (10 min itself is amber);
  - the tooltip counts the window's losses and risks.
- **One click: the panel** `audio-faults-panel` under the bar (a full-width
  flex item), windows `audio-window` `data-hours` 1 / 24 / 168 / 720 (24 h
  = the segment's own answer, no second request):
  - a row per path (`audio-path-row`, `data-path`) in `row_order`
    (program, the NDI input, each output then its loop, pipelines by id,
    the rest): its name (`Program`, `Vstup OBS manuál`, the output's
    configured name, `Slučka – <output>`, the playlist's name or
    `Playlist <id>`, ` (nebeží)` when not live), losses / risks in Slovak
    plural (`has-loss` / `has-risk`), `bez chyby od 9.10. 14:05 (3 d 4 h)`
    (its own `clean_since_ms`, beyond the window) or `bez chyby viac ako
    90 d`, a loop's `skontrolované 59 min · ticho 1 min`;
  - a timeline per row: an SVG of `BUCKETS` = 60 buckets, bucket i =
    `[from + ⌈i·span/60⌉, …)` so an instant's bucket is `⌊(t − from)·60 /
    span⌋` exactly; a bar (`audio-bar`, `data-loss` / `data-risk`) only for
    a bucket with a loss or risk ROW, read from the answer's
    `loss_buckets` / `risk_buckets` (`buckets_of`), loss red at the
    bottom, risk amber on it, to scale against the panel's largest bucket,
    a part that is not empty at least `MIN_BAR` = 2 of 24 units; each of
    the answer's `starts` a dashed MARK at its place, in whole SVG units
    (`mark_x`; `audio-start-mark`, `data-ts`, tooltip `spustenie 12.10.
    14:05:07`; `start_marks`), never a bar (an `interrupted` restart is a
    loss and has its bar); one shared axis (`audio-axis-tick`) on whole
    LOCAL times (10 min, 1 h, 3 h, midnights, every 5th midnight), each
    tick snapped with the zone's offset AT that tick (`axis_ticks(from,
    to, &offset_east)`, #257 review): a midnight stays a midnight across a
    daylight-saving change and the ticks only move forward (a local time
    the spring change skips maps to an instant already ticked; the
    autumn's repeated hour is ticked once, its summer reading);
  - the rows are a `<For>` keyed on their whole shown content (`RowView`:
    the texts, the bars, the marks), so a poll that changes nothing in a
    row keeps its element and a hovered tooltip (#257 review); a row whose
    clean span (whole minutes) or a bar's tooltip (its bucket's minutes)
    changes is drawn anew, at most about once a minute;
  - the event list (`audio-fault-event`, `data-ts` / `data-path` /
    `data-severity`): every loss and risk row (no start, no meter), newest
    first, one instant's rows in row order; time `12.10. 14:05:07`, the
    path's name, the kind in Slovak (`kind_sk`, an unknown kind as sent),
    the amount in its unit on its path (`unit_of(path, kind)`: `overflow`
    is frames on an ASIO output and a count on a pipeline, `×3`; `×2`; a
    count of 1 nothing; µs as `12,5 ms`;
    lane 1's kinds `coalesced` "zahodený blok programu", `interrupted`
    "prerušenie", `fast_callback` "predčasné volanie", `overload`
    "preťaženie", `no_source` "bez zdroja", `unsupported` "nepodporovaný
    formát"; a pipeline's `underrun_start` "podtečenie na začiatku" and
    `underrun_end` "podtečenie na konci" (#257, `underrun` stays
    "podtečenie");
    frames as ms at the path's rate: the program's 48 kHz, an output's
    driver / output rate from `GET /api/v1/program` read when the panel
    opens, else `480 vzoriek`). A keyed `<For>` (the key holds every shown
    field), so a poll never resets the list's scroll.
- **ONE poll, one request in flight** (`AudioFaults::start`,
  `next_fetch`): the 24 h answer every 10 s; while the panel is open on
  another window, that window at once on a switch, then every 10 s; else a
  250 ms nap. A failed request keeps the last answer and is asked again 10 s
  later; after `MAX_FAILED_POLLS` = 3 failed requests in a row
  (`failed_after`) the segment reads grey `Zvuk: nedostupné` (`data-level`
  unknown, `unavailable`) and the open panel names its answer's age by the
  page's clock (`audio-faults-stale`, "Server neodpovedá (3× po sebe) —
  údaje sú z poslednej odpovede pred 1 min.", `stale_sk`); the next answer
  ends it. A closed panel forgets its window (`polls_for`) and drops that
  window's answer (`AudioFaults::set_open`; one landing after the close is
  dropped by the poll before any render), so a reopened panel reads
  "Načítavam…" until a fresh answer lands, never a stale week.
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
  FOH risks), `old` (one program loss 3 h ago), `restart` (every path first
  started 10 d ago, a program loss 3 d 4 h ago, a deploy 2 h 14 min ago:
  "Zvuk: bez chyby 3 d 4 h"), `flood` (600 FOH risks in five clusters 1 /
  3 / 5 / 7 / 9 h ago and a program loss 20 h ago: past the newest 500
  events, so only the timeline shows it); `fail: true` answers every read
  500, `freeze: true` answers at one fixed `now` (two answers differ only
  where the ledger does: the keyed-rows test); the live paths are program,
  `input`, FOH,
  SoundGrid + its loop and `pipeline:1`; every live path (re)started 2 h
  14 min 30 s ago, offsets mid-minute so a minute text holds 30 s; the mock
  answers a scenario's rows as `build_answer` does (any window, the clean
  times over its 90 days).
  Reset with `/__mock/audio-faults-reset` in `beforeEach` / `afterEach`.
- **Post-deploy** (`e2e/post-deploy-audio-faults-ui.spec.ts`, SNV's suite
  and PP's subset, read-only): a `?hours=24` read of the box's ledger before
  the page and one after its segment renders; the segment's `data-level`
  and text must agree with EITHER read (#257 review: a fault recorded
  between them; a path with a loss → `loss` | `recent`, its text `Zvuk: N
  chyb… za 24 h · posledná pred … · …`; none → `ok` | `risk`, `Zvuk: bez
  chyby …`; `Zvuk: nedostupné` agrees with neither), and one click lists a
  row per path of either read, `program` among them; a clean console. It logs the
  segment's text: the owner's acceptance ("after the deploy the segment
  reads the time since the last real fault") is read from that line.

## Decisions (review round 1; the main session may revisit)

- `clean_since_ms` never restarts at a deploy (the main session's ruling,
  lane 2 merge round): a planned restart is no fault; only a loss (an
  `interrupted` one included, looked up over the 90 days kept) or the
  ledger's first sight of a path (its first start, which the prune keeps
  for good: #257 review, no saw-tooth once it ages past 90 days) bounds
  it. A start is drawn as a mark.

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
