import { test, expect, type Page, type APIRequestContext } from "@playwright/test";

// #194 hotfix (round 3c-hot): the owner's box regression — a Dabing dub video
// "len pada a nespusta sa to" and the mixer faders "sa nedali normalne hybat".
// Root cause: a live now-playing POSITION tick re-created the whole Player (the
// Dabing page re-set an unchanged playlist id every 2 s) and re-ran the mixer
// slot closure (it read a plain `has_content()` over `store.now_playing`), and
// the seek/faders had no drag gate, so a tick snatched the value out from under
// the operator's finger. The suite never advanced now-playing DURING a
// drag/preview, so it stayed green while the app felt broken.
//
// This spec turns the mock's 500 ms position TICK mode ON and proves, under a
// real browser, that a position tick updates text/progress and NOTHING else:
//   - the Player root element identity is unchanged across ticks,
//   - the click-started preview <video> element stays mounted across ticks,
//   - a fader drag holds its dragged value across ticks and commits ONE PATCH,
//   - a seek drag holds its dragged position across ticks and commits ONE seek.
//
// This runs in the CHROMIUM project. The live-preview MSE decode (H.264/AAC) +
// the preview WebSocket lifecycle are proven in `preview.spec.ts` (the `chrome`
// project) — bundled Chromium lacks the codecs, so the shim never opens the
// preview WS here; we assert the <video> ELEMENT stays mounted (proving the
// slot was not re-created), which is codec-independent.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

const DABING_PLAYLIST_ID = 500;
const DUB_VIDEO_ID = 344;

// Bring up the Dabing page with a ready dub video PLAYING off-program and the
// 500 ms position tick running, so the shared Player mounts the dub mixer.
async function setupDabingPlayer(page: Page, request: APIRequestContext) {
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: DUB_VIDEO_ID,
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
          playlist_id: DABING_PLAYLIST_ID,
          video_id: DUB_VIDEO_ID,
          song: "Morning Prayer",
          duration_ms: 200000,
          position_ms: 0,
          state: "Playing",
        },
      ],
    },
  });
}

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // Always turn tick mode back off so later specs are unaffected.
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("the Dabing Player and its preview <video> survive position ticks (no re-create) (#194)", async ({
  page,
  request,
}) => {
  await setupDabingPlayer(page, request);

  // is_decoding is true (Playing + content) → the click-to-start control shows.
  const playerHandle = await page.getByTestId("player").elementHandle();
  await page.getByTestId("preview-start").click();
  const video = page.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });
  const videoHandle = await video.elementHandle();

  // Let >= 8 s of 500 ms position ticks flow (16 ticks).
  await page.waitForTimeout(8000);

  // The exact same DOM elements are still connected — a re-created Player /
  // preview slot would have detached these (the regression).
  expect(await playerHandle!.evaluate((el) => el.isConnected)).toBe(true);
  expect(await videoHandle!.evaluate((el) => el.isConnected)).toBe(true);
  await expect(video).toBeVisible();
  // The Player is a single element, not duplicated.
  await expect(page.getByTestId("player")).toHaveCount(1);
});

test("a fader drag holds its dragged value across ticks and commits exactly one PATCH (#194)", async ({
  page,
  request,
}) => {
  const patches: Array<{ dabing?: number }> = [];
  await page.route("**/api/v1/mix", async (route) => {
    if (route.request().method() === "PATCH") {
      try {
        patches.push(JSON.parse(route.request().postData() ?? "{}"));
      } catch {
        patches.push({});
      }
    }
    await route.continue();
  });

  await setupDabingPlayer(page, request);
  const fader = page.getByTestId("mix-dabing");
  await expect(fader).toBeEnabled({ timeout: 15000 });

  // Begin a real pointer drag and move the fader to 40 %. `input` is the LAST
  // signal-mutating dispatch so the gate's dragged value settles at 40.
  await fader.evaluate((el: HTMLInputElement) => {
    el.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    el.value = "40";
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });

  // Hold the pointer down across ~4 position ticks. A live tick must NOT reset
  // the fader (before the fix the mixer was re-created / the gain overwritten).
  await page.waitForTimeout(2000);
  await expect(fader).toHaveValue("40");

  // Move to 70 % and release → exactly ONE PATCH carrying the final ratio.
  // Real browsers fire `pointerup` BEFORE `change` on a slider release, so
  // dispatch that order — it exercises the commit path that must read the drag
  // signal (not the DOM, which `pointerup` may have snapped back to live gain).
  await fader.evaluate((el: HTMLInputElement) => {
    el.value = "70";
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  });

  await expect.poll(() => patches.length, { timeout: 5000 }).toBeGreaterThan(0);
  // Give any spurious extra PATCH a chance to arrive, then assert exactly one.
  await page.waitForTimeout(1500);
  expect(patches.length).toBe(1);
  expect(patches[0].dabing).toBeCloseTo(0.7, 2);
  await expect(fader).toHaveValue("70");
});

