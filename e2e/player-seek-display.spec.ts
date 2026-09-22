import { test, expect, Page, APIRequestContext } from "@playwright/test";

// #184 — the seek bar must never jump BACKWARDS after a commit. The owner's
// re-test: "ked som dal pretocit tak … fader … skocil spat a potom na miesto".
// Root cause: on release the drag gate cleared and the bar snapped to the LIVE
// now-playing position, which is still the PRE-seek position for ~2-3 s until
// the pipeline's post-seek fast-forward catches up — so the thumb visibly jumped
// back to the stale position and then forward.
//
// This spec drives a REAL mouse drag (the `player-mouse.spec.ts` pattern:
// pointerup BEFORE change) with the mock's 500 ms position tick running AND the
// new `seek_hold_ms` knob keeping the tick position STALE for 2 s after the seek
// POST (mirroring the real fast-forward latency). It asserts the displayed
// position (the `player-seek` value + the readout) never drops below the seek
// target during that 2 s window, then follows the live position once the mock's
// fast-forward lands. Runs in the CHROMIUM project (codec-independent — no
// preview MSE decode needed).

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
const SEEK_HOLD_MS = 2000;

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

/** The pure `sp_core::seek_model::format_position` mirror (M:SS). */
function formatPosition(ms: number): string {
  const totalSecs = Math.floor(ms / 1000);
  return `${Math.floor(totalSecs / 60)}:${String(totalSecs % 60).padStart(2, "0")}`;
}

/** Bring up the Dabing Player with a ready dub PLAYING off-program and the tick
 *  running with a `seek_hold_ms` post-seek stale window. */
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
      seek_hold_ms: SEEK_HOLD_MS,
      items: [
        {
          playlist_id: DABING_PLAYLIST_ID,
          video_id: DUB_VIDEO_ID,
          song: "Morning Prayer",
          duration_ms: DURATION_MS,
          position_ms: 0,
          state: "Playing",
        },
      ],
    },
  });
}

/** Real mouse drag along a horizontal range input from `from` to `to` (fractions). */
async function mouseDrag(page: Page, selector: string, from: number, to: number) {
  await page.locator(selector).scrollIntoViewIfNeeded();
  const box = await page.locator(selector).boundingBox();
  if (!box) throw new Error(`no bounding box for ${selector}`);
  const pt = (f: number) => ({ x: box.x + box.width * f, y: box.y + box.height / 2 });
  const a = pt(from);
  const b = pt(to);
  await page.mouse.move(a.x, a.y);
  await page.mouse.down();
  await page.mouse.move(b.x, b.y, { steps: 8 });
  await page.mouse.up();
}

test.describe("#184: the seek bar holds the committed target and never jumps back", () => {
  test("after a real-mouse seek the displayed position never drops below the target during the fast-forward window, then follows live", async ({
    page,
    request,
  }) => {
    test.setTimeout(45_000);

    // Record the committed seek target but let the POST reach the mock so it
    // schedules the 2 s stale hold + jump.
    let seekTarget: number | null = null;
    await page.route("**/api/v1/playback/*/seek", async (route) => {
      const body = JSON.parse(route.request().postData() || "{}");
      if (typeof body.position_ms === "number") seekTarget = body.position_ms;
      await route.continue();
    });

    await setupDabingPlayer(page, request);
    const seek = page.getByTestId("player-seek");
    const pos = page.getByTestId("player-pos");
    await expect(seek).toBeEnabled({ timeout: 15000 });

    // Let a few ticks advance the live position so the PRE-seek position is
    // clearly non-zero and far below the forward-seek target.
    await page.waitForTimeout(1000);
    const preDrag = Number(await seek.inputValue());

    // Real mouse drag forward to ~60 % of the duration.
    await mouseDrag(page, '[data-testid="player-seek"]', 0.1, 0.6);

    // The commit fired exactly one seek POST — capture its target.
    await expect.poll(() => seekTarget, { timeout: 5000 }).not.toBeNull();
    const target = seekTarget as number;
    expect(target).toBeGreaterThan(preDrag + 1500);

    // Well inside the 2 s hold the readout shows the TARGET time, not the stale
    // live time (this is what the owner saw jump back before the fix).
    await page.waitForTimeout(500);
    const readout = (await pos.textContent()) ?? "";
    expect(
      readout.startsWith(formatPosition(target)),
      `the readout must show the committed target (${formatPosition(target)}) during the hold, got "${readout}"`,
    ).toBe(true);

    // Sample the bar across the rest of the hold window: it must NEVER drop below
    // the committed target (before the fix it snapped to the stale live position).
    let minValue = Number.POSITIVE_INFINITY;
    const start = Date.now();
    while (Date.now() - start < 1200) {
      minValue = Math.min(minValue, Number(await seek.inputValue()));
      await page.waitForTimeout(150);
    }
    expect(
      minValue,
      "the displayed seek position must never drop below the committed target during the fast-forward window",
    ).toBeGreaterThanOrEqual(target);

    // After the hold the mock jumps the live position to the target and keeps
    // advancing — the bar must FOLLOW it (not stay frozen at the target).
    await expect
      .poll(async () => Number(await seek.inputValue()), {
        timeout: 6000,
        message:
          "once the pipeline caught up the bar must follow the advancing live position past the target",
      })
      .toBeGreaterThan(target);
  });
});
