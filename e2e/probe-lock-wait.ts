/**
 * The A/V gate waits on cg OBS's genlock state before it records (#221,
 * dev.19) — the pure decision, the endpoint read, and the bounded wait. The
 * camera-box internals behind every field, and why each choice was made, are
 * in `.claude/rules/av-gate-lock-wait.md`; this doc keeps what the code does.
 *
 * Why: the gate records `SP-program` through cg OBS's probe input
 * (`av-sync-probe.ts`), a DistroAV receiver attached just before the take.
 * camera-box's genlock audio pairing WITHHOLDS the probe's audio from cg
 * OBS's mix until it places it (up to 10 s after its first packet); dev.17's
 * take started before that and opened with two dropouts (run 37423917199).
 * The dev.18 meter wait (`obs-audio-wait.ts`) is tapped BEFORE the withhold.
 * This wait (the main session's ROZHODNUTÉ, #221 comment 6012938501, on
 * camera-box's read path, comment 6012957666) runs before the meter: lock →
 * meter → StartRecord ([`probeReadyForTake`]).
 *
 * The read: `GET :8899/bundle-state.json`, camera-box's health server on cg
 * OBS's box; its `genlock_lock` object comes from the NEWEST
 * `genlock-lock-json:` line in cg OBS's log, written on a box state / reason
 * / media-clock change, else every ~30 s. [`parseGenlockLock`] refuses a
 * response without the object, or a pre-v6 line (no `n_idle`).
 *
 * The condition ([`probeLockVerdict`], all at once, compared exactly): the
 * probe's `connected === true` (added in review round 1: an absent input is
 * never idle and keeps its FIFO lock), `idle === false` (the line is from
 * after the attach, given a probe idle for minutes before it), `locked ===
 * true`, and the box `state === "LOCKED"` for `reason === "none"`. Before
 * the attach, [`probeAttachRefusal`] refuses a probe line that could already
 * read as a GO (a run cancelled mid-take).
 *
 * **It is NOT an exact view of the withhold** (the open design question on
 * #221, comments 6014055098, 6014658984 and 6016284903). The wait normally
 * says GO on the LOCKED/none change line that ends the attach's
 * DEGRADED/`audio_pairing` slew, after the audio is placed. Residuals:
 * 1. libobs reads the PENDING withhold as paired (offset 0) and does not
 *    clear the FIFO lock when the probe is idled or starves, so a HEARTBEAT
 *    written between the probe reading `idle: false` (it can from the bind
 *    on: a reattach can reset its idle sample ring) and the placement can
 *    read as a GO during the withhold;
 * 2. an attach with no `audio_pairing` phase writes no change line, so the
 *    first fresh line is the next heartbeat (≤ 30 s): the 15 s bound can fail
 *    on a healthy probe;
 * 3. an idle or absent input's phase events count 0, so a woken probe's
 *    LIFETIME relocks / late holds / backward steps come back as new events
 *    and hold DEGRADED/`recent_event` for 60 s ([`probePhaseEvents`] reads
 *    the count before the attach; [`waitForProbeLock`] passes it to
 *    [`explainProbeLock`] only while the last ANSWERED read started within
 *    `WAKE_LATCH_WINDOW_MS` of `attachedAt`, the bind SongPlayer saw; never
 *    from `recent_event_inputs`, which names camera-box's top lifetime
 *    offender). The latch is box-wide: ANY rise of cg OBS's phase-event sum
 *    holds it — any genlock input that wakes or reconnects with lifetime
 *    events (e.g. after a SongPlayer restart, if cg OBS has another genlock
 *    input on `SP-program`, SongPlayer's only NDI sender), or a real
 *    relock, late hold or backward step on a live input (a backward step is
 *    not in the facet: invisible in both lists). The inputs of the line
 *    before the attach (the spec logs them, [`summarizeInputs`]) and of the
 *    line at the bound (the bound's error lists them, with how long after
 *    the bind SongPlayer saw its last answered read started) are
 *    candidates, never proof: they carry no times, and the
 *    input with the most lifetime events is not evidence;
 * 4. the refusal trusts the newest line before the attach: a probe whose FIFO
 *    locked after that line (while still unlocked in it) is not refused.
 *
 * Polled every 250 ms, bounded at 15 s from the call. Each read is a FULL
 * camera-box gather (seconds), so a read gets 10 s, sees the state at its
 * START, counts when it started within the bound, and none starts after it:
 * the worst case is the bound plus one read (`LOCK_WAIT_WORST_MS`). A failed
 * read is counted and retried while the bound allows. Fails loud, never a
 * skip: at the bound with the last facet, the failed reads, the trail of
 * changes and what the first unmet condition means.
 *
 * The endpoint (`CG_BUNDLE_STATE_URL`, a URL or a comma list) defaults to
 * loopback first (the gate runs ON cg OBS's box and the server binds
 * 0.0.0.0), then `resolume.lan`. [`resolveBundleState`] picks the first URL
 * that answers with the facet, once per run, before the gate switches any
 * scene: none answering fails the gate there.
 *
 * The HTTP glue is the spec's (Playwright's `request`); these helpers are
 * unit-tested in the ubuntu mock suite (`probe-lock-wait.spec.ts`).
 */

