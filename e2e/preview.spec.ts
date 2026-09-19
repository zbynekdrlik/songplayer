import { test, expect } from "@playwright/test";

// #178: the dashboard playlist card's live A/V preview is now a real MSE
// `<video>` fed fragmented MP4 over the `preview.ws` WebSocket (the #15 JPEG
// `<img>` is gone from the card). The mock (`mock-api.mjs`) streams a canned
// H.264/AAC fMP4 fixture over that WS. H.264/AAC are ABSENT from Playwright's
// bundled Chromium, so this spec runs ONLY in the `chrome` project
// (channel: 'chrome') — see playwright.config.ts. The mock marks playlist 1
// (Worship) Playing and preselects it in the single work area; playlist 2
// (Background) stays Idle so its card shows the placeholder, not a `<video>`.

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

test("playing card streams live A/V into an MSE <video> that decodes and advances", async ({
  page,
}) => {
  // #165: the mock marks Worship (playlist 1) Playing, so it is preselected in
  // the single work area — no need to pick it.
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });

  // readyState >= 3 (HAVE_FUTURE_DATA) proves the browser actually DECODED the
  // streamed H.264+AAC — a broken stream or a codec-less browser never gets
  // here.
  await expect
    .poll(
      async () =>
        video.evaluate((el: HTMLVideoElement) => el.readyState),
      { timeout: 15000 },
    )
    .toBeGreaterThanOrEqual(3);

  // Real decoded geometry (the fixture is 640x360).
  const width = await video.evaluate(
    (el: HTMLVideoElement) => el.videoWidth,
  );
  expect(width).toBeGreaterThan(0);

  // currentTime advances — the media is actually playing, not just buffered.
  const t0 = await video.evaluate(
    (el: HTMLVideoElement) => el.currentTime,
  );
  await expect
    .poll(
      async () =>
        video.evaluate((el: HTMLVideoElement) => el.currentTime),
      { timeout: 10000 },
    )
    .toBeGreaterThan(t0 + 0.05);
});

test("unmute button flips the <video> from muted to audible", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });
  // Starts muted (Chrome autoplay gesture rule — muted autoplay is allowed).
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.muted), {
      timeout: 10000,
    })
    .toBe(true);
  // One real click (a user gesture) unmutes.
  await card.getByTestId("preview-unmute").click();
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.muted), {
      timeout: 5000,
    })
    .toBe(false);
});

test("idle card shows the preview placeholder and mounts no <video>", async ({
  page,
}) => {
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
  const card = page.locator(".playlist-card");
  await expect(card.getByTestId("preview-placeholder")).toBeVisible({
    timeout: 10000,
  });
  // No live preview <video> is mounted for an idle card (on-demand only — no WS,
  // no encoder child).
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
});
