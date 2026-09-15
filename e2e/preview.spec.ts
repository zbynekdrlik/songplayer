import { test, expect } from "@playwright/test";

// #15 part 2: live video preview of the currently-playing song in each
// playlist card. The mock marks playlist 1 (Worship) Playing and serves a real
// JPEG for its preview; playlist 2 (Background) has now-playing info but stays
// Idle, so its card must show the placeholder, not an <img>.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

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

test("playing card renders a live preview image with non-zero size", async ({
  page,
}) => {
  // #165: the mock marks Worship (playlist 1) Playing, so it is preselected in
  // the single work area — no need to pick it.
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const worshipCard = page.locator(".playlist-card");
  const img = worshipCard.getByTestId("preview-img");
  await expect(img).toBeVisible({ timeout: 10000 });
  // The <img> must actually decode a frame (proves a real JPEG was served and
  // rendered, not a broken image).
  await expect
    .poll(async () => img.evaluate((el: HTMLImageElement) => el.naturalWidth), {
      timeout: 10000,
    })
    .toBeGreaterThan(0);
});

test("idle card shows the preview placeholder, hides the image, and issues no preview request", async ({
  page,
}) => {
  // The single stable <img> exists in the DOM for every card now, but an idle
  // card must keep it hidden AND never hit the network endpoint (its src is an
  // inline data-URI while idle).
  const idlePreviewRequests: string[] = [];
  page.on("request", (req) => {
    if (/\/api\/v1\/playback\/2\/preview\.jpg/.test(req.url())) {
      idlePreviewRequests.push(req.url());
    }
  });
  await page.goto("/");
  // #165: bring the idle Background playlist into the single work area by
  // selecting its row (Worship is playing and preselected by default).
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 10000,
  });
  await page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Background" })
    .click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Background");
  const bgCard = page.locator(".playlist-card");
  await expect(bgCard.getByTestId("preview-placeholder")).toBeVisible({
    timeout: 10000,
  });
  await expect(bgCard.getByTestId("preview-img")).toBeHidden();
  // Idle card issues no preview request across several tick intervals.
  await page.waitForTimeout(1500);
  expect(idlePreviewRequests).toEqual([]);
});

test("preview endpoint returns a JPEG for a playing playlist", async ({
  request,
}) => {
  const resp = await request.get("/api/v1/playback/1/preview.jpg");
  expect(resp.status()).toBe(200);
  expect(resp.headers()["content-type"]).toContain("image/jpeg");
  const body = await resp.body();
  expect(body.length).toBeGreaterThan(0);
});

test("preview endpoint returns 204 for an idle playlist", async ({
  request,
}) => {
  const resp = await request.get("/api/v1/playback/2/preview.jpg");
  expect(resp.status()).toBe(204);
});
