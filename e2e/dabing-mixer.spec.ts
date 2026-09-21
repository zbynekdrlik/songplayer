import { test, expect } from "@playwright/test";

// #184 defect B: the Dashboard (Prehľad) and Live (Naživo) pages showed the
// KARAOKE mixer for a playing DUB video, because `store.dabing` was filled only
// by visiting the Dabing page. The fix hoists the `/api/v1/dabing` poll into
// `App`, so the shared Player picks the dub mixer for a dub video on EVERY page.
// This spec proves that WITHOUT ever navigating to /dabing, plus the one-app
// rule (stems-ready dub → BOTH mixers; a plain stems song → karaoke only) and
// exactly one PATCH per preset click. Zero console errors.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  // Start every test from a clean dub list + no leaked position tick.
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/tick", { data: { enabled: false } });
});

test.afterEach(async ({ request }) => {
  // Reset the shared in-memory mock state so a leaked dub row / tick cannot
  // flip a sibling spec (workers: 1, serial). player.spec.ts asserts the
  // Dashboard shows NO dub fader for a plain song and runs after this file.
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test.describe("the dub mixer follows the playing dub video on every page (#184)", () => {
  test("Dashboard shows the dub mixer without visiting /dabing", async ({
    page,
    request,
  }) => {
    // A ready dub for video 1 — the video the Dashboard's playlist 1 plays by
    // default (the mock's WS marks playlist 1 Playing with video_id 1).
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
        dub_mix_ratio: 1.0,
      },
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // The App-level poll populated store.dabing on the Dashboard → the shared
    // Player renders the dub mixer for the playing dub, WITHOUT visiting /dabing.
    await expect(page.getByTestId("dub-mixer-state")).toBeVisible({
      timeout: 15000,
    });
    await expect(page.getByTestId("dub-mix-fader")).toBeVisible({
      timeout: 15000,
    });
    // We never navigated to /dabing.
    expect(new URL(page.url()).pathname).toBe("/");
  });

  test("Live shows the dub mixer without visiting /dabing", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 700,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
        dub_mix_ratio: 1.0,
      },
    });

    await page.goto("/live");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // Make the dub video the playing item on the live playlist (id 184) — the
    // live Player is for that playlist. The default WS tick never touches 184.
    await request.post("/__mock/now-playing", {
      data: {
        playlist_id: 184,
        video_id: 700,
        song: "Kázeň",
        duration_ms: 200000,
      },
    });

    await expect(page.getByTestId("dub-mixer-state")).toBeVisible({
      timeout: 15000,
    });
    expect(new URL(page.url()).pathname).toBe("/live");
  });

  test("a stems-ready dub shows BOTH the dub and karaoke mixers", async ({
    page,
    request,
  }) => {
    // stem_status "done" is the raw stems-ready value on the videos row, so the
    // playing dub is stems-capable → both panels (the #194 one-app rule).
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: "done",
        dub_mix_ratio: 1.0,
      },
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("dub-mixer-state")).toBeVisible({
      timeout: 15000,
    });
    // The karaoke mixer is ALSO mounted (its now-playing state line resolves).
    await expect(page.getByTestId("karaoke-now-playing")).toBeVisible({
      timeout: 15000,
    });
  });

  test("a plain stems song shows only the karaoke mixer", async ({ page }) => {
    // No dub rows (reset in beforeEach) → the playing song (video 1) is a plain
    // stems song: karaoke mixer, no dub mixer.
    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("karaoke-now-playing")).toBeVisible({
      timeout: 15000,
    });
    await expect(page.getByTestId("dub-mixer-state")).toHaveCount(0);
    await expect(page.getByTestId("dub-mix-fader")).toHaveCount(0);
  });
});

test.describe("a dub preset click issues exactly one PATCH (#184)", () => {
  test("clicking a preset PATCHes /dub-mix exactly once", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
        dub_mix_ratio: 1.0,
      },
    });

    let patchCount = 0;
    await page.route("**/api/v1/videos/*/dub-mix", async (route) => {
      if (route.request().method() === "PATCH") patchCount += 1;
      await route.continue();
    });

    await page.goto("/");
    await expect(page.getByTestId("dub-mixer-presets")).toBeVisible({
      timeout: 15000,
    });

    // "Originál" preset (ratio 0.0). One click → one PATCH.
    await page.getByTestId("mixer-preset-original").click();
    await expect.poll(() => patchCount, { timeout: 5000 }).toBe(1);
    // Give any spurious extra PATCH a chance to arrive, then re-assert.
    await page.waitForTimeout(1000);
    expect(patchCount, "one preset click = one PATCH").toBe(1);
  });
});
