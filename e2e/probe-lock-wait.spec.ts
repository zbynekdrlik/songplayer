/**
 * Unit tests for the A/V gate's wait for cg OBS's genlock LOCK on its probe
 * (#221, dev.19). Runs in the ubuntu mock suite (playwright.config.ts); these
 * `test()` blocks never touch `page`, so they need no browser and no box.
 *
 * dev.18 waits for the probe's `InputVolumeMeters`, which is tapped BEFORE
 * camera-box's genlock audio-pairing withhold: it proves DistroAV delivers
 * audio, never that the mix gets it. The main session's ROZHODNUTÉ
 * (#221 comment 6012938501) makes the wait exact: before the meter wait, the
 * gate polls camera-box's `genlock_lock` facet on cg OBS's `:8899`
 * `/bundle-state.json` (camera-box's read path, #221 comment 6012957666)
 * until the probe's `idle` is false, its `locked` is true, and the box is
 * `LOCKED` for reason `none`. `idle === false` proves the heartbeat is from
 * after the attach.
 */

import { test, expect } from "@playwright/test";
import {
  DEFAULT_BUNDLE_STATE_URLS,
  LOCK_POLL_MS,
  LOCK_READ_TIMEOUT_MS,
  LOCK_WAIT_TIMEOUT_MS,
  LOCK_WAIT_WORST_MS,
  bundleStateRead,
  bundleStateUrls,
  explainProbeLock,
  parseGenlockLock,
  probeLockVerdict,
  resolveBundleState,
  waitForProbeLock,
  type GenlockLock,
  type HttpGet,
} from "./probe-lock-wait";

const PROBE = "A/V gate SP-program";

/** The probe's per-input entry, as camera-box's facet carries it. */
function probe(fields: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    locked: true,
    connected: true,
    idle: false,
    latency_ms: 100,
    underruns: 0,
    relocks: 1,
    late_holds: 0,
    depth: 3,
    ...fields,
  };
}

/** A `genlock_lock` facet: the box verdict, one live playlist input, and the
 *  probe's entry (none for null). */
function lock(
  state: string,
  reason: string,
  probeEntry: Record<string, unknown> | null,
  extra: Record<string, unknown> = {},
): GenlockLock {
  return {
    state,
    reason,
    n_inputs: probeEntry ? 2 : 1,
    inputs: {
      "sp-slow": { locked: true, connected: true, idle: false, depth: 2 },
      ...(probeEntry ? { [PROBE]: probeEntry } : {}),
    },
    source: "log",
    ...extra,
  };
}

/** camera-box's bundle-state body carrying `genlock_lock`. */
function body(genlockLock: unknown): string {
  return JSON.stringify({ genlock_build_sha: "abc", obs_process_count: "1", genlock_lock: genlockLock });
}

/** A fake clock: `sleep` advances it and records each sleep. */
function fakeClock() {
  const clock = {
    t: 0,
    sleeps: [] as number[],
    now: () => clock.t,
    sleep: async (ms: number) => {
      clock.sleeps.push(ms);
      clock.t += ms;
    },
  };
  return clock;
}

/** A read that answers `at(startMs)` after `durationMs` of the fake clock,
 *  recording when each read started and the timeout it was given. */
function fakeRead(
  clock: ReturnType<typeof fakeClock>,
  at: (startMs: number) => GenlockLock,
  durationMs = 0,
) {
  const starts: number[] = [];
  const timeouts: number[] = [];
  return {
    starts,
    timeouts,
    read: async (timeoutMs: number): Promise<GenlockLock> => {
      starts.push(clock.t);
      timeouts.push(timeoutMs);
      const start = clock.t;
      clock.t += durationMs;
      return at(start);
    },
  };
}

async function rejection(p: Promise<unknown>): Promise<string> {
  return p.then(
    () => {
      throw new Error("expected a rejection");
    },
    (e: Error) => e.message,
  );
}

