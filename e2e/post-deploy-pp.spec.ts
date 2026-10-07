/**
 * #229: the post-deploy subset at the PP site (resolume-pp), run by
 * .github/workflows/deploy-pp.yml after every main release
 * (post-deploy-pp.config.ts):
 *  - the dashboard shows the deployed version, with a clean console;
 *  - PP keeps its own identity (`node_name` `pp`, in PP's DB, which no
 *    deploy writes) and reads its peer `snv` through Cloudflare with a
 *    service token (`peer-probe-gate.ts::peerSetupFailures`);
 *  - PP reads SNV's catalog live through Cloudflare
 *    (`POST /api/v1/exchange/probe`, `probeFailures`);
 *  - PP transfers SNV's smallest file artifact through Cloudflare with its
 *    own client and reads back its byte count and sha256
 *    (`POST /api/v1/exchange/probe/transfer`, `transferFailures`);
 *  - a playlist pressed through SongPlayer's facade (Companion's path) plays
 *    on SongPlayer's program, and SP-program has a receiver;
 *  - a manual scene pressed through the facade reaches the program as "OBS
 *    manuál" (source -1), cg OBS accepting it.
 * SP-program-MAX is gated by post-deploy-max.spec.ts in the same run. The A/V
 * gate comes later, once PP records.
 *
 * The scenes come from `pp-scenes.ts` (the playlist scene from SongPlayer's
 * own catalog; the manual scene = cg OBS's own program scene unless the repo
 * variable PP_MANUAL_SCENE names one). `afterAll` puts SP-program back on its
 * start source with a dashboard cut (it tells cg OBS nothing) only while the
 * program is still what the gate left (`programRestoreTarget`), and cg OBS
 * back on its own only when the manual press moved it and it is still there
 * (`cgRestoreTarget`; through the facade when SP-program stays on "OBS
 * manuál", `cgRestoreVia`): an operator's press meanwhile is kept. The
 * tests are not serial: a missing Cloudflare token must not hide the playback
 * results.
 */
import {
  test,
  expect,
  request as apiRequest,
  type APIRequestContext,
} from "@playwright/test";
import { healthRow } from "./box-api";
import { ObsDriver } from "./obs-driver";
import {
  peerSetupFailures,
  probeFailures,
  transferFailures,
  type ExchangeStatusView,
  type ProbeResult,
  type TransferProbe,
} from "./peer-probe-gate";
import {
  cgRestoreTarget,
  cgRestoreVia,
  gateIsLatest,
  isManualScene,
  manualCutLanded,
  pickManualScene,
  pickPlaylistScene,
  playlistNames,
  programRestoreTarget,
  type GatePress,
  type LastCutView,
  type ManualCutView,
} from "./pp-scenes";
import { readEngineActiveScene, type PlaylistRow } from "./program-state";

const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";
const FACADE_WS_URL = process.env.FACADE_WS_URL || "ws://localhost:4456";
const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";
const EXPECTED_VERSION = (process.env.SP_EXPECTED_VERSION || "").trim();
const MANUAL_SCENE = (process.env.PP_MANUAL_SCENE || "").trim();
const NODE = "pp";
const PEER = "snv";
const PEER_HOST = "sp.newlevel.media";

/** A bounded read that never throws (`expect.poll` aborts on a throw). */
async function getJson<T>(request: APIRequestContext, url: string): Promise<T | null> {
  try {
    const resp = await request.get(url, { timeout: 10_000 });
    return resp.ok() ? ((await resp.json()) as T) : null;
  } catch {
    return null;
  }
}

async function readPlaylists(request: APIRequestContext): Promise<PlaylistRow[]> {
  const resp = await request.get("/api/v1/playlists");
  expect(resp.status(), "GET /api/v1/playlists").toBe(200);
  return (await resp.json()) as PlaylistRow[];
}

/** A playlist's playable videos: downloaded and normalized (the rows of a
 *  synced but not yet downloaded song are listed too). */
async function playableCount(request: APIRequestContext, pid: number): Promise<number> {
  const resp = await request.get(`/api/v1/playlists/${pid}/videos`);
  expect(resp.status(), `GET /api/v1/playlists/${pid}/videos`).toBe(200);
  const videos = (await resp.json()) as { normalized: boolean }[];
  return videos.filter((v) => v.normalized).length;
}

/** `GET /api/v1/program`, the fields the restore reads. */
interface ProgramNow {
  source: number | null;
  remote: { last_remote_cut: LastCutView | null };
}

