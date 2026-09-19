import { test, expect, type Page } from "@playwright/test";

// #178: the dashboard playlist card's live A/V preview is a real MSE `<video>`
// fed fragmented MP4 over the `preview.ws` WebSocket (the #15 JPEG `<img>` is
// gone from the card). Round 3: the preview is CLICK-to-start — a Playing card
// shows a `preview-start` control and only mounts the `<video>` (opening the WS)
// after the operator clicks it, so nothing runs an encoder child unwatched. The
// mock (`mock-api.mjs`) streams a canned H.264/AAC fMP4 fixture over that WS.
// H.264/AAC are ABSENT from Playwright's bundled Chromium, so this spec runs
// ONLY in the `chrome` project (channel: 'chrome') — see playwright.config.ts.
// The mock marks playlist 1 (Worship) Playing and preselects it in the single
// work area; playlist 2 (Background) stays Idle so its card shows the
// placeholder, never a start control or a `<video>`.

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

// Open the playing Worship card and click its start control, returning the
// card + the now-mounted <video> locator.
async function startPreview(page: Page) {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  // Round 3: no <video> until the operator clicks start (on-demand only).
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
  await card.getByTestId("preview-start").click();
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });
  return { card, video };
}

test("clicking start streams live A/V into an MSE <video> that decodes and advances", async ({
  page,
}) => {
  const { video } = await startPreview(page);

  // readyState >= 3 (HAVE_FUTURE_DATA) proves the browser actually DECODED the
  // streamed H.264+AAC — a broken stream or a codec-less browser never gets here.
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);

  // Real decoded geometry (the fixture is 640x360).
  const width = await video.evaluate((el: HTMLVideoElement) => el.videoWidth);
  expect(width).toBeGreaterThan(0);

  // currentTime advances — the media is actually playing, not just buffered.
  const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
  await expect
    .poll(
      async () => video.evaluate((el: HTMLVideoElement) => el.currentTime),
      { timeout: 10000 },
    )
    .toBeGreaterThan(t0 + 0.05);
});

test("the click-started <video> ends up audible (unmuted directly or via the unmute button)", async ({
  page,
}) => {
  const { card, video } = await startPreview(page);
  // The start click is the user gesture, so the player tries to start UNMUTED.
  // Wait until it has decoded (the initial play() has resolved either way).
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);
  // If the browser rejected the unmuted autoplay and the shim fell back to
  // muted, one click of the unmute button re-enables audio.
  if (await video.evaluate((el: HTMLVideoElement) => el.muted)) {
    await card.getByTestId("preview-unmute").click();
  }
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.muted), {
      timeout: 5000,
    })
    .toBe(false);
});

test("the stop control tears the <video> down", async ({ page }) => {
  const { card, video } = await startPreview(page);
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);
  await card.getByTestId("preview-stop").click();
  // The <video> is unmounted (WS closed → the encoder child dies) and the start
  // control comes back in the placeholder.
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
  await expect(card.getByTestId("preview-start")).toBeVisible();
});

test("a playing card mounts no <video> until start is clicked", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  // Playing, but on-demand: the start control is shown and NO <video>/WS exists.
  await expect(card.getByTestId("preview-start")).toBeVisible();
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
});

test("idle card shows the preview placeholder and no start control or <video>", async ({
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
  // An idle card is not watchable — no start control, no <video>, no WS.
  await expect(card.getByTestId("preview-start")).toHaveCount(0);
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
});
