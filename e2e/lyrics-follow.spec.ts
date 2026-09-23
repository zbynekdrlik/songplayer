import { test, expect, Page, APIRequestContext } from "@playwright/test";

// #184 round F — the subtitle/lyrics panel must FOLLOW the spoken line. The
// owner (2026-09-23): "hovorena veta v subtitles okne sa neposuva takze musim
// mysou scrolovat". Root cause: the shared `LyricsView` only toggled the
// `lyr-current` class; its 260 px scroller (`.lyrics-view-scroll`) was never
// scrolled, so the active line walked out of view.
//
// This spec drives the Dabing Player (playlist 500, the dub video 344) with the
// mock's 500 ms position TICK advancing one lyric line per tick over a 100-line
// fixture track (`/__mock/lyrics-mode` "long"), and proves with the REAL
// browser:
//   1. once the active line has travelled far past the first screenful, the
//      `.lyr-current` line is inside the scroller's visible rect;
//   2. only the panel scrolls — `window.scrollY` never moves (no
//      `scrollIntoView`, which would also scroll the page);
//   3. a real mouse wheel on the panel pauses the follow: the panel stays where
//      the operator put it for ~4 s while the line keeps advancing, then the
//      follow resumes on its own;
//   4. zero console errors / warnings (the shared afterEach).
// The component is shared (Prehľad / Naživo / Dabing / Texty), so one proof on
// one page covers the one behaviour everywhere.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const DABING_PLAYLIST_ID = 500;
const DUB_VIDEO_ID = 344;
// One fixture line every 2000 ms; the tick advances 2000 ms per 500 ms tick, so
// the active line moves one line per tick (2 lines / s).
const LINE_MS = 2000;
const DURATION_MS = 200000;

let consoleMessages: string[] = [];

test.use({ viewport: { width: 1400, height: 1000 } });

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
  // Global mock state: tick OFF, lyrics back to the default 2-line track, the
  // added dub row + the mix memories back to their defaults.
  await request.post("/__mock/tick", { data: { enabled: false } });
  await request.post("/__mock/lyrics-mode", { data: { mode: "track" } });
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

/** Geometry of the Player's lyrics scroller and its active line. */
async function panelState(page: Page) {
  return page.evaluate(() => {
    const scroller = document.querySelector(
      '[data-testid="player"] [data-testid="lyrics-view"]',
    ) as HTMLElement | null;
    if (!scroller) return null;
    const lines = Array.from(scroller.querySelectorAll(".lyrics-list > li"));
    const current = scroller.querySelector(".lyr-current") as HTMLElement | null;
    const idx = current ? lines.findIndex((li) => li.contains(current)) : -1;
    const sc = scroller.getBoundingClientRect();
    const cur = current ? current.getBoundingClientRect() : null;
    return {
      idx,
      scrollTop: scroller.scrollTop,
      windowScrollY: window.scrollY,
      inside:
        cur !== null && cur.top >= sc.top - 1 && cur.bottom <= sc.bottom + 1,
    };
  });
}

/** The Dabing Player with a ready dub PLAYING, the long lyrics track, and the
 *  tick advancing one line per 500 ms tick (or, with `stepMs: 0`, a track that
 *  sits still at `positionMs` — a paused track). */
async function setupFollowingPanel(
  page: Page,
  request: APIRequestContext,
  { positionMs = 0, stepMs = LINE_MS }: { positionMs?: number; stepMs?: number } = {},
) {
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/lyrics-mode", { data: { mode: "long" } });
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
          duration_ms: DURATION_MS,
          position_ms: positionMs,
          step_ms: stepMs,
          state: "Playing",
        },
      ],
    },
  });
  const panel = page.getByTestId("player").getByTestId("lyrics-view");
  // The whole 100-line list rendered inside the panel.
  await expect(panel.locator(".lyrics-list > li")).toHaveCount(100, {
    timeout: 15000,
  });
  // Bring the panel on screen ONCE (this may scroll the window); every window
  // position read after this must stay put.
  await panel.scrollIntoViewIfNeeded();
  await page.waitForTimeout(300);
  return panel;
}