/** The pause between two reads. */
export const LOCK_POLL_MS = 250;

/** The wait's bound: no read starts after it. */
export const LOCK_WAIT_TIMEOUT_MS = 15_000;

/** One read's own timeout: camera-box's own `:8899` watchdogs allow 10 s
 *  (their server has answered in ~6.6 s). A read over it fails and is
 *  retried while the bound allows. */
export const LOCK_READ_TIMEOUT_MS = 10_000;

/** The wait's worst case: a read that starts at the bound and takes its own
 *  timeout. The A/V gate's time budget counts this (`av-sync-budget.ts`). */
export const LOCK_WAIT_WORST_MS = LOCK_WAIT_TIMEOUT_MS + LOCK_READ_TIMEOUT_MS;

/** How long after `attachedAt` a `recent_event` can still be the woken
 *  probe's: camera-box holds the latch 60 s after the wake; the probe wakes
 *  within ~3.5 s of its bind (60 frames in ~2.4 s plus one 1 Hz widget
 *  tick, or at once when the reconnect reset its idle sample ring); and the
 *  spec takes `attachedAt` ~1–2 s AFTER the bind (SongPlayer's ~1 s
 *  receiver-count sample plus the 500 ms receiver poll). The 10 s over the
 *  latch is a deliberate margin: the explanation may name the wake a little
 *  late, never miss it. */
export const WAKE_LATCH_WINDOW_MS = 70_000;

/** cg OBS's health endpoint, tried in this order (the file doc). */
export const DEFAULT_BUNDLE_STATE_URLS: readonly string[] = [
  "http://127.0.0.1:8899/bundle-state.json",
  "http://localhost:8899/bundle-state.json",
  "http://resolume.lan:8899/bundle-state.json",
];

/** The conditions, as the error texts name them. */
const CONDITION =
  'its idle false, locked true, state "LOCKED", reason "none", with its NDI connection live';

/** camera-box's `genlock_lock` object, as served (values unchecked). */
export type GenlockLock = Record<string, unknown>;

/** One HTTP GET: the status and the body text. Throws when nothing answers
 *  within `timeoutMs`. */
export type HttpGet = (url: string, timeoutMs: number) => Promise<{ status: number; body: string }>;

/** One read of `genlock_lock` within `timeoutMs`; throws on failure. */
export type LockRead = (timeoutMs: number) => Promise<GenlockLock>;

function isObject(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** A value as JSON (strings quoted), for the error texts. */
function fmt(v: unknown): string {
  return JSON.stringify(v) ?? "undefined";
}

/** A value bare when it is a string, for the trail. */
function word(v: unknown): string {
  return typeof v === "string" ? v : fmt(v);
}

/** The endpoint URLs: `CG_BUNDLE_STATE_URL` (one URL or a comma list), else
 *  [`DEFAULT_BUNDLE_STATE_URLS`]. */
export function bundleStateUrls(env: string | undefined): string[] {
  const listed = (env ?? "")
    .split(",")
    .map((u) => u.trim())
    .filter((u) => u !== "");
  return listed.length > 0 ? listed : [...DEFAULT_BUNDLE_STATE_URLS];
}

/**
 * The `genlock_lock` object of a bundle-state response. Throws on anything
 * else: a status other than 200, a body that is not a JSON object, one
 * without the facet (camera-box omits it when cg OBS's log holds no
 * `genlock-lock-json:` line), or a line from before camera-box's schema v6
 * (no `n_idle`: every `idle` then defaults to false, so no line could be
 * proven fresh).
 */
export function parseGenlockLock(status: number, body: string): GenlockLock {
  if (status !== 200) throw new Error(`HTTP ${status}`);
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    throw new Error(`not JSON: ${fmt(body.slice(0, 80))}`);
  }
  if (!isObject(parsed)) throw new Error("not a JSON object");
  const facet = parsed.genlock_lock;
  if (!isObject(facet)) {
    throw new Error(
      "no genlock_lock object (camera-box omits it when cg OBS's log holds no " +
        "genlock-lock-json: line: a stock OBS, or no genlock line in the read tail)",
    );
  }
  if (typeof facet.n_idle !== "number") {
    throw new Error(
      `a genlock line from before camera-box's schema v6 (n_idle ${fmt(facet.n_idle)}): ` +
        "it reports no per-input idle, so no line can be proven to come from after the attach",
    );
  }
  return facet;
}

