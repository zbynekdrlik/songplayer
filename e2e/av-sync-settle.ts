/**
 * The A/V gate's probe settle (#268) — the pure decisions.
 *
 * The gate points cg OBS's probe input (`av-sync-probe.ts`) at `SP-program`
 * right before its take, so the probe's receiver COLD-STARTS every run. In
 * camera-box's genlock build the shallow lock then latches the receive depth
 * (~3 s), and the probe's audio hold follows the realized video delay,
 * SLEWED through a resampler at 1000 ppm (camera-box 1367, never a step):
 * one frame of depth (33 ms) takes 33 s. Both failing takes on #268 were that
 * slew from their first second to their last (-999.67 / +999.59 ppm, fit
 * residual < 0.1 ms; finding 6148747323). The audio meter wait
 * (`obs-audio-wait.ts`) proves only that audio flows.
 *
 * So, per ROZHODNUTÉ 6148359080 (never a permanently named probe: it would
 * mask SP-program's real consumers):
 *
 * 1. **Lock precondition.** camera-box republishes its genlock lock verdict
 *    on `:8899/bundle-state.json`, `genlock_lock.inputs[<input name>]`
 *    (issuecomment-6148382512: `locked`, `connected`, `idle`, `latency_ms`,
 *    `underruns`, `relocks`, `late_holds`, `depth`). The facet is parsed from
 *    the newest `genlock-lock-json:` OBS log line, written every 30 s, and
 *    carries no "since". `idle: true` means the input is off program. The
 *    probe is off program until the gate switches cg OBS to the probe scene,
 *    so only a line with `idle: false` was written after that switch (after
 *    the probe was pointed): the gate waits for such a line reading
 *    `locked` and `connected`, bounded at two lines.
 * 2. **Analyse only the settled part.** The analysed part starts
 *    [`PROBE_SETTLE_MS`] after the probe's receiver attached (camera-box: a
 *    cold start settles in >= ~45 s). The take records that head plus its
 *    20 s, and `av_sync_check.py --analysis-start-s` cuts the head from
 *    every verdict input.
 *
 * The glue (the fetch, the polls, the recording) is in
 * `post-deploy-av-sync.spec.ts`; these helpers are unit-tested in the
 * ubuntu mock suite (`av-sync-settle.spec.ts`).
 */

/** camera-box's bundle-state server, on the box the gate's runner runs on. */
export const CAMERA_BOX_STATE_URL = "http://127.0.0.1:8899/bundle-state.json";

/** How often camera-box writes the `genlock-lock-json:` line the facet reads. */
export const FACET_LINE_MS = 30_000;

/**
 * The lock precondition's bound: two facet lines. A line written just before
 * the lock latched (~3 s after the receiver start) is followed by one that
 * reads it, so a healthy probe reads locked within ~33 s of the switch.
 */
export const PROBE_LOCK_TIMEOUT_MS = 2 * FACET_LINE_MS;

/** How often the gate reads the facet while it waits. */
export const PROBE_LOCK_POLL_MS = 2_000;

/**
 * From the probe receiver's attach to the analysed part's start
 * (camera-box 6148340563: the latch ~3 s, then one frame of depth slewed at
 * 1000 ppm, 33 s; more if the depth moves twice). Run 38402810942 attempt 2
 * measured the slew still running 24 s after the receiver start.
 */
export const PROBE_SETTLE_MS = 45_000;

/**
 * The most the cold probe adds to the first take over a warm one: the lock
 * wait (at most its bound) plus the recorded settle head end within
 * `max(PROBE_LOCK_TIMEOUT_MS, PROBE_SETTLE_MS)` of the attach.
 */
export const COLD_PROBE_MS = Math.max(PROBE_LOCK_TIMEOUT_MS, PROBE_SETTLE_MS);

/** The probe's entry in the facet, as camera-box serves it. */
export type ProbeLockEntry = Record<string, unknown>;

/** What one read of the facet says about the probe. */
export type ProbeLock =
  /** The document could not be fetched or parsed (the glue's own state). */
  | { kind: "unreadable"; error: string }
  /** No `genlock_lock.inputs` object in the document. */
  | { kind: "no_facet" }
  /** The facet does not list the probe (or its entry is not an object). */
  | { kind: "no_input"; inputs: string[] }
  /** A line from before the switch: the probe off program (`idle` not false). */
  | { kind: "stale"; entry: ProbeLockEntry }
  | { kind: "not_connected"; entry: ProbeLockEntry }
  | { kind: "not_locked"; entry: ProbeLockEntry }
  /** On program, connected and locked: the precondition holds. */
  | { kind: "locked"; entry: ProbeLockEntry };

const isObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

/**
 * The probe's lock state from one `bundle-state.json` document. Only real
 * booleans count: `idle === false`, then `connected === true`, then
 * `locked === true`.
 */
export function probeLock(doc: unknown, input: string): ProbeLock {
  const facet = isObject(doc) ? doc.genlock_lock : undefined;
  const inputs = isObject(facet) ? facet.inputs : undefined;
  if (!isObject(inputs)) return { kind: "no_facet" };
  const entry = inputs[input];
  if (!isObject(entry)) return { kind: "no_input", inputs: Object.keys(inputs) };
  if (entry.idle !== false) return { kind: "stale", entry };
  if (entry.connected !== true) return { kind: "not_connected", entry };
  if (entry.locked !== true) return { kind: "not_locked", entry };
  return { kind: "locked", entry };
}

const LISTED_INPUTS = 10;

/** One line naming what the read saw: for the gate's log and its failure. */
export function describeProbeLock(lock: ProbeLock): string {
  switch (lock.kind) {
    case "unreadable":
      return `camera-box's bundle-state is not readable (${lock.error})`;
    case "no_facet":
      return "camera-box's bundle-state carries no genlock_lock.inputs facet";
    case "no_input": {
      const shown = lock.inputs.slice(0, LISTED_INPUTS).map((n) => JSON.stringify(n));
      const more = lock.inputs.length - shown.length;
      return (
        `the genlock_lock facet does not list the probe (it lists ${shown.join(", ") || "nothing"}` +
        `${more > 0 ? ` and ${more} more` : ""})`
      );
    }
    case "stale":
      return (
        "the facet's newest line reads the probe off program (idle is not false): it was written " +
        `before cg OBS switched to the probe scene, and it is rewritten every ${FACET_LINE_MS / 1000} s ` +
        JSON.stringify(lock.entry)
      );
    case "not_connected":
      return `the probe is on program but not connected ${JSON.stringify(lock.entry)}`;
    case "not_locked":
      return `the probe is connected but not locked ${JSON.stringify(lock.entry)}`;
    case "locked":
      return `the probe is locked and connected, on program ${JSON.stringify(lock.entry)}`;
  }
}

/**
 * Where a take's analysed part starts, in ms of its recording: the settle
 * left after the probe's receiver attached (`attachedAtMs`) when the
 * recording was asked to start (`startCalledAtMs`, read BEFORE the
 * StartRecord call: the file's time 0 is no earlier, so the analysed part
 * never starts before the settle). 0 once the settle is over (a retake).
 */
export function analysisStartMs(
  attachedAtMs: number,
  startCalledAtMs: number,
  settleMs: number = PROBE_SETTLE_MS,
): number {
  return Math.max(0, attachedAtMs + settleMs - startCalledAtMs);
}
