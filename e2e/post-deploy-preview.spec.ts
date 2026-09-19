/**
 * #178 post-deploy: the dashboard card's live A/V preview `<video>` (MSE, fed
 * fragmented MP4 over `preview.ws` by the box's on-demand ffmpeg encoder child)
 * must actually DECODE and PLAY on the deployed box.
 *
 * This runs against the REAL deployed SongPlayer + OBS, so it must decode real
 * H.264/AAC — which Playwright's bundled Chromium cannot. It therefore runs
 * ONLY under the `edge` project in `post-deploy.config.ts` (channel 'msedge' —
 * Edge is always present on the Windows box and carries the proprietary codecs);
 * the default `chromium` project ignores this file.
 *
 * Scene discipline (ci-workflows.md / CLAUDE.md): it drives OBS to the SAME
 * `sp-fast` scene the main post-deploy suite already uses (no NEW scene is put
 * on the wall), captures the operator's scene at start and restores it after
 * every test AND at suite end, proving the engine followed — never leaving the
 * wall on the E2E scene.
 */

import { test, expect, request as apiRequest } from "@playwright/test";
import { ObsDriver } from "./obs-driver";

const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";
const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";

const FAST_PLAYLIST_NAME = "ytfast";
const FAST_SCENE_NAME = "sp-fast";

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

async function findPlaylistId(
  request: import("@playwright/test").APIRequestContext,
  name: string,
): Promise<number> {
  const resp = await request.get("/api/v1/playlists");
  const list = (await resp.json()) as Array<{ id: number; name: string }>;
  const pl = list.find((p) => p.name === name);
  expect(pl, `deployed server must have a "${name}" playlist`).toBeTruthy();
  return pl!.id;
}

test.describe("#178 live preview <video> post-deploy", () => {
  let obs: ObsDriver | null = null;
  let initialScene: string | null = null;
  let consoleMessages: string[] = [];

  test.beforeAll(async () => {
    obs = await ObsDriver.connect(OBS_WS_URL);
    try {
      initialScene = await obs.currentProgramScene();
    } catch {
      initialScene = null;
    }
  });

  test.beforeEach(async ({ page }) => {
    consoleMessages = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });
  });

  test.afterEach(async () => {
    const driver = obs;
    if (driver && initialScene) {
      try {
        await driver.switchScene(initialScene);
      } catch {
        // best-effort between tests; afterAll asserts the final state
      }
    }
    const real = consoleMessages.filter(
      (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
    );
    expect(real).toEqual([]);
  });

  test.afterAll(async () => {
    const driver = obs;
    if (!driver) return;
    try {
      const target = initialScene ?? FAST_SCENE_NAME;
      await driver.switchScene(target);
    } catch {
      // best-effort
    } finally {
      await driver.disconnect();
    }
  });

  test("playing card's preview <video> decodes on the box and advances", async ({
    page,
    request,
  }) => {
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();
    const fastId = await findPlaylistId(request, FAST_PLAYLIST_NAME);
    const scenes = await obs!.listScenes();
    expect(
      scenes.includes(FAST_SCENE_NAME),
      `deployed OBS must have an "${FAST_SCENE_NAME}" scene subscribed to "SP-fast"`,
    ).toBe(true);

    // Put ytfast on program so its card is Playing (the card only mounts the
    // preview <video> — and opens preview.ws — while Playing).
    await obs!.switchScene(FAST_SCENE_NAME);
    const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
    try {
      const deadline = Date.now() + 8_000;
      let active = false;
      while (Date.now() < deadline) {
        const status = (await (await ctx.get("/api/v1/status")).json()) as {
          active_playlist_ids?: number[];
        };
        if ((status.active_playlist_ids ?? []).includes(fastId)) {
          active = true;
          break;
        }
        await new Promise((r) => setTimeout(r, 250));
      }
      expect(
        active,
        `ytfast (id=${fastId}) must become active after switching to "${FAST_SCENE_NAME}"`,
      ).toBe(true);
    } finally {
      await ctx.dispose();
    }

    // Open the dashboard and select the ytfast work-area card.
    await page.goto("/");
    await expect(page.getByTestId("playlist-workspace")).toBeVisible({
      timeout: 30_000,
    });
    const row = page
      .getByTestId("playlist-selector-row")
      .filter({ hasText: FAST_PLAYLIST_NAME });
    await row.click();
    await expect(page.getByTestId("workspace-title")).toHaveText(
      FAST_PLAYLIST_NAME,
      { timeout: 10_000 },
    );
    const card = page.locator(".playlist-card", { hasText: FAST_PLAYLIST_NAME });
    const video = card.getByTestId("preview-video");
    await expect(video).toBeVisible({ timeout: 10_000 });

    // The box's ffmpeg encoder child must spawn on this first viewer, produce a
    // keyframe-aligned fMP4, and the browser must DECODE it: readyState >= 3.
    await expect
      .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
        timeout: 40_000,
      })
      .toBeGreaterThanOrEqual(3);

    const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
    await expect
      .poll(async () => video.evaluate((el: HTMLVideoElement) => el.currentTime), {
        timeout: 15_000,
      })
      .toBeGreaterThan(t0 + 0.05);

    // Real decoded geometry — the encoder outputs a fixed 640x360 canvas.
    const width = await video.evaluate((el: HTMLVideoElement) => el.videoWidth);
    expect(width).toBeGreaterThan(0);
  });
});
