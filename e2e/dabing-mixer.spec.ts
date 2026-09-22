import { test, expect } from "@playwright/test";

// #184 round G: the ONE live mixer follows the playing DUB video on EVERY page,
// because `store.dabing` is filled by an App-level poll. A ready dub adds the
// mix-dabing fader; a plain stems song shows only mix-vokaly + mix-podklad. This
// spec proves that WITHOUT navigating to /dabing, plus exactly one PATCH per
// preset click. Zero console errors.

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
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/tick", { data: { enabled: false } });
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test.describe("the dub fader follows the playing dub video on every page", () => {
  test("Dashboard shows mix-dabing without visiting /dabing", async ({
    page,
    request,
  }) => {
    // A ready dub for video 1 — the video the Dashboard's playlist 1 plays by
    // default (the mock WS marks playlist 1 Playing with video_id 1).
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // The App-level poll populated store.dabing → the shared Player's LiveMixer
    // shows the dabing fader for the playing dub, WITHOUT visiting /dabing.
    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-vokaly")).toBeVisible();
    expect(new URL(page.url()).pathname).toBe("/");
  });

  test("Live shows mix-dabing without visiting /dabing", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 700,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });

    await page.goto("/live");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // Make the dub the playing item on the live playlist (id 184).
    await request.post("/__mock/now-playing", {
      data: {
        playlist_id: 184,
        video_id: 700,
        song: "Kázeň",
        duration_ms: 200000,
      },
    });

    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    expect(new URL(page.url()).pathname).toBe("/live");
  });

  test("a stems-ready dub shows all three faders on one strip", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: "done",
      },
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-vokaly")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-podklad")).toBeVisible();
    await expect(page.getByTestId("mix-dabing")).toBeVisible();
    // Still ONE strip (not two mixers).
    await expect(page.locator(".mixer.mixer-live")).toHaveCount(1);
  });

  test("a plain stems song shows only mix-vokaly + mix-podklad", async ({
    page,
  }) => {
    // No dub rows (reset in beforeEach) → the playing song (video 1) is a plain
    // stems song: no dabing fader.
    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-vokaly")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-podklad")).toBeVisible();
    await expect(page.getByTestId("mix-dabing")).toHaveCount(0);
  });
});

test.describe("a dub preset click issues exactly one PATCH", () => {
  test("clicking Originál PATCHes /mix exactly once", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });

    let patchCount = 0;
    await page.route("**/api/v1/mix", async (route) => {
      if (route.request().method() === "PATCH") patchCount += 1;
      await route.continue();
    });

    await page.goto("/");
    await expect(page.getByTestId("mix-presets")).toBeVisible({
      timeout: 15000,
    });

    await page.getByTestId("mixer-preset-original").click();
    await expect.poll(() => patchCount, { timeout: 5000 }).toBe(1);
    await page.waitForTimeout(1000);
    expect(patchCount, "one preset click = one PATCH").toBe(1);
  });
});