/** The probe's own entry in `inputs` (an own key holding an object), or null. */
function probeEntry(lock: GenlockLock, probeInput: string): Record<string, unknown> | null {
  const inputs = lock.inputs;
  if (!isObject(inputs) || !Object.prototype.hasOwnProperty.call(inputs, probeInput)) return null;
  const entry = inputs[probeInput];
  return isObject(entry) ? entry : null;
}

export interface LockVerdict {
  /** Every condition holds: record. */
  go: boolean;
  /** The conditions that do not hold, in the order an attach reaches them. */
  unmet: string[];
}

/** Whether cg OBS's genlock has LOCKED the probe (the file doc's
 *  conditions, compared exactly). */
export function probeLockVerdict(lock: GenlockLock, probeInput: string): LockVerdict {
  const unmet: string[] = [];
  const entry = probeEntry(lock, probeInput);
  if (entry === null) {
    unmet.push(`input "${probeInput}" present`);
  } else {
    if (entry.connected !== true) unmet.push("probe connected === true");
    if (entry.idle !== false) unmet.push("probe idle === false");
    if (entry.locked !== true) unmet.push("probe locked === true");
  }
  if (lock.state !== "LOCKED") unmet.push('state === "LOCKED"');
  if (lock.reason !== "none") unmet.push('reason === "none"');
  return { go: unmet.length === 0, unmet };
}

/**
 * Why the gate must not attach the probe on this facet, or null when it may.
 * Read BEFORE the attach: a probe line that could already read as a GO —
 * connected, not `idle` (it received within the last minute: a run
 * cancelled mid-take) and `locked` (an idled probe keeps its FIFO lock) —
 * means no later line could be proven to come from after this run's
 * attach. Not refused: a probe camera-box does not list (a fresh box), an
 * absent one (never idle, so its idle proves nothing; the wait needs a
 * connected line), an unlocked one (a never-attached probe right after cg
 * OBS starts reads connected and not yet classified idle). The last is a
 * known residual: a probe whose FIFO locked AFTER the newest line (and was
 * idled since) keeps `locked: true`, so a heartbeat before this run's attach
 * could read as a GO. It needs that line to show the probe receiving but
 * unlocked: before its first attach locks after a cg OBS start, or right
 * after a FIFO lock clear (a backward-step regime end, a latency-pin rise).
 */
export function probeAttachRefusal(lock: GenlockLock, probeInput: string): string | null {
  const entry = probeEntry(lock, probeInput);
  if (entry === null || entry.connected !== true || entry.idle !== false || entry.locked !== true) {
    return null;
  }
  return (
    `cg OBS's newest genlock line shows the probe "${probeInput}" receiving (idle false) and ` +
    "locked before this run attached it: it received within the last minute (a run " +
    "cancelled mid-take?), so the lock wait could not prove a line to come from after the " +
    "attach: re-run once it has received nothing for ~90 s (camera-box's 60 s idle window " +
    "plus a heartbeat of up to 30 s) (#221)"
  );
}

/**
 * The probe's lifetime phase events camera-box can see (`relocks +
 * late_holds`; `backward_steps` is not in the facet), or null without a
 * probe entry or with a field that is not a count. Read before the attach:
 * non-zero predicts the woken probe's DEGRADED/`recent_event` (the file
 * doc's third residual).
 */
export function probePhaseEvents(lock: GenlockLock, probeInput: string): number | null {
  const entry = probeEntry(lock, probeInput);
  if (entry === null) return null;
  const { relocks, late_holds: lateHolds } = entry;
  if (typeof relocks !== "number" || typeof lateHolds !== "number") return null;
  return relocks + lateHolds;
}

