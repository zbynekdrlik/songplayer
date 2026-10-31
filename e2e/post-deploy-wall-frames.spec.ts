/**
 * #272 lane 2: the post-deploy WALL FRAME gate (SNV). The owner
 * (16.10.2026): "hlavne zlepsuj svoj e2e ci test ktory zlyha ked je tento
 * problem … nemalo by to prejst cez ziaden ci beh".
 *
 * Every other gate reads SongPlayer's own counters. None looked at the
 * picture Arena puts on the LED wall, so deploys went green while the wall
 * juddered. This spec plays SongPlayer's frame-ID wall clips on program
 * (the test item's `wall-30`, then `wall-25`), reads them back from Arena's
 * Spout output, the picture wall-bridge takes for the wall
 * (`scripts/wall_frames.py --spout`), and fails on ANY lost frame,
 * jump back, judder (a boundary held the wrong number of refreshes), blend
 * weight off the pacer's rule or frozen wall (`wall-frames-gate.ts`,
 * unit-tested in the mock suite). An incomplete or failed capture fails
 * too, named as such: the gate never passes without positive evidence.
 *
 * What it sees is Arena's output BEFORE wall-bridge, NVIDIA Surround and the
 * LED processor (owner, 30.10.2026, #272: the wall is off the Windows desktop
 * since 29.10., desktop duplication no longer sees it). A clean take covers
 * only its span (the judder comes in waves): it is a regression check, never
 * proof of a frame-exact path (`test-item-burn.md`).
 *
 * It needs Arena's wall clip on SongPlayer's program (`SP-program-MAX`, or
 * NDI `SP-program`): a wall showing another source reads nothing and fails
 * by name. It restores the program source it found, then leaves camera-box's
 * clip as the test item's remembered clip (`/start` + `/stop`, no body).
 */

import { test, expect, APIRequestContext } from "@playwright/test";
import { spawnSync } from "child_process";
import * as path from "path";
import {
  ArenaLayer,
  EXIT_NOTHING_DECODED,
  WallFramesReport,
  coveringLayers,
  wallFramesFailures,
} from "./wall-frames-gate";

/** The e2e venv's Python (numpy + SpoutGL, ci.yml "Prepare the A/V gate's own Python"). */
const PYTHON =
  process.env.SP_WALL_PYTHON ||
  process.env.SP_AVSYNC_PYTHON ||
  "C:\\ProgramData\\SongPlayer\\e2e\\avsync_venv\\Scripts\\python.exe";
/** The runner's checkout of the reader. */
const READER = path.resolve(__dirname, "..", "scripts", "wall_frames.py");
/** Arena's Spout sender of the wall screen (`snv surround 7680`'s Screen 2). */
const SPOUT_SENDER = process.env.SP_WALL_SPOUT_SENDER || "Arena - Screen 2";
/** An optional region `L,T,R,B` of the sender's picture, a manual override.
 *  #272 v2: with none, the reader finds the clip's copies on the whole
 *  picture and reads only their strip rows and columns, so a change of
 *  Arena's wall mapping needs no new rows here. */
const REGION = process.env.SP_WALL_REGION || "";
/** Arena renders on the primary display's 60.000 Hz (`gpu-max.md`). */
const REFRESH_HZ = "60";
/** Seconds captured per clip: ~1200 boundaries, 4× the reader's floor. */
const TAKE_S = 40;
/** The clips, in order: one picture per boundary, then the #251 blend. */
const CLIPS: Array<{ name: string; fps: number; row: string }> = [
  { name: "wall-30", fps: 30, row: "wallclip-30" },
  { name: "wall-25", fps: 25, row: "wallclip-25" },
];

interface OnAirItem {
  playlist_id: number;
  youtube_id: string;
  position_ms: number;
}

interface ProgramRead {
  source: number | null;
  on_air_item: OnAirItem | null;
}

interface TestItemRead {
  imported: boolean;
  item: { playlist_id: number } | null;
  clips: Array<{ name: string; imported: boolean }>;
}

async function program(request: APIRequestContext): Promise<ProgramRead> {
  const resp = await request.get("/api/v1/program");
  expect(resp.status(), "GET /api/v1/program").toBe(200);
  return (await resp.json()) as ProgramRead;
}

async function cut(request: APIRequestContext, source: number): Promise<void> {
  const resp = await request.post("/api/v1/program/cut", { data: { source } });
  expect(resp.status(), `POST /api/v1/program/cut ${source}: ${await resp.text()}`).toBe(200);
}

