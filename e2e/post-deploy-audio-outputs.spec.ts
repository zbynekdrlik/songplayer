/**
 * #233 post-deploy gates on SNV:
 * 1. FOH's path is unchanged by the output list: the migrated VBAN output to
 *    fohabl.lan:6980 runs at 48 kHz INT24, stream `sp-program`, no delay,
 *    blocks going out, no send errors (two reads, `fohPathFailures`).
 * 2. A VBAN destination at 96 kHz: a receiver ON THE BOX (a UDP socket on
 *    127.0.0.1) reads rate index 4, 200-frame INT24 packets, a contiguous
 *    counter, 480 a second. The test adds a temporary entry `e2e-96k` and
 *    restores the stored list in `finally`; the FOH entries are unchanged by
 *    both PATCHes, so the outputs task keeps them running (Task 1.8).
 *
 * Read-only otherwise. No sleeps: every wait is an expect.poll. The 96 kHz
 * test reads the settings (to restore them), so the file records no trace.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import * as dgram from "node:dgram";
import { FOH_TARGET, MIN_BLOCKS, fohPathFailures, receiverFailures, type OutputStatus } from "./audio-outputs-gate";

async function outputs(request: APIRequestContext): Promise<OutputStatus[]> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  return (await resp.json()).outputs as OutputStatus[];
}

// The 96 kHz test reads GET /api/v1/settings (every secret masked since
// #229) to restore the list: no trace, so a failure never uploads that body
// (`post-deploy-program-state.md`; a trace option is per file, not per group).
test.use({ trace: "off" });

function fohBlocks(list: OutputStatus[]): number {
  return list.find((o) => o.vban?.targets[0]?.target === FOH_TARGET)?.blocks_sent ?? -1;
}

test.describe("audio outputs (#233)", () => {
  test("FOH still gets 48 kHz INT24 sp-program", async ({ request }) => {
    test.setTimeout(30_000);
    const first = await outputs(request);
    console.log(`[#233 outputs] first: ${JSON.stringify(first)}`);
    const start = fohBlocks(first);
    await expect
      .poll(async () => fohBlocks(await outputs(request)) - start, {
        message: `${MIN_BLOCKS} FOH blocks go out`,
        timeout: 10_000,
      })
      .toBeGreaterThanOrEqual(MIN_BLOCKS);
    const second = await outputs(request);
    console.log(`[#233 outputs] second: ${JSON.stringify(second)}`);
    expect(fohPathFailures(first, second), "the FOH path").toEqual([]);
  });

  test("a 96 kHz VBAN destination reads index 4 and a contiguous counter", async ({ request }) => {
    test.setTimeout(60_000);
    const socket = dgram.createSocket("udp4");
    const packets: Uint8Array[] = [];
    socket.on("message", (m) => packets.push(new Uint8Array(m)));
    await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", () => resolve()));
    const port = socket.address().port;
    const stored = (await (await request.get("/api/v1/settings")).json()).audio_outputs as string | undefined;
    const kept = (JSON.parse(stored && stored.trim() ? stored : "[]") as { id: string }[]).filter(
      (e) => !e.id.startsWith("e2e-"),
    );
    const restore = JSON.stringify(kept);
    const withProbe = [
      ...kept,
      {
        id: "e2e-96k",
        name: "E2E 96 kHz",
        type: "vban",
        enabled: true,
        rate: 96000,
        delay_ms: 0,
        vban: { host: "127.0.0.1", port, stream_name: "sp-e2e-96k", format: "int24" },
      },
    ];
    try {
      const add = await request.patch("/api/v1/settings", { data: { audio_outputs: JSON.stringify(withProbe) } });
      expect(add.status(), "the probe entry is accepted").toBe(204);
      await expect
        .poll(() => packets.length, {
          message: "the 96 kHz output starts (the list is re-read every 5 s)",
          timeout: 20_000,
        })
        .toBeGreaterThan(0);
      packets.length = 0;
      await expect
        .poll(() => packets.length, { message: "one second of 96 kHz packets", timeout: 10_000 })
        .toBeGreaterThanOrEqual(480);
      const taken = packets.slice(0, 480);
      expect(
        receiverFailures(taken, { srIndex: 4, frames: 200, formatBit: 0x02, stream: "sp-e2e-96k", minPackets: 480 }),
      ).toEqual([]);
    } finally {
      const back = await request.patch("/api/v1/settings", { data: { audio_outputs: restore } });
      socket.close();
      expect(back.status(), "the stored list is restored").toBe(204);
    }
  });
});
