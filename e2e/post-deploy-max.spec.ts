/**
 * #223 S2 post-deploy gate: `SP-program-MAX` goes out on the box.
 *
 * SongPlayer composes a fixed 3840×2160 picture of SP-program on the GPU and
 * shares it with Resolume Arena over Spout (the `program-max` thread).
 * Nothing else in CI would go red if that thread died on the box: the
 * program, VBAN and every other output keep working without it. So this spec
 * reads the live telemetry (`GET /api/v1/program` → `max`) twice and applies
 * the gate (`max-gate.ts`, unit-tested in the mock suite): the setting on,
 * the canvas and the Spout name, a hardware adapter, `running`, at least one
 * grid second of boundaries out, and in between none coalesced (MAX kept up
 * with the program), none failed and no device lost.
 *
 * The cost p99s are logged, not gated here: the budget (upload + draw + send
 * under 10 ms) and Arena's side (its source list, a scratch layer's FPS) are
 * the main session's box gate (`.claude/rules/gpu-max.md`). API-level on
 * purpose: MAX has no dashboard surface yet.
 */

import { test, expect, APIRequestContext } from "@playwright/test";
import { MIN_BOUNDARIES, MaxStatus, maxGateFailures } from "./max-gate";

/** The fields of `GET /api/v1/program` this spec reads: MAX, and the
 *  program's own coalesces and late takes (a burst after a program-side
 *  stall can coalesce MAX's 2-deep queue with MAX healthy). */
interface ProgramRead {
  max: MaxStatus;
  health: { coalesced: number; timing: { ready_late_us_max: number } };
}

async function readProgram(request: APIRequestContext): Promise<ProgramRead> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  return (await resp.json()) as ProgramRead;
}

async function readMax(request: APIRequestContext): Promise<MaxStatus> {
  return (await readProgram(request)).max;
}

/** One log line of a read: MAX, plus the program's side of a coalesce. */
function logLine(what: string, read: ProgramRead): string {
  const h = read.health;
  return (
    `[#223 max] ${what}: ${JSON.stringify(read.max)}; program coalesced=${h.coalesced} ` +
    `ready_late_us_max=${h.timing.ready_late_us_max}`
  );
}

test.describe("SP-program-MAX (#223 S2)", () => {
  test("composes on the GPU and keeps up with every boundary", async ({ request }) => {
    test.setTimeout(60_000);

    // The thread builds its compositor and sender at the first boundary
    // after the start: wait for the first one to go out.
    await expect
      .poll(async () => (await readMax(request)).submitted, {
        message: "SP-program-MAX sends its first boundary",
        timeout: 30_000,
      })
      .toBeGreaterThan(0);
    const firstRead = await readProgram(request);
    const first = firstRead.max;
    console.log(logLine("first", firstRead));

    await expect
      .poll(async () => (await readMax(request)).submitted - first.submitted, {
        message: `${MIN_BOUNDARIES} more boundaries go out (one grid second)`,
        timeout: 20_000,
      })
      .toBeGreaterThanOrEqual(MIN_BOUNDARIES);
    const secondRead = await readProgram(request);
    const second = secondRead.max;
    console.log(
      `${logLine("second", secondRead)}; p99 upload+draw+send = ` +
        `${second.upload_us_p99 + second.draw_us_p99 + second.send_us_p99} us; ` +
        `sent after the offer p50/p99/max = ${second.send_at_us_p50}/` +
        `${second.send_at_us_p99}/${second.send_at_us_max} us, late ` +
        `+${second.send_late - first.send_late}`,
    );
    expect(maxGateFailures(first, second), "the SP-program-MAX gate").toEqual([]);
  });
});