test("a seek drag holds its dragged position across ticks and commits exactly one seek (#194)", async ({
  page,
  request,
}) => {
  const seekBodies: Array<{ position_ms?: number }> = [];
  await page.route("**/api/v1/playback/*/seek", async (route) => {
    try {
      seekBodies.push(JSON.parse(route.request().postData() ?? "{}"));
    } catch {
      seekBodies.push({});
    }
    await route.fulfill({ status: 204 });
  });

  await setupDabingPlayer(page, request);
  const seek = page.getByTestId("player-seek");
  await expect(seek).toBeEnabled({ timeout: 15000 });

  // Grab the seek bar and drag it to a fixed 100 000 ms (a clean multiple of the
  // 1000 ms step) while the live position keeps advancing under it.
  await seek.evaluate((el: HTMLInputElement) => {
    el.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    el.value = "100000";
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });

  await page.waitForTimeout(2000); // ~4 ticks advance the live position
  // The gate pins the dragged position — the advancing live position must NOT
  // snap the thumb back (the seek-bar regression).
  await expect(seek).toHaveValue("100000");

  // Release → exactly ONE seek POST with the dragged value. Real browsers fire
  // `pointerup` before `change`, so the commit must read the drag signal, not
  // the DOM (which may have snapped back to the advancing live position).
  await seek.evaluate((el: HTMLInputElement) => {
    el.dispatchEvent(new PointerEvent("pointerup", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
  });

  await expect.poll(() => seekBodies.length, { timeout: 5000 }).toBeGreaterThan(0);
  await page.waitForTimeout(1000);
  expect(seekBodies.length).toBe(1);
  expect(seekBodies[0].position_ms).toBe(100000);
});

test("a bare change with no preceding input commits nothing — the drag-gate dirty latch (#198)", async ({
  page,
  request,
}) => {
  // #198 item 1: after #200 the pointer release commits the drag, but a bare
  // `change` (a programmatic/stale dispatch, or a keyboard change that never
  // had an `input` in THIS session) still ran commit_seek(seek_drag_ms) /
  // commit(drag_pct) — and those signals start at 0, so it committed 0 ms / 0 %.
  // A `dirty` latch set by `on:input` must make a `change` with no prior input a
  // no-op. Here we dispatch a bare `change` on the seek bar and the dub fader
  // WITHOUT any preceding `input`/drag and assert NEITHER commits.
  const seekBodies: Array<{ position_ms?: number }> = [];
  await page.route("**/api/v1/playback/*/seek", async (route) => {
    try {
      seekBodies.push(JSON.parse(route.request().postData() ?? "{}"));
    } catch {
      seekBodies.push({});
    }
    await route.fulfill({ status: 204 });
  });
  const patches: Array<{ dabing?: number }> = [];
  await page.route("**/api/v1/mix", async (route) => {
    if (route.request().method() === "PATCH") {
      try {
        patches.push(JSON.parse(route.request().postData() ?? "{}"));
      } catch {
        patches.push({});
      }
    }
    await route.continue();
  });

  await setupDabingPlayer(page, request);
  const seek = page.getByTestId("player-seek");
  await expect(seek).toBeEnabled({ timeout: 15000 });
  const fader = page.getByTestId("mix-dabing");
  await expect(fader).toBeEnabled({ timeout: 15000 });

  // A bare `change` on each control — no `pointerdown`, no `input`, no drag.
  await seek.dispatchEvent("change");
  await fader.dispatchEvent("change");

  // Give any wrongly-fired request time to arrive, then assert none did.
  await page.waitForTimeout(1500);
  expect(seekBodies.length, "a bare change must not commit a seek").toBe(0);
  expect(patches.length, "a bare change must not commit a fader PATCH").toBe(0);
});
