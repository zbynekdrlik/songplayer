import { test, expect, Page } from "@playwright/test";

/**
 * Post-deploy Dabing checks on the REAL box (#184 D5 item 2 + #200).
 *
 * The Dabing output (SP-dabing) is never on program during CI, so starting the
 * sample dub here touches only the sp-dabing OBS inputs, never the wall. What
 * this proves after every deploy:
 *  1. the Dabing section lists a READY dub (the 40-min acceptance sample) and
 *     the SP-dabing NDI output carries at least one receiver;
 *  2. the shared Player on /dabing is driven by a REAL mouse: a drag on the
 *     dub fader PATCHes the ratio and stays put, a drag on the seek bar posts a
 *     seek — the two controls the owner found dead on 20.9.2026 (#200);
 *  3. zero console errors throughout.
 */

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];
let dabingPid = 0;
let sampleVideoId = 0;

async function mouseDrag(page: Page, selector: string, from: number, to: number) {
  // page.mouse has no auto-scroll: a control below the fold receives nothing.
  await page.locator(selector).scrollIntoViewIfNeeded();
  const box = await page.locator(selector).boundingBox();
  if (!box) throw new Error(`no bounding box for ${selector}`);
  const vertical = box.height > box.width;
  const pt = (f: number) =>
    vertical
      ? { x: box.x + box.width / 2, y: box.y + box.height * (1 - f) }
      : { x: box.x + box.width * f, y: box.y + box.height / 2 };
  const a = pt(from);
  const b = pt(to);
  await page.mouse.move(a.x, a.y);
  await page.mouse.down();
  await page.mouse.move(b.x, b.y, { steps: 8 });
  await page.mouse.up();
}

test.describe.serial("Dabing output on the box (#184, #200)", () => {
  test.beforeEach(async ({ page }) => {
    consoleMessages = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });
  });

  test.afterEach(async () => {
    const real = consoleMessages.filter(
      (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
    );
    expect(real).toEqual([]);
  });

  test.afterAll(async ({ request }) => {
    // Leave the box as found: dub-only mix, Dabing output paused.
    if (sampleVideoId) {
      await request.patch(`/api/v1/videos/${sampleVideoId}/dub-mix`, {
        data: { ratio: 1.0 },
      });
    }
    if (dabingPid) {
      await request.post(`/api/v1/playback/${dabingPid}/pause`);
    }
  });

  test("a READY dub is listed and SP-dabing has a receiver", async ({ request }) => {
    const dab = await request.get("/api/v1/dabing");
    expect(dab.status()).toBe(200);
    const body = (await dab.json()) as {
      playlist_id: number;
      videos: Array<{ video_id?: number; id?: number; dub_status: string; chain_state: string }>;
    };
    dabingPid = body.playlist_id;
    const ready = body.videos.find((v) => v.dub_status === "ready");
    expect(ready, "at least one dub must be ready on the box").toBeTruthy();
    // DubRow carries the id as `video_id` (the row is keyed by the video).
    sampleVideoId = Number(ready!.video_id ?? ready!.id);
    expect(sampleVideoId, "the ready dub must carry a numeric video id").toBeGreaterThan(0);

    const health = await request.get("/api/v1/ndi/health");
    expect(health.status()).toBe(200);
    const rows = (await health.json()) as Array<{
      playlist_id: number;
      ndi_name: string;
      connections: number;
    }>;
    const out = rows.find((r) => r.playlist_id === dabingPid);
    expect(out, "SP-dabing must be advertised").toBeTruthy();
    expect(out!.ndi_name).toBe("SP-dabing");
    expect(out!.connections).toBeGreaterThanOrEqual(1);
  });

  test("real mouse: the dub fader and the seek bar commit on release", async ({
    page,
  }) => {
    await page.goto("/dabing");
    const row = page.locator(`[data-testid="song-row"][data-video-id="${sampleVideoId}"]`);
    await expect(row).toBeVisible({ timeout: 15000 });
    await expect(row.getByTestId("chip-dub")).toContainText("hotový");

    // Start the sample on the (off-program) Dabing output.
    await row.getByTestId("song-row-play").click();
    const playpause = page.getByTestId("player-playpause");
    await expect(playpause).toContainText("Pauza", { timeout: 20000 });

    // Dub fader: drag from the top (100 %) to ~40 % → ONE PATCH, fader stays.
    const fader = page.getByTestId("dub-mix-fader");
    await expect(fader).toBeEnabled({ timeout: 15000 });
    const patch = page.waitForResponse(
      (r) => r.url().includes(`/api/v1/videos/${sampleVideoId}/dub-mix`) && r.request().method() === "PATCH",
      { timeout: 10000 },
    );
    await mouseDrag(page, '[data-testid="dub-mix-fader"]', 0.98, 0.4);
    expect((await patch).status()).toBe(200);
    await page.waitForTimeout(1500);
    const v = Number(await fader.inputValue());
    expect(v).toBeGreaterThan(20);
    expect(v).toBeLessThan(60);

    // Seek bar: drag to ~30 % → a seek is posted (204) and the position follows.
    const seek = page.getByTestId("player-seek");
    await expect(seek).toBeEnabled({ timeout: 15000 });
    const seekResp = page.waitForResponse(
      (r) => r.url().includes(`/api/v1/playback/${dabingPid}/seek`) && r.request().method() === "POST",
      { timeout: 10000 },
    );
    await mouseDrag(page, '[data-testid="player-seek"]', 0.05, 0.3);
    expect((await seekResp).status()).toBe(204);
  });
});
