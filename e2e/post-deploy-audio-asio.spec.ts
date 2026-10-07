/**
 * #233 lane 3 post-deploy ASIO gate (SNV: post-deploy.config.ts; PP:
 * post-deploy-pp.config.ts), read-only:
 * 1. The box has Dante Virtual Soundcard registered as an ASIO driver
 *    (`GET /api/v1/audio/asio-drivers`, a registry read; no driver loaded).
 * 2. Exactly SP_ASIO_OUTPUTS_EXPECTED enabled ASIO outputs exist (ci.yml /
 *    deploy-pp.yml; "0" until the main session adds the box's DVS entry,
 *    then "1"), and each one runs, then holds a minute of program blocks at
 *    its driver's rate with no underrun, no reopen, |ppm| <= 300 and a
 *    latency (`asioGateFailures`).
 *
 * No sleeps: every wait is an expect.poll. Right after a deploy the outputs
 * task applies the list on its first pass and a worker opens its driver
 * within a second or two, so the count, the running state and the blocks
 * are polled; a poll's read never throws (`expect.poll` does not retry a
 * generator that throws, `post-deploy-program-state.md`).
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import { DVS_DRIVER, WINDOW_BLOCKS, asioGateFailures, type OutputStatus } from "./audio-outputs-gate";

const EXPECTED = Number(process.env.SP_ASIO_OUTPUTS_EXPECTED ?? "0");

/** `outputs[]` now, or null when the read failed (a poll tries again). */
async function readOutputs(request: APIRequestContext): Promise<OutputStatus[] | null> {
  try {
    const resp = await request.get("/api/v1/program", { timeout: 10_000 });
    if (resp.status() !== 200) return null;
    return (await resp.json()).outputs as OutputStatus[];
  } catch {
    return null;
  }
}

/** One output, read strictly (a failed read fails the test). */
async function output(request: APIRequestContext, id: string): Promise<OutputStatus> {
  const list = await readOutputs(request);
  expect(list, "GET /api/v1/program").not.toBeNull();
  const o = (list as OutputStatus[]).find((x) => x.id === id);
  expect(o, `output ${id}`).toBeDefined();
  return o as OutputStatus;
}

test.describe("ASIO output (#233)", () => {
  test("the box has Dante Virtual Soundcard as an ASIO driver", async ({ request }) => {
    const resp = await request.get("/api/v1/audio/asio-drivers");
    expect(resp.status(), "GET /api/v1/audio/asio-drivers").toBe(200);
    const drivers = (await resp.json()).drivers as string[];
    console.log(`[#233 asio] drivers: ${JSON.stringify(drivers)}`);
    expect(drivers).toContain(DVS_DRIVER);
  });

  test("every enabled ASIO output runs at its driver's rate with no underrun over a minute", async ({
    request,
  }) => {
    test.setTimeout(240_000);
    expect(Number.isInteger(EXPECTED) && EXPECTED >= 0, "SP_ASIO_OUTPUTS_EXPECTED").toBe(true);
    // A failed read is no count (never zero outputs: with EXPECTED "0" a
    // broken API would pass), so the poll reads -1 then and tries again.
    const enabledAsio = async () =>
      (await readOutputs(request))?.filter((o) => o.type === "asio" && o.enabled) ?? null;
    await expect
      .poll(async () => (await enabledAsio())?.length ?? -1, {
        message: "enabled ASIO outputs (SP_ASIO_OUTPUTS_EXPECTED)",
        timeout: 20_000,
      })
      .toBe(EXPECTED);
    const enabled = await enabledAsio();
    expect(enabled, "GET /api/v1/program").not.toBeNull();
    for (const listed of enabled as OutputStatus[]) {
      const read = async () => (await readOutputs(request))?.find((o) => o.id === listed.id);
      await expect
        .poll(async () => (await read())?.state ?? "unread", {
          message: `ASIO output ${listed.id} runs`,
          timeout: 30_000,
        })
        .toBe("running");
      const first = await output(request, listed.id);
      console.log(`[#233 asio] first: ${JSON.stringify(first)}`);
      await expect
        .poll(async () => ((await read())?.blocks_sent ?? first.blocks_sent) - first.blocks_sent, {
          message: `ASIO output ${first.id}: one minute of program blocks`,
          timeout: 120_000,
        })
        .toBeGreaterThanOrEqual(WINDOW_BLOCKS);
      const second = await output(request, first.id);
      console.log(`[#233 asio] second: ${JSON.stringify(second)}`);
      if (second.note) console.log(`[#233 asio] note: ${second.note}`);
      expect(asioGateFailures(first, second), `ASIO output ${first.id}`).toEqual([]);
    }
  });
});
