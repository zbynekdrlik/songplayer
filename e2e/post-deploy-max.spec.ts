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

async function readMax(request: APIRequestContext): Promise<MaxStatus> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  const body = (await resp.json()) as { max: MaxStatus };
  return body.max;
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
    const first = await readMax(request);
    console.log(`[#223 max] first: ${JSON.stringify(first)}`);

    await expect
      .poll(async () => (await readMax(request)).submitted - first.submitted, {
        message: `${MIN_BOUNDARIES} more boundaries go out (one grid second)`,
        timeout: 20_000,
      })
      .toBeGreaterThanOrEqual(MIN_BOUNDARIES);
    const second = await readMax(request);
    console.log(
      `[#223 max] second: ${JSON.stringify(second)}; p99 upload+draw+send = ` +
        `${second.upload_us_p99 + second.draw_us_p99 + second.send_us_p99} us`,
    );
    expect(maxGateFailures(first, second), "the SP-program-MAX gate").toEqual([]);
  });
});