/**
 * Every genlock input of the facet in one line: its name, `connected`,
 * `idle`, `locked` and lifetime `relocks + late_holds` (`?` when not
 * counts); "none" without inputs. The spec logs it for the line before the
 * attach, and the bound's error for the last answered line: candidates for
 * the box-wide `recent_event` latch (an input that woke or reconnected, or a
 * real relock, late hold or backward step — the last invisible in both),
 * never proof — the lists carry no times, and the
 * input with the most lifetime events is not evidence (camera-box's own
 * top-offender trap).
 */
export function summarizeInputs(lock: GenlockLock): string {
  const inputs = lock.inputs;
  if (!isObject(inputs)) return "none";
  const rows = Object.entries(inputs).map(([name, entry]) => {
    if (!isObject(entry)) return `${fmt(name)} ?`;
    const { relocks, late_holds: lateHolds } = entry;
    const events =
      typeof relocks === "number" && typeof lateHolds === "number" ? relocks + lateHolds : "?";
    return (
      `${fmt(name)} connected=${word(entry.connected)} idle=${word(entry.idle)} ` +
      `locked=${word(entry.locked)} events=${events}`
    );
  });
  return rows.length > 0 ? rows.join("; ") : "none";
}

/** The facet in words: the box verdict, the probe's entry (or the inputs
 *  there are), and any named offender. */
export function describeGenlockLock(lock: GenlockLock, probeInput: string): string {
  const parts = [`state ${fmt(lock.state)}`, `reason ${fmt(lock.reason)}`];
  const entry = probeEntry(lock, probeInput);
  if (entry !== null) {
    parts.push(`input "${probeInput}" ${fmt(entry)}`);
  } else {
    const names = isObject(lock.inputs) ? Object.keys(lock.inputs) : [];
    const listed = names.length > 0 ? names.map((n) => fmt(n)).join(", ") : "none";
    parts.push(`no input "${probeInput}" (inputs: ${listed})`);
  }
  for (const key of ["recent_event_inputs", "audio_unexpected_inputs"]) {
    if (lock[key] !== undefined) parts.push(`${key} ${fmt(lock[key])}`);
  }
  return parts.join(", ");
}

/** One line of the trail: the box verdict and the probe's lock and idle. */
function summarizeLock(lock: GenlockLock, probeInput: string): string {
  const entry = probeEntry(lock, probeInput);
  const probe =
    entry === null ? "no probe" : `probe locked=${word(entry.locked)} idle=${word(entry.idle)}`;
  return `${word(lock.state)}/${word(lock.reason)}, ${probe}`;
}

/**
 * What a facet that is not yet a GO means: the FIRST condition the attach
 * has not reached (the probe listed, connected, its heartbeat fresh, its
 * FIFO locked, then the box LOCKED for none). `phaseEventsBeforeAttach` is
 * [`probePhaseEvents`] of the line read before the attach: with reason
 * `recent_event` and a count above 0, the cause is the woken probe. It is
 * never read from `recent_event_inputs`, which names camera-box's TOP
 * LIFETIME offender, not the input whose count rose.
 */
