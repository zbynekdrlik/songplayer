/**
 * The A/V gate waits for cg OBS's genlock to LOCK its probe before it records
 * (#221, dev.19) — the pure decision, the endpoint read, and the bounded wait.
 *
 * Why: the gate records `SP-program` through cg OBS's probe input
 * (`av-sync-probe.ts`), a DistroAV receiver attached just before the take.
 * Its audio reaches cg OBS's MIX (what StartRecord records) only once
 * camera-box's genlock audio pairing (camera-box 1367) has locked: until
 * then the pairing WITHHOLDS the probe's packets from the mix, for up to
 * 10 s after its first packet. dev.17's take started before LOCKED and
 * opened with two dropouts (run 37423917199).
 *
 * dev.18 waits for the probe's `InputVolumeMeters` (`obs-audio-wait.ts`),
 * but that meter is tapped BEFORE the withhold: it proves DistroAV delivers
 * audio, never that the mix gets it. This wait reads the lock itself (the
 * main session's ROZHODNUTÉ, #221 comment 6012938501; camera-box's read
 * path, comment 6012957666) and runs BEFORE the meter wait: lock → meter
 * → StartRecord.
 *
 * The read: `GET :8899/bundle-state.json`, camera-box's health endpoint on
 * cg OBS's box (`scripts/bundle-state-server.py`). Its `genlock_lock`
 * object is parsed at request time from the NEWEST `genlock-lock-json:`
 * line in cg OBS's log (`scripts/bundle_state_genlock.py`):
 * - `state`: LOCKED | DEGRADED | UNLOCKED, `reason`: none | audio_pairing |
 *   recent_event | qpc_drift | …, as cg OBS's statusbar widget decided them;
 * - `inputs["A/V gate SP-program"]` = `{locked, connected, idle,
 *   latency_ms, underruns, relocks, late_holds, depth}`.
 * The widget writes the line on a box state or reason CHANGE, else every
 * ~30 s (`OBSBasicStatusBar.cpp`, `GENLOCK_JSON_HEARTBEAT_TICKS`). The facet
 * is omitted when cg OBS's log holds no such line.
 *
 * The condition, all four at once:
 * - the probe's `idle === false`. `idle` is true for a CONNECTED input with
 *   fewer than 60 frames over the last 60 s. An idled probe
 *   (`ndi_source_name` "") stops DistroAV's receiver thread, so it keeps
 *   `connected` and reads `idle: true` about a minute later; the probe is
 *   idle for minutes between runs. So a line with it `idle: false` was
 *   written AFTER the attach: this is what proves the heartbeat fresh;
 * - the probe's `locked === true`;
 * - `state === "LOCKED"` and `reason === "none"`. The box-level
 *   `audio_pairing` reason is the HTTP proxy for the probe's audio
 *   placement, which only cg OBS's log carries per input.
 * camera-box's observed attach (6.10.2026, local time): attach 08:49:33.1,
 * shallow latch 36.13, DEGRADED/audio_pairing 36.22 (the probe locked, not
 * idle), LOCKED/none 37.21.
 *
 * Polled every 250 ms, bounded at 15 s from the call. Each read is a FULL
 * camera-box gather (the log read, an obs-websocket read, the Windows
 * process facets): camera-box's own `:8899` watchdogs allow it 10 s and note
 * it has answered in ~6.6 s, so the real cadence is the gather time plus
 * 250 ms. The log is read first, so a read sees the state of its START: a
 * read that starts within the bound counts even when it answers after it,
 * and none starts after it. Worst case: the bound plus one read
 * (`LOCK_WAIT_WORST_MS`).
 *
 * Fails loud, never a skip (it is a dependency of the gate):
 * - the bound, with the last `genlock_lock` seen (state, reason, the probe's
 *   fields) and what it means ([`explainProbeLock`]);
 * - an endpoint that does not answer, answers non-200, non-JSON, or
 *   without the facet: at once.
 *
 * The endpoint (`CG_BUNDLE_STATE_URL`, a URL or a comma list) defaults to
 * loopback first: the gate runs ON cg OBS's box (win-resolume) and the
 * server binds 0.0.0.0, so a loopback refusal is instant, while the LAN name
 * `resolume.lan` (the name camera-box gave) can hang on DNS or a route.
 * [`resolveBundleState`] picks the first URL that answers with the facet,
 * once per run, before the gate switches any scene.
 *
 * The HTTP glue is the spec's (Playwright's `request`); these helpers are
 * unit-tested in the ubuntu mock suite (`probe-lock-wait.spec.ts`).
 */

/** The pause between two reads. */
export const LOCK_POLL_MS = 250;

/** The wait's bound: no read starts after it. */
export const LOCK_WAIT_TIMEOUT_MS = 15_000;

/** One read's own timeout, camera-box's `:8899` watchdogs' 10 s (their
 *  server has answered in ~6.6 s). */
export const LOCK_READ_TIMEOUT_MS = 10_000;

/** The wait's worst case: a read that starts at the bound and takes its own
 *  timeout. The A/V gate's time budget counts this. */
export const LOCK_WAIT_WORST_MS = LOCK_WAIT_TIMEOUT_MS + LOCK_READ_TIMEOUT_MS;

/** cg OBS's health endpoint, tried in this order (the file doc). */
export const DEFAULT_BUNDLE_STATE_URLS: readonly string[] = [
  "http://127.0.0.1:8899/bundle-state.json",
  "http://localhost:8899/bundle-state.json",
  "http://resolume.lan:8899/bundle-state.json",
];

