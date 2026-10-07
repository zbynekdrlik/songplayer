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
 *  - a playlist pressed through SongPlayer's facade (Companion's path) plays
 *    on SongPlayer's program, and SP-program has a receiver;
 *  - a manual scene pressed through the facade reaches the program as "OBS
 *    manuál" (source -1), cg OBS accepting it.
 * SP-program-MAX is gated by post-deploy-max.spec.ts in the same run. The A/V
 * gate comes later, once PP records.
 *
 * The scenes come from `pp-scenes.ts` (the playlist scene from SongPlayer's
 * own catalog; the manual scene = cg OBS's own program scene unless the repo
 * variable PP_MANUAL_SCENE names one). The facade's program scene and cg
 * OBS's program scene are put back in `afterAll`. The tests are not serial: a
 * missing Cloudflare token must not hide the playback results.
 */
import {
  test,
  expect,
  request as apiRequest,
  type APIRequestContext,
} from "@playwright/test";
import { healthRow } from "./box-api";
import { ObsDriver } from "./obs-driver";
import { programReceiverVerdict, type ProgramReceiverView } from "./ndi-health-gate";
import {
  peerSetupFailures,
  probeFailures,
  type ExchangeStatusView,
  type ProbeResult,
} from "./peer-probe-gate";
import {
  manualCutLanded,
  pickManualScene,
  pickPlaylistScene,
  playlistNames,
  type ManualCutView,
  type PlaylistView,
} from "./pp-scenes";

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

/** SongPlayer's own program scene (`/api/v1/status.active_scene`). */
async function activeScene(request: APIRequestContext): Promise<string | null> {
  return (await getJson<{ active_scene: string | null }>(request, "/api/v1/status"))?.active_scene ?? null;
}

async function readPlaylists(request: APIRequestContext): Promise<PlaylistView[]> {
  const resp = await request.get("/api/v1/playlists");
  expect(resp.status(), "GET /api/v1/playlists").toBe(200);
  return (await resp.json()) as PlaylistView[];
}

async function videoCount(request: APIRequestContext, pid: number): Promise<number> {
  const resp = await request.get(`/api/v1/playlists/${pid}/videos`);
  expect(resp.status(), `GET /api/v1/playlists/${pid}/videos`).toBe(200);
  const videos = (await resp.json()) as unknown;
  return Array.isArray(videos) ? videos.length : 0;
}

test.describe("PP post-deploy (#229)", () => {
  test("the dashboard shows the deployed version", async ({ page, request }) => {
    const consoleMessages: string[] = [];
    page.on("console", (msg) => {
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
});

test.describe("PP's program through the facade (#229)", () => {
  let facade: ObsDriver | null = null;
  let cg: ObsDriver | null = null;
  /** SP-program's scene before the tests (null: nothing was on it). */
  let startScene: string | null = null;
  /** cg OBS's own program scene before the tests. */
  let cgStartScene: string | null = null;
  /** A test pressed a scene: afterAll restores the program. */
  let pressed = false;

  test.beforeAll(async () => {
    facade = await ObsDriver.connect(FACADE_WS_URL);
    cg = await ObsDriver.connect(OBS_WS_URL);
    try {
      startScene = await facade.currentProgramScene();
    } catch {
      startScene = null; // the facade's 604: nothing is on SP-program
    }
    cgStartScene = await cg.currentProgramScene();
    console.log(`[#229 pp] at the start: SP-program "${startScene}", cg OBS "${cgStartScene}"`);
  });

  test.afterAll(async () => {
    const problems: string[] = [];
    const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
    try {
      if (pressed && facade && startScene !== null) {
        try {
          await facade.switchScene(startScene);
          const deadline = Date.now() + 8_000;
          let now = await activeScene(ctx);
          while (now !== startScene && Date.now() < deadline) {
            await new Promise((r) => setTimeout(r, 200));
            now = await activeScene(ctx);
          }
          if (now !== startScene) {
            problems.push(`SP-program is on "${now}", not back on "${startScene}"`);
          }
        } catch (e) {
          problems.push(`putting SP-program back on "${startScene}" failed: ${e}`);
        }
      } else if (pressed) {
        console.log("[#229 pp] nothing was on SP-program at the start: the test's scene stays");
      }
      if (cg && cgStartScene !== null) {
        try {
          const now = await cg.currentProgramScene();
          if (now !== cgStartScene) {
            await cg.switchScene(cgStartScene);
            console.log(`[#229 pp] cg OBS back on "${cgStartScene}" (was "${now}")`);
          }
        } catch (e) {
          problems.push(`putting cg OBS back on "${cgStartScene}" failed: ${e}`);
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
    const videos = new Map<number, number>();
    for (const r of rows.filter((x) => x.is_active)) videos.set(r.id, await videoCount(request, r.id));
    const pick = pickPlaylistScene(rows, refused, videos);
    expect(
      pick,
      `a playlist to put on program (active, a scene, videos, not refused ${JSON.stringify(refused)}) ` +
        `among ${JSON.stringify(rows.map((r) => [r.id, r.ndi_output_name, r.is_active, r.kind]))}`,
    ).not.toBeNull();
    const { scene, playlistId } = pick!;
    console.log(`[#229 pp] playlist scene: ${scene} (playlist ${playlistId})`);

    pressed = true;
    await facade!.switchScene(scene);
    await expect
      .poll(() => activeScene(request), {
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
    await expect
      .poll(
        async () => {
          const p = await getJson<ProgramReceiverView>(request, "/api/v1/program");
          return p ? programReceiverVerdict(p) : null;
        },
        { message: "SP-program carries the playlist and has a live receiver", timeout: 60_000 },
      )
      .toMatchObject({ ok: true, source: playlistId });
  });

  test("a manual scene pressed through the facade reaches the program as OBS manual", async ({
    request,
  }) => {
    const rows = await readPlaylists(request);
    const pick = pickManualScene({
      configured: MANUAL_SCENE,
      cgProgram: await cg!.currentProgramScene(),
      scenes: await facade!.listScenes(),
      playlistNames: playlistNames(rows),
    });
    if ("error" in pick) throw new Error(pick.error);
    console.log(`[#229 pp] manual scene: ${pick.scene}`);

    pressed = true;
    await facade!.switchScene(pick.scene);
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
      .poll(() => activeScene(request), {
        message: `SongPlayer names the program ${pick.scene}`,
        timeout: 15_000,
      })
      .toBe(pick.scene);
  });
});
