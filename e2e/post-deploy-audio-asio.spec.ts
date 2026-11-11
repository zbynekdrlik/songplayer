/**
 * #233 lane 3 post-deploy ASIO gate (SNV: post-deploy.config.ts; PP:
 * post-deploy-pp.config.ts), read-only:
 * 1. The driver list reads (`GET /api/v1/audio/asio-drivers`, a registry
 *    read; no driver loaded), and a box expected to run an ASIO output
 *    (SP_ASIO_OUTPUTS_EXPECTED > 0) has Dante Virtual Soundcard among them,
 *    by its exact name. A box with none expected is not asked for DVS
 *    (#233 release review).
 * 2. Exactly SP_ASIO_OUTPUTS_EXPECTED enabled ASIO outputs exist
 *    (`asioCount`; ci.yml / deploy-pp.yml: "1" at SNV, "2" at PP). One in a
 *    calm wait for its driver's clock (`no_clock` / `clock_off_rate`: its
 *    device or network is off, e.g. PP's SoundGrid outside services; #229,
 *    ROZHODNUTÉ 11.11.2026) counts, is named in the log and is not measured;
 *    every other one (`gatedAsioOutputs`) runs, then holds a minute of program blocks at
 *    its driver's rate with no underrun, no reopen, no hard re-centre
 *    (#233 review round 2: the owner's fault, a faded skip or insert),
 *    |ppm| <= 300 and a latency (`asioGateFailures`). Every output is measured over the SAME
 *    minute, so the gate's time does not grow with the number of outputs
 *    (up to 4, `MAX_ASIO_OUTPUTS`).
 *
 * 3. #233 (the spawn finding): a child process SongPlayer spawns (one yt-dlp,
 *    `POST /api/v1/youtube/probe`; skipped when no output runs, every
 *    expected one in a calm clock wait) moves no running ASIO output's `resyncs` or
 *    `hard_recentres`, and no output stops or restarts: every ASIO output
 *    runs in the ASIO host process `sp-audio-host`, which spawns nothing
 *    (`asio-spawn-gate.ts`, unit-tested in the mock suite).
 * 4. #257: no audio path (the program, every output, a loop check, the NDI
 *    input, the playlist pipelines) records a `loss` in the audio fault
 *    ledger from one settle after every gated ASIO output runs to the file's
 *    end, read from `GET /api/v1/audio/faults` after another settle
 *    (`audio-faults-read.ts`; the decision `audio-faults-gate.ts`,
 *    unit-tested in the mock suite); every path and its `risk` rows are
 *    logged first, also when the window never opened.
 *
 * No sleeps but three: every wait is an expect.poll, except the bounded
 * settle after the spawn (`SETTLE_AFTER_SPAWN_MS`, 5 s: the absence of a
 * resync cannot be polled for) and the ledger's two settles
 * (`LEDGER_SETTLE_MS`, 11 s, idem). Right after a deploy the outputs
 * task applies the list on its first pass and a worker opens its driver
 * within a second or two, so the count, the running state and the blocks
 * are polled; a poll's read never throws (`expect.poll` does not retry a
 * generator that throws, `post-deploy-program-state.md`).
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  DVS_DRIVER,
  WINDOW_BLOCKS,
  asioCount,
  asioCountReading,
  asioGateFailures,
  type OutputStatus,
} from "./audio-outputs-gate";
import {
  SETTLE_AFTER_SPAWN_MS,
  probeSpawnFailures,
  spawnCounters,
  spawnDisturbances,
  spawnReady,
} from "./asio-spawn-gate";
import { expectNoLossSince, fileStartOrNull, openFaultWindow } from "./audio-faults-read";

const EXPECTED = Number(process.env.SP_ASIO_OUTPUTS_EXPECTED ?? "0");
/** #229 review F1: whether an output in a calm clock wait counts toward
 *  EXPECTED — "1" only in deploy-pp.yml (PP's SoundGrid network is off
 *  outside services); on resolume-snv a Dante / DVS output with no clock fails. */
const CLOCK_WAIT_OK = process.env.SP_ASIO_CLOCK_WAIT_OK === "1";

/** #257: where this file's audio window opened (the server's clock), one
 * settle after its outputs run; `null` until then. */
