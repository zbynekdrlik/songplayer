/**
 * #233 post-deploy gates on SNV:
 * 1. FOH's path is unchanged by the output list: the migrated VBAN output to
 *    fohabl.lan:6980 runs at 96 kHz INT24 (the owner's ruling of 11.10.2026,
 *    #233; 48 kHz before), stream `sp-program`, no delay,
 *    blocks going out, no send errors (two reads, `fohPathFailures`).
 * 2. A VBAN destination at 96 kHz: a receiver ON THE BOX (a UDP socket on
 *    127.0.0.1) reads rate index 4, 200-frame INT24 packets, a contiguous
 *    counter, 480 a second. The test adds a temporary entry `e2e-96k` and
 *    restores the stored list in `finally`; the FOH entries are unchanged by
 *    both PATCHes, so the outputs task keeps them running (Task 1.8).
 * 3. #257: no audio path records a `loss` in the audio fault ledger from
 *    the moment FOH is listed to the file's end (`audio-faults-gate.ts`);
 *    its `risk` rows are logged.
 *
 * Read-only otherwise. No sleeps: every wait is an expect.poll. The 96 kHz
 * test reads the settings (to restore them), so the file records no trace.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import * as dgram from "node:dgram";
import { FOH_TARGET, MIN_BLOCKS, fohPathFailures, receiverFailures, type OutputStatus } from "./audio-outputs-gate";
import {
  FAULTS_URL,
  LEDGER_SETTLE_MS,
  audioFaultFailures,
  audioFaultRisks,
  type FaultsAnswer,
} from "./audio-faults-gate";

async function outputs(request: APIRequestContext): Promise<OutputStatus[]> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  return (await resp.json()).outputs as OutputStatus[];
}

// The 96 kHz test reads GET /api/v1/settings (every secret masked since
// #229) to restore the list: no trace, so a failure never uploads that body
// (`post-deploy-program-state.md`; a trace option is per file, not per group).
test.use({ trace: "off" });

/** #257: the audio fault ledger's answer over the last hour, read strictly. */
async function readFaults(request: APIRequestContext): Promise<FaultsAnswer> {
  const resp = await request.get(FAULTS_URL, { timeout: 10_000 });
  expect(resp.status(), `GET ${FAULTS_URL}`).toBe(200);
  return (await resp.json()) as FaultsAnswer;
}

/** #257: where this file's audio window opened (the server's clock), once
 * its outputs run; `null` until then. */
let faultWindowStart: number | null = null;

function fohBlocks(list: OutputStatus[]): number {
  return list.find((o) => o.vban?.targets[0]?.target === FOH_TARGET)?.blocks_sent ?? -1;
}

test.describe("audio outputs (#233)", () => {
  test("FOH gets 96 kHz INT24 sp-program", async ({ request }) => {
    test.setTimeout(45_000);
    // Right after a restart FOH is listed once the outputs task's first pass
    // ran (the migration, every entry's DNS): poll for it. A read that throws
    // counts as "not yet" (expect.poll does not retry a throwing generator).
    await expect
      .poll(
        async () => {
          try {
            return fohBlocks(await outputs(request));
          } catch {
            return -1;
          }
        },
        { message: `a VBAN output to ${FOH_TARGET}`, timeout: 20_000 },
      )
      .toBeGreaterThanOrEqual(0);
    // #257: the audio window opens once FOH is listed.
    faultWindowStart = (await readFaults(request)).now_ms;
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
    // The box has a stored list (the migration ran). Without one the probe's
    // PATCH would store a list, and the migration then never runs (it acts
    // only while no list is stored): FOH would stay off for good.
    const stored = (await (await request.get("/api/v1/settings")).json()).audio_outputs as string | undefined;
    expect(stored?.trim(), "a stored output list").toBeTruthy();
    const kept = (JSON.parse(stored as string) as { id: string }[]).filter((e) => !e.id.startsWith("e2e-"));
    // 480 datagrams a second on loopback: a 4 MiB receive buffer, so a busy
    // runner never drops one and fakes a counter jump.
    const socket = dgram.createSocket({ type: "udp4", recvBufferSize: 4 << 20 });
    const packets: Uint8Array[] = [];
    socket.on("message", (m) => packets.push(new Uint8Array(m)));
    await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", () => resolve()));
    const port = socket.address().port;
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
      // Restore from the list stored NOW (an operator may have saved one
      // meanwhile), without the probe; the snapshot only if it cannot be read.
      let current = kept;
      try {
        const now = (await (await request.get("/api/v1/settings")).json()).audio_outputs as string | undefined;
        if (now && now.trim()) current = JSON.parse(now) as { id: string }[];
      } catch {
        current = kept;
      }
      const restore = JSON.stringify(current.filter((e) => !e.id.startsWith("e2e-")));
      const back = await request.patch("/api/v1/settings", { data: { audio_outputs: restore } });
      try {
        // #257: the probe output stops before its receiver closes: a send to
        // a closed port is a send error, a loss on the audio fault ledger.
        if (back.status() === 204) {
          const probeRuns = async () => {
            try {
              return (await outputs(request)).some((o) => o.id.startsWith("e2e-"));
            } catch {
              return true;
            }
          };
          await expect
            .poll(probeRuns, { message: "the probe output is gone (the list is re-read every 5 s)", timeout: 20_000 })
            .toBe(false);
        }
      } finally {
        socket.close();
      }
      expect(back.status(), "the stored list is restored").toBe(204);
    }
  });

  test("no audio path lost a sample in this file's window (#257)", async ({ request }) => {
    test.setTimeout(60_000);
    expect(faultWindowStart, "the window opened (its outputs ran)").not.toBeNull();
    const start = faultWindowStart as number;
    await new Promise((resolve) => setTimeout(resolve, LEDGER_SETTLE_MS));
    const answer = await readFaults(request);
    for (const line of audioFaultRisks(answer, start)) console.log(`[#257 faults] risk: ${line}`);
    expect(audioFaultFailures(answer, start), "a loss row of any audio path in the window").toEqual([]);
  });
});