test.describe("#184 round F: the lyrics panel follows the spoken line", () => {
  test("the active line stays visible inside the panel, and only the panel scrolls", async ({
    page,
    request,
  }) => {
    test.setTimeout(45_000);
    await setupFollowingPanel(page, request);
    // Make the PAGE scrollable no matter how tall the Dabing layout renders at
    // this viewport, so a `scrollIntoView`-style regression (which scrolls the
    // window too) would really move `window.scrollY` — the guard below must be
    // able to fail.
    const scrollable = await page.evaluate(() => {
      document.body.style.paddingBottom = "3000px";
      const el = document.scrollingElement ?? document.documentElement;
      return el.scrollHeight > window.innerHeight;
    });
    expect(scrollable).toBe(true);
    const start = await panelState(page);
    expect(start).not.toBeNull();
    const windowY = start!.windowScrollY;

    // Wait until the active line is far past the first screenful (~4 lines fit
    // the 260 px panel; line 12 is well below it without auto-follow).
    await expect
      .poll(async () => (await panelState(page))?.idx ?? -1, { timeout: 15000 })
      .toBeGreaterThanOrEqual(12);

    // (1) The panel followed: the active line is inside the scroller's rect
    // (poll — the follow is a smooth scroll) and the panel really scrolled.
    await expect
      .poll(async () => (await panelState(page))?.inside ?? false, {
        timeout: 3000,
      })
      .toBe(true);
    const followed = await panelState(page);
    expect(followed!.scrollTop).toBeGreaterThan(0);

    // (2) Only the panel scrolled — the page did not move.
    expect(followed!.windowScrollY).toBe(windowY);
  });

  test("a real mouse wheel on the panel pauses the follow ~5 s, then it follows again", async ({
    page,
    request,
  }) => {
    test.setTimeout(45_000);
    const panel = await setupFollowingPanel(page, request);

    // Let the follow get going first (the panel is scrolled down to line ~10).
    await expect
      .poll(async () => (await panelState(page))?.idx ?? -1, { timeout: 15000 })
      .toBeGreaterThanOrEqual(10);
    await expect
      .poll(async () => (await panelState(page))?.inside ?? false, {
        timeout: 3000,
      })
      .toBe(true);
    // The operator scrolls the panel UP with the real mouse wheel. The
    // pre-wheel scrollTop is read right before the wheel, and the wheel (-300)
    // is far larger than one in-flight follow step (~1 line), so the net upward
    // move is unambiguous.
    const box = await panel.boundingBox();
    if (!box) throw new Error("no bounding box for the lyrics panel");
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    const beforeWheel = (await panelState(page))!.scrollTop;
    expect(beforeWheel).toBeGreaterThan(150);
    const t0 = Date.now();
    await page.mouse.wheel(0, -300);
    await page.waitForTimeout(500);
    const held = (await panelState(page))!;
    expect(held.scrollTop).toBeLessThan(beforeWheel - 100);
    const idxAtWheel = held.idx;

    // (3a) Until 3.5 s after the wheel the panel stays exactly where the
    // operator left it, even though the active line keeps advancing
    // (2 lines / s). Time-bounded (not a fixed sample count) so slow
    // `page.evaluate` round-trips can never run the window past the 5 s pause.
    while (Date.now() - t0 < 3500) {
      await page.waitForTimeout(250);
      const s = (await panelState(page))!;
      expect(Math.abs(s.scrollTop - held.scrollTop)).toBeLessThanOrEqual(1);
    }
    const duringPause = (await panelState(page))!;
    // Still inside the pause window when the hold was last read.
    expect(Date.now() - t0).toBeLessThan(4500);
    expect(duringPause.idx).toBeGreaterThan(idxAtWheel + 4);
    // The hold was real: the advancing line left the view and the panel did
    // NOT chase it.
    expect(duringPause.inside).toBe(false);

    // (3b) After the 5 s pause the follow resumes on its own: the (by now far
    // advanced) active line is back inside the panel.
    await expect
      .poll(async () => (await panelState(page))?.inside ?? false, {
        timeout: 5000,
      })
      .toBe(true);
    const resumed = (await panelState(page))!;
    expect(resumed.scrollTop).toBeGreaterThan(held.scrollTop + 50);
  });

  test("a track paused mid-way opens with its active line already in view", async ({
    page,
    request,
  }) => {
    // Round-F review: the first follow after a lyrics (re)load can race the
    // `<ol>` render (the active-line Memo wakes the follow before the list is
    // in the DOM). A PLAYING track recovers at its next line change; a track
    // sitting still never produces one — so the follow must also re-run when
    // the list mounts. Line 31 (60 s in) is far below the first screenful.
    test.setTimeout(45_000);
    await setupFollowingPanel(page, request, { positionMs: 30 * LINE_MS, stepMs: 0 });
    await expect
      .poll(async () => (await panelState(page))?.idx ?? -1, { timeout: 15000 })
      .toBe(30);
    await expect
      .poll(async () => (await panelState(page))?.inside ?? false, {
        timeout: 5000,
      })
      .toBe(true);
    const s = (await panelState(page))!;
    expect(s.idx).toBe(30);
    expect(s.scrollTop).toBeGreaterThan(0);
  });
});