test.describe("PP post-deploy (#229)", () => {
  test("the dashboard shows the deployed version", async ({ page, request }) => {
    const consoleMessages: string[] = [];
    page.on("console", (msg) => {
      // Chromium's benign SRI warning on the preloaded WASM bundle
      // (crbug.com/981419), filtered by every other dashboard spec.
      if (/integrity.*attribute.*ignored/i.test(msg.text())) return;
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });
    expect(EXPECTED_VERSION, "deploy-pp.yml sets SP_EXPECTED_VERSION").not.toBe("");
    const status = await getJson<{ version: string }>(request, "/api/v1/status");
    expect(status?.version, "GET /api/v1/status → version").toBe(EXPECTED_VERSION);
    await page.goto("/");
    await expect(page.locator('[data-testid="version"]')).toHaveText(`v${EXPECTED_VERSION}`, {
      timeout: 30_000,
    });
    expect(consoleMessages, "a clean browser console").toEqual([]);
  });

  test("PP keeps its own identity and reads snv through Cloudflare with a token", async ({
    request,
  }) => {
    const s = await getJson<ExchangeStatusView>(request, "/api/v1/exchange/status");
    expect(s, "GET /api/v1/exchange/status").not.toBeNull();
    // The status names no key and no Cloudflare secret (`peer::lan`).
    console.log(`[#229 pp] exchange: ${JSON.stringify(s)}`);
    expect(peerSetupFailures(s!, NODE, PEER, PEER_HOST)).toEqual([]);
  });

  test("PP reads SNV's catalog live through Cloudflare", async ({ request }) => {
    const resp = await request.post("/api/v1/exchange/probe", { timeout: 60_000 });
    const body = await resp.text();
    expect(resp.status(), `POST /api/v1/exchange/probe: ${body}`).toBe(200);
    const results = JSON.parse(body) as ProbeResult[];
    console.log(`[#229 pp] probe: ${body}`);
    expect(probeFailures(results, PEER, PEER_HOST)).toEqual([]);
  });

  test("PP transfers a real artifact from SNV through Cloudflare", async ({ request }) => {
    // PP's client fetches SNV's smallest file artifact (≤ 64 MiB) into a temp
    // dir outside its cache. It does not wait for the per-peer transfer slot
    // (the workers' transfers queue there), so only that one file's transfer
    // through Cloudflare counts against this bound.
    test.setTimeout(360_000);
    const resp = await request.post("/api/v1/exchange/probe/transfer", { timeout: 330_000 });
    const body = await resp.text();
    expect(resp.status(), `POST /api/v1/exchange/probe/transfer: ${body}`).toBe(200);
    const results = JSON.parse(body) as TransferProbe[];
    console.log(`[#229 pp] transfer probe: ${body}`);
    expect(transferFailures(results, PEER, PEER_HOST)).toEqual([]);
  });
});

