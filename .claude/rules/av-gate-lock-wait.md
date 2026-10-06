---
paths:
  - "e2e/probe-lock-wait*.ts"
  - "e2e/av-sync-budget*.ts"
  - "e2e/post-deploy-av-sync.spec.ts"
  - "e2e/obs-audio-wait*.ts"
---

# The A/V gate's lock wait on cg OBS's genlock (#221 dev.19)

The post-deploy A/V gate (`e2e/post-deploy-av-sync.spec.ts`, the rest of it in
`obs-ndi-health.md`) records `SP-program` through cg OBS's probe input "A/V
gate SP-program", attached just before each take. camera-box's genlock audio
pairing WITHHOLDS the probe's audio from cg OBS's mix until it places it (up
to 10 s), and the dev.18 meter wait (`obs-audio-wait.ts`) is tapped BEFORE
that withhold. So before the meter, every take waits on cg OBS's genlock
state (`e2e/probe-lock-wait.ts`; ROZHODNUTÉ 6012938501 option 3, on
camera-box's read path, comment 6012957666): lock → meter → StartRecord,
through `probeReadyForTake` (the meter wait starts only once the lock wait
resolved; pinned in `probe-lock-wait.spec.ts`).

**It is NOT an exact view of the withhold.** Four residuals, verified in
camera-box's source and escalated as the open design question on #221
(comments 6014055098, 6014658984). Read them before trusting a green or
"fixing" a red.

## The endpoint

- `GET :8899/bundle-state.json`, camera-box's health server on cg OBS's box
  (`scripts/bundle-state-server.py`, binds 0.0.0.0). Each request is a FULL
  gather (the log read, an obs-websocket read, the Windows process facets):
  camera-box notes ~6.6 s answers, their `:8899` watchdogs allow 10 s (they
  only SKIP a pass on failure), their one gating consumer allows 30 s, and a
  cold gather was once ~18.7 s (`bundle-state-gather-latency.md`).
- Object `genlock_lock`, parsed AT REQUEST TIME from the NEWEST
  `genlock-lock-json:` line in cg OBS's log (`scripts/bundle_state_genlock.py`):
  `state` (LOCKED | DEGRADED | UNLOCKED), `reason` (none | audio_pairing |
  recent_event | qpc_drift | …), `n_idle` (schema v6+),
  `inputs["A/V gate SP-program"]` = `{locked, connected, idle, latency_ms,
  underruns, relocks, late_holds, depth}`, plus `recent_event_inputs` /
  `audio_unexpected_inputs` when they name an offender.
- camera-box OMITS the object when the log tail holds no such line, and a
  pre-v6 line (no `n_idle`) defaults every `idle` to false: both are refused
  (`parseGenlockLock`), never waited on.
- A line is written on a box `state` / `reason` / media-clock CHANGE, else
  every ~30 s (`OBSBasicStatusBar.cpp`, `GENLOCK_JSON_HEARTBEAT_TICKS`). A
  per-input change (the probe connecting or locking) writes nothing.
- `CG_BUNDLE_STATE_URL` (a URL or a comma list), default `127.0.0.1`,
  `localhost`, then `resolume.lan` (the name camera-box gave), all
  `:8899/bundle-state.json`: the gate runs ON cg OBS's box, so loopback
  answers or refuses at once; a LAN name can hang on DNS or a route.
  `resolveBundleState` picks the first URL that answers with a v6+ facet,
  once per run, at the start of the test body, before any scene switch:
  none answering fails the gate before it switches anything.

## What the fields really are (camera-box libobs + statusbar widget)

- `locked` = the FIFO frame-queue lock (`obs-source.c`
  `genlock_locked_next_boundary_ns != 0`), zeroed only on a backward-step
  regime end or a latency-pin rise: NEVER when the probe is idled or starves.
  An idled probe keeps `locked: true`.
- `connected` = DistroAV's live NDI connection count > 0, written on every
  receiver loop; after the probe is idled (`ndi_source_name` "" stops the
  receiver thread) nothing writes it, so it keeps its last value (true).
  Defaults true on old builds.
- `idle` = a CONNECTED input with < 60 frames over the last 60 s, classified
  once its sample ring spans ≥ 54 s; an absent input is never idle. An idled
  probe reads `idle: true` about a minute later (up to ~90 s for a LINE that
  says so). The probe is idle for minutes between runs, so a line with it
  `idle: false` is from after the attach: the freshness proof.
- `audio_pairing` = a genlock input's audio is PLACED but more than 33 ms
  from its video (the slew after placement). While the pairing still
  WITHHOLDS (hold mode PENDING, no video delay applied yet), libobs reads the
  offset as 0, "so the LOCK widget does not read DEGRADED".
- Phase events (`relocks + late_holds + backward_steps`) count 0 while an
  input is idle or absent; any RISE of the box's sum latches `recent_event`
  for 60 s (it outranks `audio_pairing`).

## The condition (all at once, compared exactly)

The probe's `connected === true`, `idle === false`, `locked === true`, the
box `state === "LOCKED"` and `reason === "none"` — the decided four plus
`connected` (review round 1: an ABSENT input is never idle and keeps its FIFO
lock, so a senderless probe line would read as a GO).

## The four residuals

1. **A false GO:** a HEARTBEAT written between the probe reading `idle:
   false` and the audio's placement can read as a GO during the withhold.
   The probe reads `idle: false` 60 frames (~2–2.4 s) after the bind, or
   from the bind on: DistroAV reports `connected: false` while it reconnects,
   a widget tick in that gap drops the probe's idle sample ring, and a fresh
   ring stays unclassified (`idle: false`) for ~54 s. So the window can run
   from the bind to the placement (~3 s on camera-box's 6.10 attach, up to
   ~10 s if the withhold runs its 10 s). Normally the wait says GO on the
   LOCKED/none CHANGE line that ends the attach's DEGRADED/`audio_pairing`
   slew (6.10: attach 08:49:33.1, shallow latch 36.13, DEGRADED 36.22, LOCKED
   37.21).
2. **A false red:** an attach with no `audio_pairing` phase writes no change
   line, so the first fresh line is the next heartbeat (≤ 30 s): the 15 s
   bound can fail on a healthy probe.
3. **A woken probe latches `recent_event`:** its LIFETIME phase events come
   back as a rise when it wakes, so DEGRADED/`recent_event` holds for 60 s.
   cg OBS is long-lived (the E2E starts it only when it is not running), so
   once the probe has had one phase event every later attach can reach the
   bound. The gate logs the probe's `relocks + late_holds` before the attach
   (`probePhaseEvents`; non-zero predicts it; `backward_steps` is not in the
   facet) and the bound's explanation names the wake from THAT count, never
   from `recent_event_inputs`: camera-box names its TOP LIFETIME offender
   there (recomputed every tick), not the input whose count rose, so a probe
   wake can be named as another input and another input's real event as the
   probe. A camera-box wake re-baseline fix is the cure, not a longer bound.
4. **The refusal trusts the newest line before the attach:** a probe whose
   FIFO locked AFTER that line (unlocked in it) and was idled since keeps
   `locked: true`, so a heartbeat before this run's attach could read as a
   GO. It needs that line to fall in the short window between the probe's
   idle flip and its lock.

The per-input truth is `audio_hold=` (off | latency | timecode | pending) in
the probe's `genlock-fifo audit` line, every ~5 s, in cg OBS's own log only.

## The refusal before the attach

`probeAttachRefusal`, on the facet the endpoint resolution read: a probe line
that could already read as a GO — connected, `idle: false` AND `locked: true`
(it received within the last minute: a run cancelled mid-take) — means no
later line could be proven to come from after this run's attach. The gate
idles the probe, fails loudly (appending "the gate idled it" or why idling
failed), and the next run passes once the probe has been idle for ~90 s. Not
refused: an absent probe (its idle proves nothing; the wait needs a connected
line), an unlocked one (a never-attached probe right after cg OBS starts reads
connected and not yet classified idle; residual 4), a probe camera-box does
not list yet (a fresh box). The refusal is logged before the idle, and the
idle is bounded at 5 s (`PROBE_IDLE_BOUND_MS`): obs-websocket-js never
settles a call whose socket closes.

## Cadence, bound and failures

- A read every 250 ms after the previous one answers, bounded at 15 s from
  the call. A read gets 10 s (`LOCK_READ_TIMEOUT_MS`, at least 1 ms: Playwright
  reads 0 as "no timeout"); a failed read is counted and RETRIED while the
  bound allows.
- The log is read first, so a read sees the state at its START: a read that
  starts within the bound counts even when it answers after it, and none
  starts after it (`LOCK_WAIT_WORST_MS` = 25 s).
- Fails loud, never a skip: at the bound with the last facet (state, reason,
  the probe's fields, any named offender), the failed reads, the trail of
  changes and what the first unmet condition means (`explainProbeLock`), or
  "no read answered".
- Log line: `A/V gate take N: probe locked after X ms (R reads of <url>, F
  failed, the slowest S ms: +0 ms LOCKED/none, probe locked=true idle=true →
  +3300 ms DEGRADED/audio_pairing, … → …)`. Read `the slowest` and `failed`
  first when camera-box's answers get slow.
- camera-box's server is only READ; on cg OBS the gate writes only its own
  probe's settings (pointing it, idling it).

## The budget (`e2e/av-sync-budget.ts`, pinned by `av-sync-budget.spec.ts`)

`TEST_TIMEOUT_MS` 345 s; `WORST_TAKE_MS` 225 s = skip 15 + play 30 + the lock
wait's worst 25 + the audio wait 20 + record 20 + stop 10 + analysis 60 +
cleanup 35 (`REMUX_SIBLING_WAIT_MS` 15 + 2 × `BUSY_RETRY_MS` 10) + 2 evidence
copies × 5; `RETAKE_BEFORE_MS` = 345 − 225 − `UNCOUNTED_CALLS_MS` 10 = 110 s
(the 10 s covers the calls the sum does not count: the audio wait's two
`Reidentify` round trips, the StartRecord pre-check, the `/mix` and `/videos`
reads, spawning the analysis; the lock wait's reads are inside its own worst
case).
The spec waits on these same constants. A wait that joins a take adds its
worst case to both `WORST_TAKE_MS` and `TEST_TIMEOUT_MS`, so the retake room
stays 110 s (dev.18: 300 → 320 s; dev.19: 320 → 345 s). The one-off endpoint
resolution before the first take is outside the take sum (≤ 10 s per URL
tried; loopback refuses at once).

## Testing it

The helpers take their clock, pause and read as arguments: a fake clock that
advances in each sleep and each fake read runs the whole wait (slow reads,
the bound, a read started at it) in milliseconds. A mutant that stops
advancing the clock (`sleep(0)`) HANGS the run instead of failing it: run
hand mutants with an outer timeout.
