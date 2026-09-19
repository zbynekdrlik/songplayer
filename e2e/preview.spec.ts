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
  request,
}) => {
  const { video } = await startPreview(page);

  // #194: run the decode assertion WITH the 500 ms now-playing position tick
  // flowing — a live position tick must never disturb the preview stream.
  await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: 1,
          video_id: 1,
          song: "Never Gonna Give You Up",
          duration_ms: 213000,
          position_ms: 0,
          state: "Playing",
        },
      ],
    },
  });
  try {
    // readyState >= 3 (HAVE_FUTURE_DATA) proves the browser actually DECODED the
    // streamed H.264+AAC — a broken stream or a codec-less browser never gets here.
    await expect
      .poll(
        async () => video.evaluate((el: HTMLVideoElement) => el.readyState),
        { timeout: 15000 },
      )
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
  } finally {
    await request.post("/__mock/tick", { data: { enabled: false } });
  }
});

test("the Dabing Player preview survives position ticks — WS opened once, never closed (#194)", async ({
  page,
  request,
}) => {
  // The #194 box regression: the Dabing page re-set an unchanged playlist id
  // every 2 s, re-creating the whole Player → the preview <video> unmounted and
  // its WebSocket was torn down right after opening ("WebSocket is closed before
  // the connection is established"). Here the preview WS actually opens (branded
  // Chrome has the codecs), so we can prove it opens ONCE and never closes while
  // the 500 ms position tick runs.
  const previewSockets: { closed: boolean }[] = [];
  page.on("websocket", (ws) => {
    if (ws.url().includes("/preview.ws")) {
      const rec = { closed: false };
      ws.on("close", () => {
        rec.closed = true;
      });
      previewSockets.push(rec);
    }
  });

  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 344,
      title: "Morning Prayer",
      dub_status: "ready",
      stem_status: null,
      dub_mix_ratio: 1.0,
    },
  });
  await page.goto("/dabing");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
  await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: 500,
          video_id: 344,
          song: "Morning Prayer",
          duration_ms: 200000,
          position_ms: 0,
          state: "Playing",
        },
      ],
    },
  });

  try {
    await page.getByTestId("preview-start").click();
    const video = page.getByTestId("preview-video");
    await expect(video).toBeVisible({ timeout: 10000 });
    await expect
      .poll(
        async () => video.evaluate((el: HTMLVideoElement) => el.readyState),
        { timeout: 15000 },
      )
      .toBeGreaterThanOrEqual(3);

    const playerHandle = await page.getByTestId("player").elementHandle();
    const videoHandle = await video.elementHandle();

    // Let >= 8 s of 500 ms position ticks flow.
    await page.waitForTimeout(8000);

    // Same Player + <video> elements; the preview WS opened exactly once and
    // never closed; the stream is still decoding.
    expect(await playerHandle!.evaluate((el) => el.isConnected)).toBe(true);
    expect(await videoHandle!.evaluate((el) => el.isConnected)).toBe(true);
    expect(previewSockets.length).toBe(1);
    expect(previewSockets[0].closed).toBe(false);
    expect(
      await video.evaluate((el: HTMLVideoElement) => el.readyState),
    ).toBeGreaterThanOrEqual(3);
  } finally {
    await request.post("/__mock/tick", { data: { enabled: false } });
    await request.post("/__mock/dabing-reset");
  }
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