test.describe("A/V gate: wait for cg OBS's genlock lock on the probe (#221 dev.19)", () => {
  test("the defaults: 250 ms polls, 15 s bound, 10 s per read, 25 s worst case", () => {
    expect(LOCK_POLL_MS).toBe(250);
    expect(LOCK_WAIT_TIMEOUT_MS).toBe(15_000);
    // camera-box's own :8899 watchdogs allow 10 s: the server has answered in ~6.6 s.
    expect(LOCK_READ_TIMEOUT_MS).toBe(10_000);
    // A read may start at the bound and take its own timeout.
    expect(LOCK_WAIT_WORST_MS).toBe(25_000);
  });

  test("the endpoint: loopback first (the gate runs on cg OBS's box), then resolume.lan", () => {
    expect(DEFAULT_BUNDLE_STATE_URLS).toEqual([
      "http://127.0.0.1:8899/bundle-state.json",
      "http://localhost:8899/bundle-state.json",
      "http://resolume.lan:8899/bundle-state.json",
    ]);
    expect(bundleStateUrls(undefined)).toEqual(DEFAULT_BUNDLE_STATE_URLS);
    expect(bundleStateUrls("")).toEqual(DEFAULT_BUNDLE_STATE_URLS);
    expect(bundleStateUrls(" , ")).toEqual(DEFAULT_BUNDLE_STATE_URLS);
    // CG_BUNDLE_STATE_URL replaces the defaults: one URL, or a comma list.
    expect(bundleStateUrls("http://cg:8899/bundle-state.json")).toEqual([
      "http://cg:8899/bundle-state.json",
    ]);
    expect(bundleStateUrls(" http://a:1/x , http://b:2/y ,")).toEqual(["http://a:1/x", "http://b:2/y"]);
    // A copy: the caller can never change the defaults.
    bundleStateUrls(undefined).pop();
    expect(DEFAULT_BUNDLE_STATE_URLS).toHaveLength(3);
  });

  test("all four hold: the probe receives, it is locked, the box is LOCKED for none → go", () => {
    expect(probeLockVerdict(lock("LOCKED", "none", probe()), PROBE)).toEqual({ go: true, unmet: [] });
  });

  test("a stale heartbeat (the probe still idle) → wait, even when everything else holds", () => {
    // The newest heartbeat is from before the attach: the probe idle, and
    // everything else as a long-attached input would read.
    const v = probeLockVerdict(lock("LOCKED", "none", probe({ idle: true })), PROBE);
    expect(v.go).toBe(false);
    expect(v.unmet).toEqual(["probe idle === false"]);
  });

  test("the probe locked but the box DEGRADED for audio_pairing → wait", () => {
    // camera-box's observed attach (6.10.2026): shallow latch, then
    // DEGRADED/audio_pairing with the probe locked and not idle, then LOCKED.
    const v = probeLockVerdict(lock("DEGRADED", "audio_pairing", probe()), PROBE);
    expect(v.go).toBe(false);
    expect(v.unmet).toEqual(['state === "LOCKED"', 'reason === "none"']);
  });

  test("the box LOCKED for another reason, or the probe unlocked → wait", () => {
    expect(probeLockVerdict(lock("LOCKED", "recent_event", probe()), PROBE).unmet).toEqual([
      'reason === "none"',
    ]);
    expect(probeLockVerdict(lock("UNLOCKED", "none", probe()), PROBE).unmet).toEqual([
      'state === "LOCKED"',
    ]);
    expect(probeLockVerdict(lock("LOCKED", "none", probe({ locked: false })), PROBE).unmet).toEqual([
      "probe locked === true",
    ]);
  });

  test("a missing probe key → wait", () => {
    const v = probeLockVerdict(lock("LOCKED", "none", null), PROBE);
    expect(v.go).toBe(false);
    expect(v.unmet).toEqual([`input "${PROBE}" present`]);
    // No inputs at all, or not an object.
    expect(probeLockVerdict({ state: "LOCKED", reason: "none" }, PROBE).go).toBe(false);
    expect(probeLockVerdict({ state: "LOCKED", reason: "none", inputs: [] }, PROBE).go).toBe(false);
    expect(
      probeLockVerdict({ state: "LOCKED", reason: "none", inputs: { [PROBE]: "locked" } }, PROBE).go,
    ).toBe(false);
    // Only the facet's OWN keys: "__proto__" names no inherited object.
    expect(explainProbeLock(lock("LOCKED", "none", null), "__proto__")).toContain(
      'lists no input "__proto__"',
    );
  });

  test("the values are compared exactly: no truthy or case-folded match", () => {
    for (const p of [
      probe({ idle: "false" }),
      probe({ idle: 0 }),
      probe({ idle: undefined }),
      probe({ locked: "true" }),
      probe({ locked: 1 }),
    ]) {
      expect(probeLockVerdict(lock("LOCKED", "none", p), PROBE).go, JSON.stringify(p)).toBe(false);
    }
    expect(probeLockVerdict(lock("locked", "none", probe()), PROBE).go).toBe(false);
    expect(probeLockVerdict(lock("LOCKED", "None", probe()), PROBE).go).toBe(false);
    expect(probeLockVerdict(lock("LOCKED", "", probe()), PROBE).go).toBe(false);
  });

  test("the explanation follows the first condition the attach has not reached", () => {
    expect(explainProbeLock(lock("LOCKED", "none", null), PROBE)).toContain(
      `lists no input "${PROBE}"`,
    );
    const stale = explainProbeLock(lock("UNLOCKED", "none", probe({ idle: true, locked: false })), PROBE);
    expect(stale).toContain("written before the attach");
    expect(stale).toContain("every ~30 s");
    expect(explainProbeLock(lock("DEGRADED", "audio_pairing", probe({ locked: false })), PROBE)).toContain(
      "not locked",
    );
    const pairing = explainProbeLock(lock("DEGRADED", "audio_pairing", probe()), PROBE);
    expect(pairing).toContain('"DEGRADED" for audio_pairing');
    expect(pairing).toContain("audio");
    const other = explainProbeLock(lock("DEGRADED", "recent_event", probe()), PROBE);
    expect(other).toContain('"DEGRADED" (reason "recent_event")');
    expect(other).not.toContain("audio");
  });

  test("parseGenlockLock reads the genlock_lock object of a 200 JSON body", () => {
    const gl = lock("LOCKED", "none", probe());
    expect(parseGenlockLock(200, body(gl))).toEqual(gl);
  });

  test("parseGenlockLock fails loud on anything else", () => {
    expect(() => parseGenlockLock(500, "")).toThrow("HTTP 500");
    expect(() => parseGenlockLock(404, body(lock("LOCKED", "none", probe())))).toThrow("HTTP 404");
    expect(() => parseGenlockLock(200, "<html>busy</html>")).toThrow("not JSON");
    expect(() => parseGenlockLock(200, "[1, 2]")).toThrow("not a JSON object");
    expect(() => parseGenlockLock(200, "null")).toThrow("not a JSON object");
    // camera-box OMITS the facet when cg OBS's log holds no genlock-lock-json: line.
    expect(() => parseGenlockLock(200, JSON.stringify({ genlock_build_sha: "abc" }))).toThrow(
      "no genlock_lock object",
    );
    expect(() => parseGenlockLock(200, body(null))).toThrow("no genlock_lock object");
    expect(() => parseGenlockLock(200, body("LOCKED"))).toThrow("no genlock_lock object");
  });

  test("resolveBundleState takes the first URL that answers with the facet", async () => {
    const asked: Array<[string, number]> = [];
    const get: HttpGet = async (url, timeoutMs) => {
      asked.push([url, timeoutMs]);
      if (url.includes("127.0.0.1")) throw new Error("connect ECONNREFUSED 127.0.0.1:8899");
      if (url.includes("localhost")) return { status: 200, body: JSON.stringify({ genlock_build_sha: "x" }) };
      return { status: 200, body: body(lock("LOCKED", "none", null)) };
    };
    const found = await resolveBundleState(DEFAULT_BUNDLE_STATE_URLS, get);
    expect(found.url).toBe("http://resolume.lan:8899/bundle-state.json");
    expect(found.lock.state).toBe("LOCKED");
    expect(asked).toEqual(DEFAULT_BUNDLE_STATE_URLS.map((u) => [u, LOCK_READ_TIMEOUT_MS]));
  });

  test("resolveBundleState stops at the first URL that answers", async () => {
    const asked: string[] = [];
    const get: HttpGet = async (url) => {
      asked.push(url);
      return { status: 200, body: body(lock("DEGRADED", "audio_pairing", probe())) };
    };
    expect((await resolveBundleState(DEFAULT_BUNDLE_STATE_URLS, get, 3_000)).url).toBe(
      DEFAULT_BUNDLE_STATE_URLS[0],
    );
    expect(asked).toEqual([DEFAULT_BUNDLE_STATE_URLS[0]]);
  });

  test("an unreachable endpoint fails loud, naming every URL and what it answered", async () => {
    const get: HttpGet = async (url) => {
      if (url.includes("resolume.lan")) throw new Error("getaddrinfo ENOTFOUND resolume.lan");
      if (url.includes("localhost")) return { status: 500, body: "" };
      throw new Error("connect ECONNREFUSED 127.0.0.1:8899");
    };
    const msg = await rejection(resolveBundleState(DEFAULT_BUNDLE_STATE_URLS, get));
    expect(msg).toContain("http://127.0.0.1:8899/bundle-state.json: connect ECONNREFUSED 127.0.0.1:8899");
    expect(msg).toContain("http://localhost:8899/bundle-state.json: HTTP 500");
    expect(msg).toContain("http://resolume.lan:8899/bundle-state.json: getaddrinfo ENOTFOUND resolume.lan");
    expect(msg).toContain("CG_BUNDLE_STATE_URL");
    expect(msg).toContain("(#221)");
  });

  test("bundleStateRead reads one URL with the timeout it is given", async () => {
    const asked: Array<[string, number]> = [];
    const get: HttpGet = async (url, timeoutMs) => {
      asked.push([url, timeoutMs]);
      return { status: 200, body: body(lock("LOCKED", "none", probe())) };
    };
    const read = bundleStateRead("http://cg:8899/bundle-state.json", get);
    expect((await read(1_234)).state).toBe("LOCKED");
    expect(asked).toEqual([["http://cg:8899/bundle-state.json", 1_234]]);
    const bad = bundleStateRead("http://cg:8899/bundle-state.json", async () => ({ status: 503, body: "" }));
    expect(await rejection(bad(10))).toContain("http://cg:8899/bundle-state.json: HTTP 503");
  });

  test("waitForProbeLock: idle → DEGRADED/audio_pairing → LOCKED/none, polled every 250 ms", async () => {
    // camera-box's observed attach, from the call: the old heartbeat (probe
    // idle) until 3.1 s, DEGRADED/audio_pairing (probe locked) until 4.1 s,
    // then LOCKED/none.
    const clock = fakeClock();
    const attach = (at: number): GenlockLock =>
      at < 3_100
        ? lock("LOCKED", "none", probe({ idle: true, locked: false }))
        : at < 4_100
          ? lock("DEGRADED", "audio_pairing", probe())
          : lock("LOCKED", "none", probe());
    const reads = fakeRead(clock, attach, 50);
    const report = await waitForProbeLock(reads.read, PROBE, { now: clock.now, sleep: clock.sleep });
    // Reads start every 300 ms (50 ms read + 250 ms poll): the first at or
    // after 4.1 s is the 15th, at 4 200 ms; it answers at 4 250 ms.
    expect(reads.starts).toHaveLength(15);
    expect(reads.starts[14]).toBe(4_200);
    expect(report.reads).toBe(15);
    expect(report.waitedMs).toBe(4_250);
    expect(report.slowestReadMs).toBe(50);
    expect(clock.sleeps).toEqual(Array(14).fill(LOCK_POLL_MS));
    expect(reads.timeouts.every((t) => t === LOCK_READ_TIMEOUT_MS)).toBe(true);
    // The trail names each change once, with when its read started.
    expect(report.seen).toEqual([
      "+0 ms LOCKED/none, probe locked=false idle=true",
      "+3300 ms DEGRADED/audio_pairing, probe locked=true idle=false",
      "+4200 ms LOCKED/none, probe locked=true idle=false",
    ]);
  });

  test("waitForProbeLock goes at once when the probe is already locked (a retake)", async () => {
    const clock = fakeClock();
    const reads = fakeRead(clock, () => lock("LOCKED", "none", probe()), 700);
    const report = await waitForProbeLock(reads.read, PROBE, { now: clock.now, sleep: clock.sleep });
    expect(report.reads).toBe(1);
    expect(report.waitedMs).toBe(700);
    expect(clock.sleeps).toEqual([]);
  });

  test("waitForProbeLock fails loudly at its bound with the last genlock_lock it saw", async () => {
    const clock = fakeClock();
    const stuck = lock(
      "DEGRADED",
      "audio_pairing",
      probe({ underruns: 4, relocks: 2, late_holds: 1, depth: 5 }),
      { recent_event_inputs: [{ name: "sp-slow", events: 3 }], audio_unexpected_inputs: [{ name: "mic" }] },
    );
    const reads = fakeRead(clock, () => stuck, 1_000);
    const msg = await rejection(waitForProbeLock(reads.read, PROBE, { now: clock.now, sleep: clock.sleep }));
    expect(msg).toContain(`cg OBS's genlock did not lock the probe "${PROBE}" within 15000 ms`);
    expect(msg).toContain('its idle false, locked true, state "LOCKED", reason "none"');
    expect(msg).toContain('state "DEGRADED", reason "audio_pairing"');
    expect(msg).toContain(
      `input "${PROBE}" {"locked":true,"connected":true,"idle":false,"latency_ms":100,"underruns":4,"relocks":2,"late_holds":1,"depth":5}`,
    );
    expect(msg).toContain('recent_event_inputs [{"name":"sp-slow","events":3}]');
    expect(msg).toContain('audio_unexpected_inputs [{"name":"mic"}]');
    expect(msg).toContain('"DEGRADED" for audio_pairing');
    expect(msg).toContain("the slowest 1000 ms");
    expect(msg).toContain("+0 ms DEGRADED/audio_pairing, probe locked=true idle=false");
    expect(msg).toContain("(#221)");
    // Reads every 1.25 s from 0; the last one starts within the bound.
    expect(reads.starts.at(-1)).toBeLessThanOrEqual(LOCK_WAIT_TIMEOUT_MS);
    expect(msg).toContain(`${reads.starts.length} reads`);
  });

  test("the bound names a stale heartbeat, and a missing probe with the inputs it saw", async () => {
    const clock = fakeClock();
    const stale = fakeRead(clock, () => lock("LOCKED", "none", probe({ idle: true })));
    const msg = await rejection(
      waitForProbeLock(stale.read, PROBE, { now: clock.now, sleep: clock.sleep, timeoutMs: 1_000 }),
    );
    expect(msg).toContain("within 1000 ms");
    expect(msg).toContain("written before the attach");
    // Instant reads: one every 250 ms, the last AT the bound, then no pause.
    expect(stale.starts).toEqual([0, 250, 500, 750, 1_000]);
    expect(clock.sleeps).toHaveLength(4);
    expect(msg).toContain("5 reads");
    const clock2 = fakeClock();
    const missing = fakeRead(clock2, () => lock("LOCKED", "none", null));
    const msg2 = await rejection(
      waitForProbeLock(missing.read, PROBE, { now: clock2.now, sleep: clock2.sleep, timeoutMs: 1_000 }),
    );
    expect(msg2).toContain(`no input "${PROBE}" (inputs: "sp-slow")`);
    expect(msg2).toContain(`lists no input "${PROBE}"`);
  });

  test("a read started within the bound counts, even when it answers after it", async () => {
    // camera-box's gather takes seconds: the read that starts at 14 s sees
    // the state at 14 s.
    const clock = fakeClock();
    const locksAt14 = (at: number): GenlockLock =>
      at < 14_000 ? lock("DEGRADED", "audio_pairing", probe()) : lock("LOCKED", "none", probe());
    const reads = fakeRead(clock, locksAt14, 4_500);
    const report = await waitForProbeLock(reads.read, PROBE, { now: clock.now, sleep: clock.sleep });
    // 4.5 s reads + 250 ms polls: the 4th read starts at 14 250 ms, inside
    // the bound, and answers LOCKED at 18 750 ms.
    expect(reads.starts).toEqual([0, 4_750, 9_500, 14_250]);
    expect(report.reads).toBe(4);
    expect(report.waitedMs).toBe(18_750);
    expect(report.slowestReadMs).toBe(4_500);
  });

  test("the trail keeps each change once, at most 20 lines", async () => {
    // A facet that flips every read (30 reads in 7.25 s), then locks.
    const clock = fakeClock();
    const flip = (at: number): GenlockLock =>
      at >= 7_500
        ? lock("LOCKED", "none", probe())
        : (at / 250) % 2 === 0
          ? lock("DEGRADED", "recent_event", probe())
          : lock("DEGRADED", "audio_pairing", probe());
    const reads = fakeRead(clock, flip);
    const report = await waitForProbeLock(reads.read, PROBE, { now: clock.now, sleep: clock.sleep });
    expect(report.reads).toBe(31);
    expect(report.seen).toHaveLength(20);
    expect(report.seen[0]).toBe("+0 ms DEGRADED/recent_event, probe locked=true idle=false");
    expect(report.seen[19]).toBe("+4750 ms DEGRADED/audio_pairing, probe locked=true idle=false");
    // A repeated summary is kept once.
    const clock2 = fakeClock();
    const locksAt1s = (at: number): GenlockLock =>
      at < 1_000 ? lock("DEGRADED", "audio_pairing", probe()) : lock("LOCKED", "none", probe());
    const steady = fakeRead(clock2, locksAt1s);
    const report2 = await waitForProbeLock(steady.read, PROBE, { now: clock2.now, sleep: clock2.sleep });
    expect(report2.reads).toBe(5);
    expect(report2.seen).toEqual([
      "+0 ms DEGRADED/audio_pairing, probe locked=true idle=false",
      "+1000 ms LOCKED/none, probe locked=true idle=false",
    ]);
  });

  test("no read starts after the bound, and the wait ends within its worst case", async () => {
    const clock = fakeClock();
    const reads = fakeRead(clock, () => lock("DEGRADED", "audio_pairing", probe()), LOCK_READ_TIMEOUT_MS);
    const started = clock.t;
    await rejection(waitForProbeLock(reads.read, PROBE, { now: clock.now, sleep: clock.sleep }));
    expect(reads.starts.every((s) => s - started <= LOCK_WAIT_TIMEOUT_MS)).toBe(true);
    expect(clock.t - started).toBeLessThanOrEqual(LOCK_WAIT_WORST_MS);
  });

  test("an endpoint that stops answering mid-wait fails it at once", async () => {
    const clock = fakeClock();
    let n = 0;
    const read = async (): Promise<GenlockLock> => {
      n++;
      clock.t += 100;
      if (n === 3) throw new Error("http://127.0.0.1:8899/bundle-state.json: connect ECONNREFUSED 127.0.0.1:8899");
      return lock("DEGRADED", "audio_pairing", probe());
    };
    const msg = await rejection(waitForProbeLock(read, PROBE, { now: clock.now, sleep: clock.sleep }));
    expect(n, "no read after the failure").toBe(3);
    expect(msg).toContain(`could not read cg OBS's genlock state while waiting for the probe "${PROBE}"`);
    expect(msg).toContain("after 800 ms and 2 reads");
    expect(msg).toContain("connect ECONNREFUSED 127.0.0.1:8899");
    expect(msg).toContain("(#221)");
  });
});
