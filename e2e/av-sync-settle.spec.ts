/**
 * Unit tests for the A/V gate's probe settle (#268): the lock precondition
 * read from camera-box's `genlock_lock` facet, and where a take's analysed
 * part starts. Runs in the ubuntu mock suite (playwright.config.ts); these
 * `test()` blocks never touch `page`, so they need no browser and no box.
 */

import { test, expect } from "@playwright/test";
import { AV_PROBE_INPUT } from "./av-sync-probe";
import {
  CAMERA_BOX_STATE_URL,
  COLD_PROBE_MS,
  FACET_LINE_MS,
  PROBE_LOCK_TIMEOUT_MS,
  PROBE_SETTLE_MS,
  analysisStartMs,
  describeProbeLock,
  probeLock,
  type ProbeLock,
} from "./av-sync-settle";

/** camera-box's live value of the probe (issuecomment-6148382512), off program. */
const LIVE_IDLE = {
  locked: true,
  connected: true,
  idle: true,
  latency_ms: 3,
  underruns: 1057101,
  relocks: 0,
  late_holds: 0,
  depth: 0,
};

const doc = (entry: unknown) => ({
  genlock_lock: { state: "locked", inputs: { [AV_PROBE_INPUT]: entry, "cam 1": { locked: true } } },
});

test.describe("A/V gate probe settle (#268)", () => {
  test("the facet is read where camera-box serves it, on the box the gate runs on", () => {
    expect(CAMERA_BOX_STATE_URL).toBe("http://127.0.0.1:8899/bundle-state.json");
  });

  test("a fresh line with the probe on program, connected and locked is ready", () => {
    const entry = { ...LIVE_IDLE, idle: false };
    expect(probeLock(doc(entry), AV_PROBE_INPUT)).toEqual({ kind: "locked", entry });
  });

  test("a line from before the switch (idle: true) never passes, locked or not", () => {
    // camera-box's live value: locked + connected, but off program. The facet
    // is rewritten every 30 s, so only idle: false proves the line was written
    // after the gate put the probe scene on program.
    expect(probeLock(doc(LIVE_IDLE), AV_PROBE_INPUT).kind).toBe("stale");
    const { idle: _idle, ...noIdle } = LIVE_IDLE;
    expect(probeLock(doc(noIdle), AV_PROBE_INPUT).kind).toBe("stale");
  });

  test("on program but not connected, or connected but not locked, keeps waiting", () => {
    expect(probeLock(doc({ ...LIVE_IDLE, idle: false, connected: false }), AV_PROBE_INPUT).kind).toBe(
      "not_connected",
    );
    expect(probeLock(doc({ ...LIVE_IDLE, idle: false, locked: false }), AV_PROBE_INPUT).kind).toBe(
      "not_locked",
    );
    // Only real booleans count.
    expect(
      probeLock(doc({ ...LIVE_IDLE, idle: false, locked: "true" }), AV_PROBE_INPUT).kind,
    ).toBe("not_locked");
    expect(probeLock(doc({ ...LIVE_IDLE, idle: false, connected: 1 }), AV_PROBE_INPUT).kind).toBe(
      "not_connected",
    );
    expect(probeLock(doc({ ...LIVE_IDLE, idle: 0 }), AV_PROBE_INPUT).kind).toBe("stale");
  });

  test("a document without the facet or without the probe is named", () => {
    for (const d of [null, "x", 42, {}, { genlock_lock: null }, { genlock_lock: { inputs: [] } }]) {
      expect(probeLock(d, AV_PROBE_INPUT).kind).toBe("no_facet");
    }
    expect(probeLock({ genlock_lock: { inputs: { "cam 1": {} } } }, AV_PROBE_INPUT)).toEqual({
      kind: "no_input",
      inputs: ["cam 1"],
    });
    // An entry that is not an object is no reading of the probe.
    expect(probeLock(doc(null), AV_PROBE_INPUT).kind).toBe("no_input");
    expect(probeLock(doc("locked"), AV_PROBE_INPUT).kind).toBe("no_input");
  });

  test("every state is described by what was read", () => {
    const fresh = { ...LIVE_IDLE, idle: false };
    const cases: Array<[ProbeLock, RegExp]> = [
      [{ kind: "unreadable", error: "ECONNREFUSED" }, /not readable.*ECONNREFUSED/],
      [{ kind: "no_facet" }, /no genlock_lock\.inputs facet/],
      [{ kind: "no_input", inputs: ["cam 1", "cam 2"] }, /does not list the probe.*"cam 1", "cam 2"/],
      [{ kind: "stale", entry: LIVE_IDLE }, /off program.*before cg OBS switched.*every 30 s.*"idle":true/],
      [{ kind: "not_connected", entry: { ...fresh, connected: false } }, /not connected.*"connected":false/],
      [{ kind: "not_locked", entry: { ...fresh, locked: false } }, /not locked.*"locked":false/],
      [{ kind: "locked", entry: fresh }, /locked and connected, on program.*"depth":0/],
    ];
    for (const [lock, re] of cases) expect(describeProbeLock(lock)).toMatch(re);
    // A long input list is cut, never dumped whole.
    const many = Array.from({ length: 30 }, (_, i) => `in${i}`);
    const text = describeProbeLock({ kind: "no_input", inputs: many });
    expect(text).toContain('"in9"');
    expect(text).not.toContain('"in10"');
    expect(text).toContain("20 more");
  });

  test("the analysed part starts the settle after the probe's receiver attached", () => {
    const attached = 1_000_000;
    // Recording started 5 s after the attach: the first 40 s are the settle head.
    expect(analysisStartMs(attached, attached + 5_000)).toBe(PROBE_SETTLE_MS - 5_000);
    // Started exactly at, or after, the settle: the whole recording is analysed.
    expect(analysisStartMs(attached, attached + PROBE_SETTLE_MS)).toBe(0);
    expect(analysisStartMs(attached, attached + 300_000)).toBe(0);
    expect(analysisStartMs(attached, attached, 10_000)).toBe(10_000);
  });

  test("the settle and the lock bound follow camera-box's numbers", () => {
    // camera-box 6148340563: a cold start settles in >= ~45 s (latch ~3 s +
    // one frame of depth, 33 ms, slewed at 1000 ppm = 33 s).
    expect(PROBE_SETTLE_MS).toBeGreaterThanOrEqual(45_000);
    // The facet is rewritten every 30 s: a line written just before the lock
    // latched is followed by one that reads it, inside two lines.
    expect(FACET_LINE_MS).toBe(30_000);
    expect(PROBE_LOCK_TIMEOUT_MS).toBe(2 * FACET_LINE_MS);
  });

  test("the cold probe never costs a take more than COLD_PROBE_MS", () => {
    expect(COLD_PROBE_MS).toBe(Math.max(PROBE_LOCK_TIMEOUT_MS, PROBE_SETTLE_MS));
    // Whatever the lock wait took (up to its bound), the wait plus the recorded
    // settle head ends within COLD_PROBE_MS of the attach.
    for (let lockWait = 0; lockWait <= PROBE_LOCK_TIMEOUT_MS; lockWait += 2_500) {
      expect(lockWait + analysisStartMs(0, lockWait)).toBeLessThanOrEqual(COLD_PROBE_MS);
    }
  });
});