test.describe("PP's program through the facade (#229)", () => {
  let facade: ObsDriver | null = null;
  let cg: ObsDriver | null = null;
  /** SP-program's source before the tests (`GET /api/v1/program` →
   *  `source`; null: nothing was on it, so there is nothing to put back). */
  let startSource: number | null = null;
  /** Every facade press the gate sent, in order: afterAll may undo them. */
  const presses: GatePress[] = [];
  /** The manual press moved cg OBS (from → to): afterAll may put it back. */
  let cgMoved: { from: string; to: string } | null = null;

  /** Press `scene` (which puts `source` on program) through the facade,
   *  recorded first: a press that fails half-way is still undone. */
  async function press(scene: string, source: number): Promise<void> {
    presses.push({ scene, sentAtMs: Date.now(), source });
    await facade!.switchScene(scene);
  }

  test.beforeAll(async () => {
    facade = await ObsDriver.connect(FACADE_WS_URL);
    cg = await ObsDriver.connect(OBS_WS_URL);
    const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
    try {
      const program = await getJson<ProgramNow>(ctx, "/api/v1/program");
      expect(program, "GET /api/v1/program before the presses").not.toBeNull();
      startSource = program!.source;
    } finally {
      await ctx.dispose();
    }
    const cgScene = await cg.currentProgramScene();
    console.log(`[#229 pp] at the start: SP-program source ${startSource}, cg OBS "${cgScene}"`);
  });

  test.afterAll(async () => {
    const problems: string[] = [];
    const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
    // What the restore of SP-program decided, for cg OBS's own restore.
    let programBack: number | null = null;
    let latest = false;
    let sourceNow: number | null = null;
    try {
      if (presses.length > 0) {
        try {
          const now = await getJson<ProgramNow>(ctx, "/api/v1/program");
          if (now === null) throw new Error("GET /api/v1/program failed");
          sourceNow = now.source;
          latest = gateIsLatest(presses, now.remote.last_remote_cut);
          const back = programRestoreTarget(startSource, presses, {
            source: now.source,
            last_remote_cut: now.remote.last_remote_cut,
          });
          programBack = back;
          if (back !== null) {
            const resp = await ctx.post("/api/v1/program/cut", { data: { source: back } });
            if (!resp.ok()) {
              throw new Error(`POST /api/v1/program/cut ${back}: ${resp.status()} ${await resp.text()}`);
            }
            console.log(`[#229 pp] SP-program back on source ${back} (was ${now.source})`);
          } else {
            console.log(
              `[#229 pp] SP-program left on source ${now.source} (start ${startSource}, ` +
                `last switch ${JSON.stringify(now.remote.last_remote_cut)})`,
            );
          }
        } catch (e) {
          problems.push(`putting SP-program back on source ${startSource} failed: ${e}`);
        }
      }
      if (cg && cgMoved !== null) {
        try {
          const now = await cg.currentProgramScene();
          const back = cgRestoreTarget(cgMoved, now);
          if (back !== null) {
            const backIsManual = isManualScene(back, playlistNames(await readPlaylists(ctx)));
            const via = cgRestoreVia({
              programBack,
              gateIsLatest: latest,
              source: sourceNow,
              backIsManual,
            });
            if (via === "facade") {
              // SP-program stays on "OBS manual": the press moves cg OBS back
              // and SongPlayer names the program by that scene again.
              await press(back, -1);
            } else {
              await cg.switchScene(back);
            }
            console.log(`[#229 pp] cg OBS back on "${back}" via ${via} (the gate had moved it to "${now}")`);
          } else {
            console.log(`[#229 pp] cg OBS is on "${now}" (moved since the press): left as it is`);
          }
        } catch (e) {
          problems.push(`putting cg OBS back on "${cgMoved.from}" failed: ${e}`);
        }
      }
    } finally {
      await ctx.dispose();
      await facade?.disconnect();
      await cg?.disconnect();
    }
    expect(problems, "afterAll restores PP's program").toEqual([]);
  });

  test("a playlist pressed through the facade plays on program, and SP-program has a receiver", async ({
    request,
  }) => {
    test.setTimeout(150_000);
    const rows = await readPlaylists(request);
    const program = await getJson<{ cut_refused: { source: number }[] | null }>(
      request,
      "/api/v1/program",
    );
    expect(program, "GET /api/v1/program").not.toBeNull();
    const refused = (program!.cut_refused ?? []).map((r) => r.source);
    const playable = new Map<number, number>();
    for (const r of rows.filter((x) => x.is_active)) {
      playable.set(r.id, await playableCount(request, r.id));
    }
    const pick = pickPlaylistScene(rows, refused, playable);
    expect(
      pick,
      `a playlist to put on program (active, a scene, a playable video, not refused ` +
        `${JSON.stringify(refused)}; playable ${JSON.stringify([...playable])}) ` +
        `among ${JSON.stringify(rows.map((r) => [r.id, r.ndi_output_name, r.is_active, r.kind]))}`,
    ).not.toBeNull();
    const { scene, playlistId } = pick!;
    console.log(`[#229 pp] playlist scene: ${scene} (playlist ${playlistId})`);

    await press(scene, playlistId);
    await expect
      .poll(() => readEngineActiveScene(request), {
        message: `SongPlayer's program reaches ${scene}`,
        timeout: 15_000,
      })
      .toBe(scene);
    await expect
      .poll(
        async () => {
          try {
            const row = await healthRow(request, playlistId);
            return row ? `${row.state}/${row.transport ?? "?"}` : "no health row";
          } catch (e) {
            return `read failed: ${e}`;
          }
        },
        { message: `playlist ${playlistId} plays on program (state/transport)`, timeout: 30_000 },
      )
      .toBe("Playing/Playing");
    // PP has no NDI consumer of SP-program: its wall takes SP-program-MAX
    // (post-deploy-max.spec.ts), and strih/stream are off at PP. So the PP
    // gate asserts the program's source; the live-receiver check stays SNV's
    // (post-deploy.spec.ts). Release 0.72.0 review.
    await expect
      .poll(
        async () => {
          const p = await getJson<{ source: number | null }>(request, "/api/v1/program");
          return p ? p.source : null;
        },
        { message: "SP-program carries the playlist", timeout: 60_000 },
      )
      .toBe(playlistId);
  });

  test("a manual scene pressed through the facade reaches the program as OBS manual", async ({
    request,
  }) => {
    const rows = await readPlaylists(request);
    const cgNow = await cg!.currentProgramScene();
    const pick = pickManualScene({
      configured: MANUAL_SCENE,
      cgProgram: cgNow,
      scenes: await facade!.listScenes(),
      playlistNames: playlistNames(rows),
    });
    if ("error" in pick) throw new Error(pick.error);
    console.log(`[#229 pp] manual scene: ${pick.scene} (cg OBS on "${cgNow}")`);

    // The press forwards the scene to cg OBS first: recorded before it, so a
    // press that fails half-way is still put back.
    if (pick.scene !== cgNow) cgMoved = { from: cgNow, to: pick.scene };
    await press(pick.scene, -1);
    await expect
      .poll(
        async () => {
          const p = await getJson<ManualCutView>(request, "/api/v1/program");
          return p ? manualCutLanded(p, pick.scene) : false;
        },
        {
          message: `${pick.scene} reaches the program as OBS manual (source -1, cg OBS accepted)`,
          timeout: 15_000,
        },
      )
      .toBe(true);
    await expect
      .poll(() => readEngineActiveScene(request), {
        message: `SongPlayer names the program ${pick.scene}`,
        timeout: 15_000,
      })
      .toBe(pick.scene);
  });
});