export function explainProbeLock(
  lock: GenlockLock,
  probeInput: string,
  phaseEventsBeforeAttach: number | null = null,
): string {
  const entry = probeEntry(lock, probeInput);
  if (entry === null) {
    return (
      `camera-box's genlock lists no input "${probeInput}": the probe is not a genlock ` +
      "input on cg OBS (is its genlock_fifo on?), or it is named differently."
    );
  }
  if (entry.connected !== true) {
    return (
      `The newest line shows the probe without a live NDI connection (connected ` +
      `${fmt(entry.connected)}): written before the attach, or SP-program's sender is gone.`
    );
  }
  if (entry.idle !== false) {
    return (
      `The newest heartbeat does not show the probe receiving (idle ${fmt(entry.idle)}), so ` +
      "it was written before the attach: camera-box writes one on a box state or reason " +
      "change, else every ~30 s, and an attach with no audio_pairing phase changes neither."
    );
  }
  if (entry.locked !== true) {
    return `The probe receives, but its genlock FIFO is not locked (locked ${fmt(entry.locked)}).`;
  }
  if (
    lock.reason === "recent_event" &&
    phaseEventsBeforeAttach !== null &&
    phaseEventsBeforeAttach > 0
  ) {
    return (
      `cg OBS's genlock is ${fmt(lock.state)} for recent_event, and the probe had ` +
      `${phaseEventsBeforeAttach} lifetime phase events before the attach (relocks + late ` +
      "holds): camera-box counts an idle input's phase events as 0, so the woken probe's " +
      "lifetime totals came back as new events, which hold recent_event for 60 s (a " +
      "camera-box wake re-baseline issue, #221 comment 6014658984; recent_event_inputs " +
      "names camera-box's top lifetime offender, not necessarily the probe)."
    );
  }
  if (lock.reason === "audio_pairing") {
    return (
      `cg OBS's genlock is ${fmt(lock.state)} for audio_pairing: a genlock input's placed ` +
      "audio is still more than one frame (33 ms) from its video, the slew after the pairing " +
      "places it."
    );
  }
  const boxWide =
    lock.reason === "recent_event"
      ? " recent_event is box-wide: ANY rise of cg OBS's phase-event sum in the last 60 s " +
        "latches it — a genlock input that woke or reconnected with lifetime events (events=0 " +
        "in a list does not rule one out: backward steps are not shown), or a real relock, " +
        "late hold or backward step (not in the facet: invisible in both lists) on a live " +
        "input. The inputs the gate logged ahead of the attach and the " +
        "inputs at the bound are candidates only (they carry no times, and the first list is " +
        "older than the attach); the input with the most lifetime events is not evidence."
      : "";
  return (
    `cg OBS's genlock is ${fmt(lock.state)} (reason ${fmt(lock.reason)}), not "LOCKED" ` +
    `(reason "none").${boxWide}`
  );
}

/** A read of one URL (`url: <failure>` on failure). The timeout is whole
 *  milliseconds, at least 1: Playwright reads 0 as "no timeout". */
export function bundleStateRead(url: string, get: HttpGet): LockRead {
  return async (timeoutMs) => {
    try {
      const r = await get(url, Math.max(1, Math.ceil(timeoutMs)));
      return parseGenlockLock(r.status, r.body);
    } catch (e) {
      throw new Error(`${url}: ${errorText(e)}`);
    }
  };
}

/**
 * The first of `urls` that answers with the facet, read once each in
 * order, and what it answered. Throws naming every URL and its failure.
 */
export async function resolveBundleState(
  urls: readonly string[],
  get: HttpGet,
  timeoutMs = LOCK_READ_TIMEOUT_MS,
): Promise<{ url: string; lock: GenlockLock }> {
  const failures: string[] = [];
  for (const url of urls) {
    try {
      return { url, lock: await bundleStateRead(url, get)(timeoutMs) };
    } catch (e) {
      failures.push(errorText(e));
    }
  }
  throw new Error(
    `cg OBS's genlock state is unreadable: no health-endpoint URL answered with its ` +
      `genlock_lock (${failures.join("; ")}). The A/V gate needs camera-box's :8899 ` +
      `bundle-state server on cg OBS's box; CG_BUNDLE_STATE_URL overrides the URLs. (#221)`,
  );
}

/**
 * Before every take (#221): the lock wait FIRST, then the meter wait, only
 * once the lock wait has resolved. A failed lock wait never starts the
 * meter wait.
 */
export async function probeReadyForTake<L, A>(
  lockWait: () => Promise<L>,
  audioWait: () => Promise<A>,
): Promise<{ lock: L; audio: A }> {
  const lock = await lockWait();
  const audio = await audioWait();
  return { lock, audio };
}

/** What the wait saw; returned when the probe is locked. */
export interface LockWaitReport {
  /** From the call to the answer that made it a GO. */
  waitedMs: number;
  /** Reads that answered with the facet. */
  reads: number;
  /** Reads that failed (no answer in time, refused, non-200, …). */
  failedReads: number;
  /** The longest read, answered or failed. */
  slowestReadMs: number;
  /** Each change of the summary, `+<ms from the call at the read's start>
   *  ms <state>/<reason>, probe locked=… idle=…`, or `… read failed: …`. */
  seen: string[];
}