/** The four conditions, in the order an attach reaches them. */
const CONDITION = 'its idle false, locked true, state "LOCKED", reason "none"';

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
 * else: a status other than 200, a body that is not a JSON object, or one
 * without the facet (camera-box omits it when cg OBS's log holds no
 * `genlock-lock-json:` line).
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
  /** All four conditions hold: record. */
  go: boolean;
  /** The conditions that do not hold, in the order an attach reaches them. */
  unmet: string[];
}

/** Whether cg OBS's genlock has LOCKED the probe (the file doc's four
 *  conditions, compared exactly). */
export function probeLockVerdict(lock: GenlockLock, probeInput: string): LockVerdict {
  const unmet: string[] = [];
  const entry = probeEntry(lock, probeInput);
  if (entry === null) {
    unmet.push(`input "${probeInput}" present`);
  } else {
    if (entry.idle !== false) unmet.push("probe idle === false");
    if (entry.locked !== true) unmet.push("probe locked === true");
  }
  if (lock.state !== "LOCKED") unmet.push('state === "LOCKED"');
  if (lock.reason !== "none") unmet.push('reason === "none"');
  return { go: unmet.length === 0, unmet };
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
 * has not reached (the probe listed, its heartbeat fresh, its FIFO locked,
 * then the box LOCKED for none).
 */
export function explainProbeLock(lock: GenlockLock, probeInput: string): string {
  const entry = probeEntry(lock, probeInput);
  if (entry === null) {
    return (
      `camera-box's genlock lists no input "${probeInput}": the probe is not a genlock ` +
      "input on cg OBS (is its genlock_fifo on?), or it is named differently."
    );
  }
  if (entry.idle !== false) {
    return (
      `The newest heartbeat does not show the probe receiving (idle ${fmt(entry.idle)}), so ` +
      "it was written before the attach: camera-box writes one on a box state or reason " +
      "change, else every ~30 s."
    );
  }
  if (entry.locked !== true) {
    return `The probe receives, but its genlock FIFO is not locked (locked ${fmt(entry.locked)}).`;
  }
  if (lock.reason === "audio_pairing") {
    return (
      `cg OBS's genlock is ${fmt(lock.state)} for audio_pairing: a genlock input's audio is ` +
      "still more than a frame from its video (the box-level proxy for the probe's audio " +
      "withheld from the mix)."
    );
  }
  return (
    `cg OBS's genlock is ${fmt(lock.state)} (reason ${fmt(lock.reason)}), not "LOCKED" ` +
    '(reason "none").'
  );
}

/** A read of one URL (`url: <failure>` on failure). */
export function bundleStateRead(url: string, get: HttpGet): LockRead {
  return async (timeoutMs) => {
    try {
      const r = await get(url, timeoutMs);
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

/** What the wait saw; returned when the probe is locked. */
export interface LockWaitReport {
  /** From the call to the answer that made it a GO. */
  waitedMs: number;
  /** Reads that answered. */
  reads: number;
  /** The longest of those reads. */
  slowestReadMs: number;
  /** Each change of the summary, `+<ms from the call at the read's start>
   *  ms <state>/<reason>, probe locked=… idle=…`. */
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
}

/** The trail is capped; a stuck facet repeats one line, which is kept once. */
const MAX_SEEN = 20;

/**
 * Read cg OBS's genlock state every `pollMs` until [`probeLockVerdict`]
 * says GO. Rejects, naming the probe, the condition and what it saw:
 * - when a read fails (the endpoint stopped answering): at once;
 * - when a read that did not GO ends after `timeoutMs`, or the next one
 *   would start after it ([`describeGenlockLock`], [`explainProbeLock`]).
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
  const report: LockWaitReport = { waitedMs: 0, reads: 0, slowestReadMs: 0, seen: [] };
  let lastSummary: string | null = null;
  for (;;) {
    const readAt = now();
    let lock: GenlockLock;
    try {
      lock = await read(readTimeoutMs);
    } catch (e) {
      report.waitedMs = now() - start;
      throw new Error(
        `could not read cg OBS's genlock state while waiting for the probe "${probeInput}" ` +
          `to lock (${CONDITION}), after ${Math.round(report.waitedMs)} ms and ` +
          `${report.reads} reads: ${errorText(e)} (#221)`,
      );
    }
    const answeredAt = now();
    report.reads++;
    report.slowestReadMs = Math.max(report.slowestReadMs, answeredAt - readAt);
    report.waitedMs = answeredAt - start;
    const summary = summarizeLock(lock, probeInput);
    if (summary !== lastSummary && report.seen.length < MAX_SEEN) {
      report.seen.push(`+${Math.round(readAt - start)} ms ${summary}`);
    }
    lastSummary = summary;
    if (probeLockVerdict(lock, probeInput).go) return report;
    // No read starts after the bound.
    if (answeredAt - start >= timeoutMs) throw boundError(lock);
    await sleep(pollMs);
    if (now() - start > timeoutMs) throw boundError(lock);
  }

  function boundError(last: GenlockLock): Error {
    return new Error(
      `cg OBS's genlock did not lock the probe "${probeInput}" within ${timeoutMs} ms ` +
        `(${CONDITION}): ${report.reads} reads, the slowest ${Math.round(report.slowestReadMs)} ms; ` +
        `last genlock_lock: ${describeGenlockLock(last, probeInput)}. ` +
        `${explainProbeLock(last, probeInput)} Seen: ${report.seen.join(" → ")} (#221)`,
    );
  }
}
