/**
 * #233 lane 3 post-deploy ASIO gate (SNV: post-deploy.config.ts; PP:
 * post-deploy-pp.config.ts), read-only:
 * 1. The driver list reads (`GET /api/v1/audio/asio-drivers`, a registry
 *    read; no driver loaded), and a box expected to run an ASIO output
 *    (SP_ASIO_OUTPUTS_EXPECTED > 0) has Dante Virtual Soundcard among them,
 *    by its exact name. A box with none expected (PP before its DVS work)
 *    is not asked for DVS (#233 release review).
 * 2. Exactly SP_ASIO_OUTPUTS_EXPECTED enabled ASIO outputs exist that are
 *    not waiting for their driver's clock (`gatedAsioOutputs`: PP's DVS waits
 *    calmly while PP has no Dante PTP clock, the owner's ruling, #233,
 *    8.10.2026; ci.yml / deploy-pp.yml: "1" at SNV, "0" at PP until its DVS
 *    has a clock), and each one runs, then holds a minute of program blocks at
 *    its driver's rate with no underrun, no reopen, no hard re-centre
 *    (#233 review round 2: the owner's fault, a faded skip or insert),
 *    |ppm| <= 300 and a latency (`asioGateFailures`). Every output is measured over the SAME
 *    minute, so the gate's time does not grow with the number of outputs
 *    (up to 4, `MAX_ASIO_OUTPUTS`).
 *
 * No sleeps: every wait is an expect.poll. Right after a deploy the outputs
 * task applies the list on its first pass and a worker opens its driver
 * within a second or two, so the count, the running state and the blocks
 * are polled; a poll's read never throws (`expect.poll` does not retry a
 * generator that throws, `post-deploy-program-state.md`).
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  DVS_DRIVER,
  WINDOW_BLOCKS,
  asioGateFailures,
  gatedAsioOutputs,
  type OutputStatus,
} from "./audio-outputs-gate";

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
  test("the driver list reads, with Dante Virtual Soundcard on a box expected to run ASIO", async ({
    request,
  }) => {
    const resp = await request.get("/api/v1/audio/asio-drivers");
    expect(resp.status(), "GET /api/v1/audio/asio-drivers").toBe(200);
    const drivers = (await resp.json()).drivers as string[];
    console.log(`[#233 asio] drivers: ${JSON.stringify(drivers)} (expected outputs: ${EXPECTED})`);
    if (EXPECTED > 0) {
      expect(drivers, "an ASIO output is expected: DVS must be registered").toContain(DVS_DRIVER);
    }
  });

  test("every enabled ASIO output runs at its driver's rate with no underrun over a minute", async ({
    request,
  }) => {
    test.setTimeout(240_000);
    expect(Number.isInteger(EXPECTED) && EXPECTED >= 0, "SP_ASIO_OUTPUTS_EXPECTED").toBe(true);
    // A failed read is no count (never zero outputs: with EXPECTED "0" a
    // broken API would pass), so the poll reads -1 then and tries again.
    const enabledAsio = async () => {
      const list = await readOutputs(request);
      return list === null ? null : gatedAsioOutputs(list);
    };
    await expect
      .poll(async () => (await enabledAsio())?.length ?? -1, {
        message: "enabled ASIO outputs (SP_ASIO_OUTPUTS_EXPECTED)",
        timeout: 20_000,
      })
      .toBe(EXPECTED);
    const enabled = await enabledAsio();
    expect(enabled, "GET /api/v1/program").not.toBeNull();
    const ids = (enabled as OutputStatus[]).map((o) => o.id);
    if (ids.length === 0) return;
    // Every output's state in one read ("unread" for a failed read).
    const states = async () => {
      const list = await readOutputs(request);
      return ids.map((id) => list?.find((o) => o.id === id)?.state ?? "unread");
    };
    await expect
      .poll(states, { message: `ASIO outputs ${ids.join(", ")} run`, timeout: 30_000 })
      .toEqual(ids.map(() => "running"));
    const first: OutputStatus[] = [];
    for (const id of ids) first.push(await output(request, id));
    console.log(`[#233 asio] first: ${JSON.stringify(first)}`);
    // The fewest blocks any output sent since its first read (a failed read
    // counts none).
    const fewest = async () => {
      const list = await readOutputs(request);
      return Math.min(
        ...first.map((f) => (list?.find((o) => o.id === f.id)?.blocks_sent ?? f.blocks_sent) - f.blocks_sent),
      );
    };
    await expect
      .poll(fewest, { message: "one minute of program blocks on every ASIO output", timeout: 120_000 })
      .toBeGreaterThanOrEqual(WINDOW_BLOCKS);
    for (const f of first) {
      const second = await output(request, f.id);
      console.log(`[#233 asio] second: ${JSON.stringify(second)}`);
      if (second.note) console.log(`[#233 asio] note: ${second.note}`);
      expect(asioGateFailures(f, second), `ASIO output ${f.id}`).toEqual([]);
    }
  });
});