export interface LockWaitOptions {
  /** Default [`LOCK_POLL_MS`]. */
  pollMs?: number;
  /** Default [`LOCK_WAIT_TIMEOUT_MS`]. */
  timeoutMs?: number;
  /** Default [`LOCK_READ_TIMEOUT_MS`]. */
  readTimeoutMs?: number;
  /** The monotonic clock (ms); default `performance.now`. */
  now?: () => number;
  /** The pause; default a `setTimeout`. */
  sleep?: (ms: number) => Promise<void>;
  /** [`probePhaseEvents`] of the line read before the attach, for the
   *  bound's explanation ([`explainProbeLock`]); default unknown. */
  phaseEventsBeforeAttach?: number | null;
  /** When the probe's receiver attached (the bind SongPlayer saw), on
   *  `now`'s clock: the wake is named only while the last answered read
   *  started within [`WAKE_LATCH_WINDOW_MS`] of it. Default unknown (no
   *  window check). */
  attachedAt?: number;
}

/** The trail is capped; a stuck facet repeats one line, which is kept once. */
const MAX_SEEN = 20;

/**
 * Read cg OBS's genlock state every `pollMs` until [`probeLockVerdict`]
 * says GO. A read that fails is counted and retried. Rejects when a read
 * that was not a GO ends after `timeoutMs`, or the next one would start
 * after it, naming the probe, the condition and what it saw
 * ([`describeGenlockLock`], [`explainProbeLock`], the failed reads).
 */
export async function waitForProbeLock(
  read: LockRead,
  probeInput: string,
  opts: LockWaitOptions = {},
): Promise<LockWaitReport> {
  const now = opts.now ?? (() => performance.now());
  const sleep = opts.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  const pollMs = opts.pollMs ?? LOCK_POLL_MS;
  const timeoutMs = opts.timeoutMs ?? LOCK_WAIT_TIMEOUT_MS;
  const readTimeoutMs = opts.readTimeoutMs ?? LOCK_READ_TIMEOUT_MS;
  const start = now();
  const report: LockWaitReport = {
    waitedMs: 0,
    reads: 0,
    failedReads: 0,
    slowestReadMs: 0,
    seen: [],
  };
  let last: GenlockLock | null = null;
  let lastAt = start;
  let lastFailure: string | null = null;
  let lastSummary: string | null = null;
  for (;;) {
    const readAt = now();
    let lock: GenlockLock | null = null;
    let summary: string;
    try {
      lock = await read(readTimeoutMs);
      summary = summarizeLock(lock, probeInput);
    } catch (e) {
      lastFailure = errorText(e);
      summary = `read failed: ${lastFailure}`;
    }
    const answeredAt = now();
    report.slowestReadMs = Math.max(report.slowestReadMs, answeredAt - readAt);
    report.waitedMs = answeredAt - start;
    if (summary !== lastSummary && report.seen.length < MAX_SEEN) {
      report.seen.push(`+${Math.round(readAt - start)} ms ${summary}`);
    }
    lastSummary = summary;
    if (lock === null) {
      report.failedReads++;
    } else {
      report.reads++;
      last = lock;
      lastAt = readAt;
      if (probeLockVerdict(lock, probeInput).go) return report;
    }
    // No read starts after the bound.
    if (answeredAt - start >= timeoutMs) throw boundError();
    await sleep(pollMs);
    if (now() - start > timeoutMs) throw boundError();
  }

  /** The count before the attach, while the wake's latch can still hold. */
  function wakeCount(): number | null {
    const withinLatch =
      opts.attachedAt === undefined || lastAt - opts.attachedAt < WAKE_LATCH_WINDOW_MS;
    return withinLatch ? (opts.phaseEventsBeforeAttach ?? null) : null;
  }

  function boundError(): Error {
    const failed =
      report.failedReads > 0 ? `, ${report.failedReads} failed (the last: ${lastFailure})` : "";
    const sinceBind =
      opts.attachedAt === undefined
        ? ""
        : ` (the last answered read started ${Math.round(lastAt - opts.attachedAt)} ms after the ` +
          "bind SongPlayer saw)";
    const lastText =
      last === null
        ? "none"
        : `${describeGenlockLock(last, probeInput)}; inputs at the bound${sinceBind}: ` +
          summarizeInputs(last);
    const why =
      last === null
        ? "No read of cg OBS's genlock state answered: is camera-box's :8899 server up?"
        : explainProbeLock(last, probeInput, wakeCount());
    return new Error(
      `cg OBS's genlock did not lock the probe "${probeInput}" within ${timeoutMs} ms ` +
        `(${CONDITION}): ${report.reads} reads answered${failed}, the slowest ` +
        `${Math.round(report.slowestReadMs)} ms; last genlock_lock: ${lastText}. ${why} ` +
        `Seen: ${report.seen.join(" → ")} (#221)`,
    );
  }
}
