/**
 * #343: does SongPlayer reach the Resolume hosts it drives? The post-deploy
 * gate's pure verdict (unit-tested in the mock suite by
 * `resolume-reach-gate.spec.ts`, run by `post-deploy-resolume-reach.spec.ts`
 * in SNV's suite and PP's subset).
 *
 * On 19.11.2026 resolume-pp's Arena-Bridge (the Arena SongPlayer's host
 * `127.0.0.1:8090` is) stopped accepting at 18:44 CET while it kept
 * rendering: no lyrics and no title reached the PP wall for hours, the
 * dashboard's segment read `Resolume: neodpovedá`, and the PP deploy that
 * night passed every Resolume-related check, because none read SongPlayer's
 * own reach of its hosts.
 *
 * The verdict reads `GET /api/v1/resolume/health` (one row per enabled
 * host, `crate::resolume::HostHealthSnapshot`): a host is reached once its
 * last refresh was good (`last_refresh_ok`) and its circuit breaker is
 * closed. The poll waits until every host is reached (right after a
 * restart the driver's first reads are still under way) and fails on the
 * hosts still not reached when its window ends.
 *
 * The owner's rule (#259 comment 6142937258): whether Resolume runs is the
 * operator's setup. So a box where Arena is not running
 * (`GET /api/v1/wall` `read.arena_running: false`) is logged and passes;
 * an empty host list passes too (nothing is driven).
 */

/** One row of `GET /api/v1/resolume/health`. */
export interface ResolumeHostHealth {
  host: string;
  last_refresh_ts?: string | null;
  last_refresh_ok: boolean;
  consecutive_failures: number;
  circuit_breaker_open: boolean;
  product_latency_ms?: number | null;
  last_full_refresh_ts?: string | null;
  clips_by_token?: Record<string, number>;
}

/** How long the gate waits for every host to be reached after a deploy. */
export const REACH_WAIT_MS = 60_000;

/** The poll's step. */
export const REACH_POLL_MS = 2_000;

/** A host SongPlayer reaches: its last refresh was good, the breaker closed. */
export function reached(host: ResolumeHostHealth): boolean {
  return host.last_refresh_ok === true && host.circuit_breaker_open !== true;
}

/**
 * The gate's failures: one line per host not reached, naming why.
 * `hosts` null = the route did not answer. `arenaRunning` false = Arena is
 * not running on the box: no failure (the operator's setup); null = not
 * known, judged as running (a SongPlayer that cannot tell must not pass a
 * dead host).
 */
export function reachFailures(
  hosts: ResolumeHostHealth[] | null,
  arenaRunning: boolean | null,
): string[] {
  if (hosts === null) return ["GET /api/v1/resolume/health did not answer"];
  if (arenaRunning === false) return [];
  return hosts
    .filter((host) => !reached(host))
    .map(
      (host) =>
        `SongPlayer reaches no Resolume at ${host.host}: ` +
        `${host.consecutive_failures} refresh(es) failed in a row` +
        (host.circuit_breaker_open ? ", circuit breaker open" : "") +
        (host.last_refresh_ts ? `, last try ${host.last_refresh_ts}` : ", never refreshed"),
    );
}

/** One line for the run's log. */
export function reachSummary(hosts: ResolumeHostHealth[] | null, arenaRunning: boolean | null): string {
  if (hosts === null) return "no answer";
  const arena = arenaRunning === null ? "unknown" : arenaRunning ? "running" : "not running";
  const each = hosts.map(
    (h) =>
      `${h.host} ${reached(h) ? "reached" : "NOT reached"} (failures ${h.consecutive_failures}, ` +
      `breaker ${h.circuit_breaker_open ? "open" : "closed"}, latency ${h.product_latency_ms ?? "—"} ms)`,
  );
  return `Arena ${arena}; ${hosts.length} host(s)${each.length ? ": " + each.join("; ") : ""}`;
}

/** One read of both facts the verdict needs. */
export interface ReachRead {
  hosts: ResolumeHostHealth[] | null;
  arenaRunning: boolean | null;
}

/**
 * Polls `read` until the verdict has no failure or `waitMs` has passed,
 * and returns the read the poll ended on (the one judged).
 */
export async function pollReach(
  read: () => Promise<ReachRead>,
  waitMs: number,
  sleep: (ms: number) => Promise<void>,
  stepMs: number = REACH_POLL_MS,
): Promise<ReachRead> {
  let last = await read();
  let waited = 0;
  while (reachFailures(last.hosts, last.arenaRunning).length > 0 && waited < waitMs) {
    await sleep(stepMs);
    waited += stepMs;
    last = await read();
  }
  return last;
}