/** Wait until `row` is on program and has played `ms`; bounded. */
async function waitOnAir(request: APIRequestContext, row: string, ms: number): Promise<void> {
  const deadline = Date.now() + 25_000;
  let last = "";
  while (Date.now() < deadline) {
    const item = (await program(request)).on_air_item;
    last = item ? `${item.youtube_id} at ${item.position_ms} ms` : "nothing";
    if (item && item.youtube_id === row && item.position_ms >= ms) {
      return;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`the clip ${row} did not play on program within 25 s (on air: ${last})`);
}

/** One Spout take of `fps`'s clip: the reader's exit code and report. */
function take(fps: number): { code: number | null; report: WallFramesReport | null; stderr: string } {
  const args = [
    READER,
    "--fps",
    String(fps),
    "--spout",
    SPOUT_SENDER,
    "--refresh-hz",
    REFRESH_HZ,
    "--seconds",
    String(TAKE_S),
  ];
  if (REGION) {
    args.push("--region", REGION);
  }
  const run = spawnSync(PYTHON, args, {
    cwd: path.dirname(READER),
    encoding: "utf8",
    timeout: (TAKE_S + 90) * 1000,
    windowsHide: true,
    maxBuffer: 64 * 1024 * 1024,
  });
  let report: WallFramesReport | null = null;
  try {
    report = JSON.parse(run.stdout) as WallFramesReport;
  } catch {
    report = null;
  }
  return { code: run.status, report, stderr: `${run.error ?? ""}${run.stderr ?? ""}` };
}

/** Arena's REST (read only), the URL `post-deploy.spec.ts` reads. */
const ARENA_COMPOSITION =
  process.env.RESOLUME_REST_URL || "http://127.0.0.1:8090/api/v1/composition";

/** #272 (19.10.2026): what covers the clip when nothing was decoded, from
 *  Arena's composition (`coveringLayers`); never changes it. */
async function whatCovers(): Promise<string> {
  let failed = "";
  // One retry (CI 39345213643: the second clip's read failed with "TypeError:
  // fetch failed" a minute after the first one answered).
  for (let attempt = 1; attempt <= 2; attempt++) {
    try {
      const resp = await fetch(ARENA_COMPOSITION, { signal: AbortSignal.timeout(5000) });
      const composition = (await resp.json()) as { layers?: ArenaLayer[] };
      const above = coveringLayers(composition.layers ?? []);
      return above.length > 0 ? above.join(", ") : "none";
    } catch (err) {
      failed = `${err}`;
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }
  return `Arena's composition could not be read (${failed})`;
}

test.describe.configure({ mode: "serial" });

test.describe("the LED wall shows every frame, in order, on its cadence (#272)", () => {
  let startSource: number | null = null;
  let testPlaylist: number | null = null;
  let started = false;

  test.afterAll(async ({ request }) => {
    test.setTimeout(60_000);
    if (startSource !== null && testPlaylist !== null && startSource !== testPlaylist) {
      await cut(request, startSource);
    }
    if (started) {
      // Leave camera-box's clip as the one a bare `sp-test` press plays.
      await request.post("/api/v1/test-item/start", { data: {} });
      await request.post("/api/v1/test-item/stop");
    }
  });

  test("wall-30 and wall-25 read back frame-exact from Arena's wall output", async ({ request }) => {
    test.setTimeout(CLIPS.length * (TAKE_S + 150) * 1000);
    const item = await request.get("/api/v1/test-item");
    expect(item.status(), "GET /api/v1/test-item").toBe(200);
    const state = (await item.json()) as TestItemRead;
    expect(state.item, "the test item playlist exists").not.toBeNull();
    testPlaylist = state.item!.playlist_id;
    for (const clip of CLIPS) {
      const listed = state.clips.find((c) => c.name === clip.name);
      expect(listed?.imported, `the wall clip ${clip.name} is imported on the box`).toBe(true);
    }
    startSource = (await program(request)).source;

    const failures: string[] = [];
    for (const clip of CLIPS) {
      const start = await request.post("/api/v1/test-item/start", { data: { clip: clip.name } });
      // The route answers 204 No Content (`api/test_item.rs::post_start`).
      expect(start.status(), `POST /api/v1/test-item/start ${clip.name}`).toBe(204);
      started = true;
      if ((await program(request)).source !== testPlaylist) {
        await cut(request, testPlaylist);
      }
      await waitOnAir(request, clip.row, 3_000);
      const result = take(clip.fps);
      const r = result.report;
      console.log(
        `wall frames ${clip.name}: exit ${result.code}, verdict ${r?.verdict ?? "-"}, ` +
          `boundaries ${r?.boundaries ?? "-"}, judged ${r?.refreshes?.judged ?? "-"}, ` +
          `uneven ${r?.refreshes?.uneven ?? "-"} ${JSON.stringify(r?.refreshes?.uneven_boundaries ?? [])}, ` +
          `jumps back ${r?.jumps_back ?? "-"}, ` +
          `lost ${r?.lost_frames ?? "-"}, decoded ${r?.decoded ?? "-"}/${r?.samples ?? "-"}, ` +
          `located ${JSON.stringify(r?.located ?? null)}`,
      );
      if (result.stderr.trim()) {
        console.log(`wall frames ${clip.name} stderr: ${result.stderr.trim().slice(0, 2000)}`);
      }
      failures.push(...wallFramesFailures(clip.name, result.code, result.report));
      if (result.code === EXIT_NOTHING_DECODED) {
        failures.push(`${clip.name}: Arena layers above SongPlayer's clip: ${await whatCovers()}`);
      }
    }
    expect(failures, "Arena's wall output must show every clip frame, in order, on its cadence").toEqual(
      [],
    );
  });
});
