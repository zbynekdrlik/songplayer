/**
 * Post-deploy gate (#343): SongPlayer reaches every Resolume host it drives.
 *
 * Read-only, in SNV's suite and PP's subset. Polls `GET /api/v1/resolume/
 * health` (≤ 60 s: the driver's first reads after the restart) until every
 * host's last refresh was good with its breaker closed; fails naming each
 * host it still does not reach (whether `Arena.exe` runs is only logged:
 * on PP the driven host is `Arena-Bridge.exe`). The verdict is
 * `resolume-reach-gate.ts`.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  REACH_WAIT_MS,
  pollReach,
  reachFailures,
  reachSummary,
  type ReachRead,
  type ResolumeHostHealth,
} from "./resolume-reach-gate";

async function getJson<T>(request: APIRequestContext, path: string): Promise<T | null> {
  try {
    const resp = await request.get(path, { timeout: 10_000 });
    if (resp.status() !== 200) return null;
    return (await resp.json()) as T;
  } catch {
    return null;
  }
}

async function readReach(request: APIRequestContext): Promise<ReachRead> {
  const hosts = await getJson<ResolumeHostHealth[]>(request, "/api/v1/resolume/health");
  const wall = await getJson<{ read?: { arena_running?: boolean | null } }>(request, "/api/v1/wall");
  const running = wall?.read?.arena_running;
  return { hosts, arenaRunning: typeof running === "boolean" ? running : null };
}

test.describe("Resolume reach post-deploy verification (#343)", () => {
  test("SongPlayer reaches every Resolume host it drives", async ({
    page,
    request,
  }) => {
    test.setTimeout(REACH_WAIT_MS + 60_000);
    const last = await pollReach(
      () => readReach(request),
      REACH_WAIT_MS,
      (ms) => page.waitForTimeout(ms),
    );
    console.log(`[#343 reach] ${reachSummary(last.hosts, last.arenaRunning)}`);
    expect(reachFailures(last.hosts), "SongPlayer's reach of Resolume").toEqual([]);
  });
});
