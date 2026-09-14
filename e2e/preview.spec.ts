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
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Worship" })).toBeVisible({
    timeout: 10000,
  });
  const worshipCard = page.locator(".playlist-card", {
    has: page.getByRole("heading", { name: "Worship" }),
  });
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

test("idle card shows the preview placeholder, not an image", async ({
  page,
}) => {
  await page.goto("/");
  const bgCard = page.locator(".playlist-card", {
    has: page.getByRole("heading", { name: "Background" }),
  });
  await expect(bgCard.getByTestId("preview-placeholder")).toBeVisible({
    timeout: 10000,
  });
  await expect(bgCard.getByTestId("preview-img")).toHaveCount(0);
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
