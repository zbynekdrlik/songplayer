import { test, expect } from "@playwright/test";

// #194: the ONE shared playback surface. `components/player.rs::Player` is
// rendered identically on the Dashboard card, the Live page and the Dabing page.
// This spec proves the cross-page consistency (same testids everywhere), the
// real seek bar (POST /api/v1/playback/{id}/seek + ±10 s), and that the mixer
// slot follows the PLAYING item (karaoke adapter for a song, dub adapter for a
// dub video). Zero console errors, per browser-console-zero-errors.md.

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

// Every child testid the shared Player sets INSIDE the component — must resolve
// on every page that shows the Player.
const PLAYER_PARTS = [
  "player-title",
  "player-seek",
  "player-pos",
  "player-back10",
  "player-fwd10",
  "player-prev",
  "player-playpause",
  "player-skip",
  "player-mode",
];

test.describe("the shared Player is identical on every page (#194)", () => {
  for (const path of ["/", "/live", "/dabing"]) {
    test(`renders with the same parts on ${path}`, async ({ page }) => {
      await page.goto(path);
      await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
      for (const part of PLAYER_PARTS) {
        await expect(
          page.getByTestId(part),
          `${part} must exist on ${path}`,
        ).toBeVisible();
      }
      // The preview slot is always present (placeholder when idle / not started).
      await expect(page.getByTestId("preview-placeholder")).toBeVisible();
    });
  }
});

test.describe("the Player seek bar drives POST /playback/{id}/seek (#194)", () => {
  test("dragging the seek bar to ~50% posts position_ms; ±10 s fire seeks", async ({
    page,
  }) => {
    // Capture every seek body (playlist 1 plays on the Dashboard → seek enabled).
    const seekBodies: Array<{ position_ms?: number }> = [];
    await page.route("**/api/v1/playback/*/seek", async (route) => {
      const raw = route.request().postData() ?? "";
      try {
        seekBodies.push(JSON.parse(raw));
      } catch {
        seekBodies.push({});
      }
      await route.fulfill({ status: 204 });
    });

    await page.goto("/");
    const seek = page.getByTestId("player-seek");
    await expect(seek).toBeVisible({ timeout: 15000 });
    // Playlist 1 (duration 213000) starts playing → the seek bar enables once
    // its NowPlaying arrives.
    await expect(seek).toBeEnabled({ timeout: 15000 });

    // Drag to ~50% (a clean multiple of the 1000 ms step) and commit. #198 item
    // 6: Playwright's fill() already fires `input` + `change` on the input, so
    // the previous explicit dispatchEvent('change') was a redundant SECOND commit
    // path (masked before only by the value-dedup). Drop it and assert fill()
    // commits EXACTLY ONE seek.
    await seek.fill("106000");

    await expect.poll(() => seekBodies.length, { timeout: 5000 }).toBeGreaterThan(0);
    // Give any spurious extra seek a chance to arrive, then assert exactly one.
    await page.waitForTimeout(1000);
    expect(seekBodies.length, "fill() must commit exactly one seek").toBe(1);
    // ~50% of the 213000 ms duration, with tolerance for the step + any tick.
    const EXPECTED = Math.round(213000 / 2); // 106500
    expect(seekBodies[0].position_ms).toBeGreaterThan(EXPECTED - 15000);
    expect(seekBodies[0].position_ms).toBeLessThan(EXPECTED + 15000);

    // +10 s fires a seek.
    const beforeFwd = seekBodies.length;
    await page.getByTestId("player-fwd10").click();
    await expect
      .poll(() => seekBodies.length, { timeout: 5000 })
      .toBeGreaterThan(beforeFwd);

    // −10 s fires a seek.
    const beforeBack = seekBodies.length;
    await page.getByTestId("player-back10").click();
    await expect
      .poll(() => seekBodies.length, { timeout: 5000 })
      .toBeGreaterThan(beforeBack);
  });
});

test.describe("the Player mixer follows the playing item (#194)", () => {
  test("Dashboard shows the karaoke mixer for a song", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // Playlist 1 plays a plain song (no dabing rows on the dashboard) → the
    // Player mounts the karaoke adapter.
    await expect(page.getByTestId("karaoke-now-playing")).toBeVisible({
      timeout: 15000,
    });
    await expect(page.getByTestId("dub-mix-fader")).toHaveCount(0);
  });

  test("Dabing shows the dub mixer once a dub video plays", async ({
    page,
    request,
  }) => {
    // A ready dub video on the Dabing playlist (id 500).
    await request.post("/__mock/dabing-reset");
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 800,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
        dub_mix_ratio: 1.0,
      },
    });

    await page.goto("/dabing");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // Wait for the row to reach store.dabing (its list row renders).
    await expect(
      page.locator('[data-testid="dabing-list"] .song-row[data-video-id="800"]'),
    ).toBeVisible({ timeout: 10000 });

    // Before it plays, the mixer slot is the karaoke adapter (no dub fader).
    await expect(page.getByTestId("dub-mix-fader")).toHaveCount(0);

    // Make the dub video the playing item → the Player picks the dub adapter.
    await request.post("/__mock/now-playing", {
      data: {
        playlist_id: 500,
        video_id: 800,
        song: "Kázeň",
        duration_ms: 200000,
      },
    });

    await expect(page.getByTestId("dub-mix-fader")).toBeVisible({
      timeout: 10000,
    });
  });
});

test.describe("the Player preview slot (#194 / #178)", () => {
  test("a playing card offers the live-preview start control without MSE", async ({
    page,
  }) => {
    // The real MSE preview needs branded Chrome (H.264/AAC) and lives in
    // preview.spec.ts (the `chrome` project). Here we only assert the start
    // control is present while playing — we never click it, so no MSE starts.
    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("preview-start")).toBeVisible({
      timeout: 15000,
    });
  });
});
