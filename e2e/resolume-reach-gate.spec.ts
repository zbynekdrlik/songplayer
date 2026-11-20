// #343: the Resolume reach gate's pure verdict (no browser, no box).
import { test, expect } from "@playwright/test";
import {
  pollReach,
  reachFailures,
  reachSummary,
  reached,
  type ResolumeHostHealth,
  type ReachRead,
} from "./resolume-reach-gate";

function host(over: Partial<ResolumeHostHealth> = {}): ResolumeHostHealth {
  return {
    host: "127.0.0.1",
    last_refresh_ts: "2026-11-19T23:58:33.573628Z",
    last_refresh_ok: true,
    consecutive_failures: 0,
    circuit_breaker_open: false,
    product_latency_ms: 0,
    clips_by_token: { "#sp-subs": 1 },
    ...over,
  };
}

/** resolume-pp at 00:58 CET 20.11.2026: Arena-Bridge accepting nothing. */
const ppHung = host({
  last_refresh_ok: false,
  consecutive_failures: 86,
  circuit_breaker_open: true,
  product_latency_ms: null,
});

test("a host whose last refresh was good with the breaker closed is reached", () => {
  expect(reached(host())).toBe(true);
  expect(reached(host({ last_refresh_ok: false }))).toBe(false);
  expect(reached(host({ circuit_breaker_open: true }))).toBe(false);
});

test("PP's hung Arena-Bridge fails the gate, naming the host", () => {
  expect(reachFailures([ppHung])).toEqual([
    "SongPlayer does not reach the Resolume host 127.0.0.1: the last refresh failed " +
      "(86 in a row), circuit breaker open, last try 2026-11-19T23:58:33.573628Z",
  ]);
});

test("a host never refreshed says so", () => {
  const fresh = host({ last_refresh_ok: false, consecutive_failures: 0, last_refresh_ts: null });
  expect(reachFailures([fresh])).toEqual([
    "SongPlayer does not reach the Resolume host 127.0.0.1: the last refresh failed " +
      "(0 in a row), never refreshed",
  ]);
});

test("a hung host fails whatever Arena.exe does: on PP the driven host is Arena-Bridge.exe", async () => {
  // resolume-pp, 19.11.2026: `arena_running` sees `Arena.exe` alone. With
  // the "Songs PP" Arena closed and Arena-Bridge hung it reads false, and
  // the gate must still fail.
  const last = await pollReach(
    async () => ({ hosts: [ppHung], arenaRunning: false }),
    0,
    async () => {},
  );
  expect(reachFailures(last.hosts)).toHaveLength(1);
});

test("a host whose map cannot be read is not reached (the probe alone answering)", () => {
  // `last_refresh_ok` is also written by a failed /composition read: with
  // no map SongPlayer pushes nothing.
  expect(reachFailures([host({ last_refresh_ok: false, consecutive_failures: 1 })])).toHaveLength(1);
});

test("reached hosts and an empty host list pass; a dead route fails", () => {
  expect(reachFailures([host(), host({ host: "10.0.0.2" })])).toEqual([]);
  expect(reachFailures([])).toEqual([]);
  expect(reachFailures(null)).toEqual(["GET /api/v1/resolume/health did not answer"]);
});

test("only the hosts not reached are named", () => {
  const failures = reachFailures([host(), host({ host: "10.0.0.2", circuit_breaker_open: true })]);
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain("10.0.0.2");
});

test("the summary names each host's state", () => {
  expect(reachSummary([ppHung], true)).toBe(
    "Arena.exe running; 1 host(s): 127.0.0.1 NOT reached (failures 86, breaker open, latency — ms)",
  );
  expect(reachSummary([], false)).toBe("Arena.exe not running; 0 host(s)");
  expect(reachSummary(null, null)).toBe("no answer");
});

test("the poll waits until every host is reached and returns that read", async () => {
  const reads: ReachRead[] = [
    { hosts: [host({ last_refresh_ok: false, consecutive_failures: 1 })], arenaRunning: true },
    { hosts: null, arenaRunning: true },
    { hosts: [host()], arenaRunning: true },
  ];
  let i = 0;
  let clock = 0;
  const slept: number[] = [];
  const last = await pollReach(
    async () => reads[Math.min(i++, reads.length - 1)],
    60_000,
    async (ms) => {
      slept.push(ms);
      clock += ms;
    },
    2_000,
    () => clock,
  );
  expect(last.hosts).toEqual([host()]);
  expect(slept).toEqual([2_000, 2_000]);
});

test("the poll gives up after its window and returns the failing read", async () => {
  let reads = 0;
  let clock = 0;
  const last = await pollReach(
    async () => {
      reads++;
      return { hosts: [ppHung], arenaRunning: true };
    },
    6_000,
    async (ms) => {
      clock += ms;
    },
    2_000,
    () => clock,
  );
  expect(reachFailures(last.hosts)).toHaveLength(1);
  expect(reads).toBe(4);
});

test("the window counts the reads' own time, not only the sleeps", async () => {
  // Each read on the box may take up to its 10 s timeout: the window is
  // elapsed time, so slow reads end it sooner.
  let reads = 0;
  let clock = 0;
  await pollReach(
    async () => {
      reads++;
      clock += 10_000;
      return { hosts: [ppHung], arenaRunning: true };
    },
    25_000,
    async (ms) => {
      clock += ms;
    },
    2_000,
    () => clock,
  );
  expect(reads).toBe(3);
});

test("a passing first read does not wait", async () => {
  let reads = 0;
  await pollReach(
    async () => {
      reads++;
      return { hosts: [host()], arenaRunning: true };
    },
    60_000,
    async () => {
      throw new Error("must not sleep");
    },
  );
  expect(reads).toBe(1);
});
