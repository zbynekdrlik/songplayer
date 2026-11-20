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

test("PP's hung Arena-Bridge fails the gate while Arena runs, naming the host", () => {
  expect(reachFailures([ppHung], true)).toEqual([
    "SongPlayer reaches no Resolume at 127.0.0.1: 86 refresh(es) failed in a row, " +
      "circuit breaker open, last try 2026-11-19T23:58:33.573628Z",
  ]);
});

test("a host never refreshed says so", () => {
  const fresh = host({ last_refresh_ok: false, consecutive_failures: 0, last_refresh_ts: null });
  expect(reachFailures([fresh], true)).toEqual([
    "SongPlayer reaches no Resolume at 127.0.0.1: 0 refresh(es) failed in a row, never refreshed",
  ]);
});

test("an unknown Arena state is judged as running", () => {
  expect(reachFailures([ppHung], null)).toHaveLength(1);
});

test("Arena not running on the box is the operator's setup: no failure", () => {
  expect(reachFailures([ppHung], false)).toEqual([]);
});

test("reached hosts and an empty host list pass; a dead route fails", () => {
  expect(reachFailures([host(), host({ host: "10.0.0.2" })], true)).toEqual([]);
  expect(reachFailures([], true)).toEqual([]);
  expect(reachFailures(null, true)).toEqual(["GET /api/v1/resolume/health did not answer"]);
});

test("only the hosts not reached are named", () => {
  const failures = reachFailures([host(), host({ host: "10.0.0.2", circuit_breaker_open: true })], true);
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain("10.0.0.2");
});

test("the summary names each host's state", () => {
  expect(reachSummary([ppHung], true)).toBe(
    "Arena running; 1 host(s): 127.0.0.1 NOT reached (failures 86, breaker open, latency — ms)",
  );
  expect(reachSummary([], false)).toBe("Arena not running; 0 host(s)");
  expect(reachSummary(null, null)).toBe("no answer");
});

test("the poll waits until every host is reached and returns that read", async () => {
  const reads: ReachRead[] = [
    { hosts: [host({ last_refresh_ok: false, consecutive_failures: 1 })], arenaRunning: true },
    { hosts: null, arenaRunning: true },
    { hosts: [host()], arenaRunning: true },
  ];
  let i = 0;
  const slept: number[] = [];
  const last = await pollReach(
    async () => reads[Math.min(i++, reads.length - 1)],
    60_000,
    async (ms) => {
      slept.push(ms);
    },
    2_000,
  );
  expect(last.hosts).toEqual([host()]);
  expect(slept).toEqual([2_000, 2_000]);
});

test("the poll gives up after its window and returns the failing read", async () => {
  let reads = 0;
  const last = await pollReach(
    async () => {
      reads++;
      return { hosts: [ppHung], arenaRunning: true };
    },
    6_000,
    async () => {},
    2_000,
  );
  expect(reachFailures(last.hosts, last.arenaRunning)).toHaveLength(1);
  expect(reads).toBe(4);
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