let faultWindowStart: number | null = null;

/** #257: the server's clock when this worker began the file: the window's
 * fallback start when an earlier test failed before it opened. */
let fileStart: number | null = null;

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
  test.beforeAll(async ({ request }) => {
    fileStart = await fileStartOrNull(request);
  });

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
    // The count (#229: an output in a calm clock wait counts, unmeasured),
    // or, when it is off, the count with the measured and the waiting
    // outputs named. A failed read is no count (never zero outputs: with
    // EXPECTED "0" a broken API would pass), so the poll reads -1 then and
    // tries again. The list the count came from is kept: the outputs the
    // test measures are taken from that same read.
    let countedList: OutputStatus[] = [];
    const counted = async () => {
      const list = await readOutputs(request);
      if (list === null) return -1;
      countedList = list;
      return asioCountReading(list, EXPECTED, CLOCK_WAIT_OK);
    };
    await expect
      .poll(counted, {
        message: "enabled ASIO outputs (SP_ASIO_OUTPUTS_EXPECTED)",
        timeout: 20_000,
      })
      .toBe(EXPECTED);
    const count = asioCount(countedList, CLOCK_WAIT_OK);
    console.log(
      `[#233 asio] counted ${count.counted}: measured ${JSON.stringify(count.measured)}, ` +
        `waiting for a clock (not measured) ${JSON.stringify(count.waiting)}`,
    );
    const ids = count.measured;
    if (ids.length === 0) {
      faultWindowStart = await openFaultWindow(request);
      return;
    }
    // Every output's state in one read ("unread" for a failed read).
    const states = async () => {
      const list = await readOutputs(request);
      return ids.map((id) => list?.find((o) => o.id === id)?.state ?? "unread");
    };
    await expect
      .poll(states, { message: `ASIO outputs ${ids.join(", ")} run`, timeout: 30_000 })
      .toEqual(ids.map(() => "running"));
    // #257: the audio window opens one settle after every gated ASIO output
    // runs (a fault from before is stamped before it).
    faultWindowStart = await openFaultWindow(request);
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

  test("a child process SongPlayer spawns disturbs no ASIO output (#233)", async ({ request }) => {
    test.setTimeout(240_000);
    // Every gated output running or in a calm clock wait (#229), read
    // strictly (a failed read is no count).
    const ready = async () => {
      const list = await readOutputs(request);
      return list === null ? -1 : spawnReady(list, CLOCK_WAIT_OK);
    };
    await expect
      .poll(ready, { message: "the ASIO outputs run, or wait calmly for a clock, before the spawn", timeout: 30_000 })
      .toBe(EXPECTED);
    const first = await readOutputs(request);
    expect(first, "GET /api/v1/program").not.toBeNull();
    const before = spawnCounters(first as OutputStatus[]);
    console.log(`[#233 spawn] before: ${JSON.stringify(before)}`);
    // Only the running outputs are watched across the spawn.
    if (before.length === 0) return;
    // One yt-dlp, spawned by SongPlayer (the probe answers 200 whatever
    // YouTube says: the spawn is what the gate needs — unless yt-dlp is not
    // installed yet, when SongPlayer spawned nothing and the gate proves
    // nothing).
    const probe = await request.post("/api/v1/youtube/probe", { timeout: 180_000 });
    expect(probe.status(), "POST /api/v1/youtube/probe").toBe(200);
    const answer = await probe.json();
    console.log(`[#233 spawn] probe: ${JSON.stringify(answer)}`);
    expect(probeSpawnFailures(answer), "the probe spawned a child").toEqual([]);
    await new Promise((resolve) => setTimeout(resolve, SETTLE_AFTER_SPAWN_MS));
    const second = await readOutputs(request);
    expect(second, "GET /api/v1/program").not.toBeNull();
    const after = spawnCounters(second as OutputStatus[]);
    console.log(`[#233 spawn] after: ${JSON.stringify(after)}`);
    expect(spawnDisturbances(before, after), "the ASIO outputs across a SongPlayer-spawned child").toEqual([]);
  });

  test("no audio path lost a sample in this file's window (#257)", async ({ request }) => {
    test.setTimeout(60_000);
    await expectNoLossSince(request, faultWindowStart, fileStart);
  });
});
