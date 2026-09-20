import { test, expect, Page, APIRequestContext } from "@playwright/test";

// #200 — REAL-POINTER interaction specs. The liveness/mixer specs drive the
// sliders with fill() + synthetic dispatchEvent('pointerup'/'change'); a real
// browser fires `pointerup` BEFORE `change`, and when the drag gate then
// re-applies the live value, Chrome suppresses `change` entirely — the owner's
// "posúvať pozíciu sa nedá" / "fádre sa nedajú hýbať" that synthetic specs
// cannot see. These specs use page.mouse only.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const DABING_PLAYLIST_ID = 500;
const DUB_VIDEO_ID = 344;
const DURATION_MS = 200000;

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/fixture", { data: { mode: "default" } });
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

async function tickDabing(request: APIRequestContext) {
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: DUB_VIDEO_ID,
      title: "Morning Prayer",
      dub_status: "ready",
      stem_status: null,
      dub_mix_ratio: 1.0,
    },
  });
  await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: DABING_PLAYLIST_ID,
          video_id: DUB_VIDEO_ID,
          duration_ms: DURATION_MS,
          state: "Playing",
        },
      ],
    },
  });
}

/** Real mouse drag along a horizontal/vertical range input from `from` to `to` (fractions). */
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

test.describe("#200: real mouse drags commit exactly once", () => {
  test("seek bar: a mouse drag posts ONE seek at the released position", async ({
    page,
    request,
  }) => {
    const seeks: number[] = [];
    await page.route("**/api/v1/playback/*/seek", async (route) => {
      seeks.push(JSON.parse(route.request().postData() || "{}").position_ms);
      await route.fulfill({ status: 204 });
    });
    await tickDabing(request);
    await page.goto("/dabing");
    const seek = page.getByTestId("player-seek");
    await expect(seek).toBeEnabled({ timeout: 15000 });

    await mouseDrag(page, '[data-testid="player-seek"]', 0.1, 0.7);

    await expect.poll(() => seeks.length, { timeout: 5000 }).toBe(1);
    // ~70 % of the duration, with slack for the thumb width + 1 s step.
    expect(seeks[0]).toBeGreaterThan(DURATION_MS * 0.55);
    expect(seeks[0]).toBeLessThan(DURATION_MS * 0.85);
    // No second commit sneaks in (a synthetic-free release path).
    await page.waitForTimeout(800);
    expect(seeks.length).toBe(1);
  });

  test("dub fader: a mouse drag PATCHes the dragged ratio and the fader stays there", async ({
    page,
    request,
  }) => {
    const ratios: number[] = [];
    await page.route("**/api/v1/videos/*/dub-mix", async (route) => {
      ratios.push(JSON.parse(route.request().postData() || "{}").ratio);
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ ratio: ratios[ratios.length - 1] }),
      });
    });
    await tickDabing(request);
    await page.goto("/dabing");
    const fader = page.getByTestId("dub-mix-fader");
    await expect(fader).toBeEnabled({ timeout: 15000 });
    await expect(fader).toHaveValue("100");

    // Vertical fader: drag from the top (100 %) down to ~30 %.
    await mouseDrag(page, '[data-testid="dub-mix-fader"]', 0.98, 0.3);

    await expect.poll(() => ratios.length, { timeout: 5000 }).toBe(1);
    expect(ratios[0]).toBeGreaterThan(0.15);
    expect(ratios[0]).toBeLessThan(0.45);
    // The fader must not snap back to the pre-drag value after release.
    await page.waitForTimeout(1200);
    const v = Number(await fader.inputValue());
    expect(v).toBeGreaterThan(15);
    expect(v).toBeLessThan(45);
  });

  test("the preview box fills the Player width (no 320 px card cap)", async ({
    page,
    request,
  }) => {
    await tickDabing(request);
    await page.goto("/dabing");
    const player = page.getByTestId("player");
    await expect(player).toBeVisible({ timeout: 15000 });
    const box = page.locator(".player-preview .preview-video-box, .player-preview .preview-placeholder").first();
    await expect(box).toBeVisible({ timeout: 10000 });
    const pw = (await player.boundingBox())!.width;
    const bw = (await box.boundingBox())!.width;
    expect(bw).toBeGreaterThan(pw * 0.9);
  });
});
