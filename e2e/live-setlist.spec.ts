import { test, expect } from "@playwright/test";

// #194: the /live global transport bar (`.live-setlist-controls` / the
// `.live-setlist-mode` select) is GONE — Pause / Skip / Previous / Play / Mode
// moved to the ONE shared <Player/> that sits under the setlist. These specs
// drive the Player's transport for the ytlive playlist (id 184) and assert each
// control fires its playback endpoint.
//
// #94 ("a failing endpoint must not be a silent no-op") lives on: the shared
// Player reports every failed transport POST into `[data-testid="player-error"]`
// (`report(...)` in components/player.rs). The `#94 error surface` block below
// drives each command through the mock's `/__mock/fail-mode` hook and asserts
// the Slovak error line — play / previous / mode on /live (idle), pause on the
// Dashboard where playlist 1 is Playing (the toggle reads "⏸ Pauza" there).

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

test.describe("the shared Player transport drives /live playback (#194)", () => {
  test("▶ Prehrať posts to /play (ytlive is not playing → play)", async ({
    page,
  }) => {
    await page.goto("/live");
    const btn = page.getByTestId("player-playpause");
    await expect(btn).toBeVisible({ timeout: 10000 });
    // ytlive (184) has no now-playing/Playing state, so the toggle reads
    // "▶ Prehrať" and clicking it posts /play.
    await expect(btn).toContainText("Prehrať");
    const post = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/playback/184/play") &&
        req.method() === "POST",
    );
    await btn.click();
    await post;
  });

  test("⏭ Ďalšia posts to /skip", async ({ page }) => {
    await page.goto("/live");
    const btn = page.getByTestId("player-skip");
    await expect(btn).toBeVisible({ timeout: 10000 });
    const post = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/playback/184/skip") &&
        req.method() === "POST",
    );
    await btn.click();
    await post;
  });

  test("⏮ Predošlá posts to /previous", async ({ page }) => {
    await page.goto("/live");
    const btn = page.getByTestId("player-prev");
    await expect(btn).toBeVisible({ timeout: 10000 });
    const post = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/playback/184/previous") &&
        req.method() === "POST",
    );
    await btn.click();
    await post;
  });

  test("the mode select PUTs the chosen mode", async ({ page }) => {
    await page.goto("/live");
    const sel = page.getByTestId("player-mode");
    await expect(sel).toBeVisible({ timeout: 10000 });
    // The select defaults to "continuous" (PlaybackMode::default); pick "loop"
    // so a real change event fires. The predicate matches the body so it can't
    // be satisfied by the page's mount-time `mode=single` PUT.
    const put = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/playback/184/mode") &&
        req.method() === "PUT" &&
        (req.postData() ?? "").includes("loop"),
    );
    await sel.selectOption("loop");
    await put;
  });
});

// #94 error surface — every transport command that fails must show up.
test.describe("#94: a failing transport POST surfaces in player-error", () => {
  // The mock's fail-mode is global in-memory state; always reset it.
  test.afterEach(async ({ request }) => {
    for (const kind of ["play", "pause", "previous", "mode"]) {
      await request.post("/__mock/fail-mode", { data: { kind, enabled: false } });
    }
  });

  async function fail(request: import("@playwright/test").APIRequestContext, kind: string) {
    await request.post("/__mock/fail-mode", { data: { kind, enabled: true } });
  }

  test("play fails → 'Prehrávanie zlyhalo'", async ({ page, request }) => {
    await fail(request, "play");
    await page.goto("/live");
    const btn = page.getByTestId("player-playpause");
    await expect(btn).toContainText("Prehrať", { timeout: 10000 });
    await expect(page.locator('[data-testid="player-error"]')).toHaveCount(0);
    await btn.click();
    await expect(page.locator('[data-testid="player-error"]')).toContainText(
      "Prehrávanie zlyhalo",
      { timeout: 5000 },
    );
  });

  test("previous fails → 'Predošlá zlyhala'", async ({ page, request }) => {
    await fail(request, "previous");
    await page.goto("/live");
    const prev = page.getByTestId("player-prev");
    await expect(prev).toBeVisible({ timeout: 10000 });
    await prev.click();
    await expect(page.locator('[data-testid="player-error"]')).toContainText(
      "Predošlá zlyhala",
      { timeout: 5000 },
    );
  });

  test("mode change fails → 'Zmena režimu zlyhala'", async ({ page, request }) => {
    await fail(request, "mode");
    await page.goto("/live");
    const mode = page.getByTestId("player-mode");
    await expect(mode).toBeVisible({ timeout: 10000 });
    await mode.selectOption("loop");
    await expect(page.locator('[data-testid="player-error"]')).toContainText(
      "Zmena režimu zlyhala",
      { timeout: 5000 },
    );
  });

  test("pause fails → 'Pauza zlyhala' (Dashboard, playlist 1 is Playing)", async ({
    page,
    request,
  }) => {
    await fail(request, "pause");
    await page.goto("/");
    const btn = page.getByTestId("player-playpause");
    await expect(btn).toContainText("Pauza", { timeout: 15000 });
    await btn.click();
    await expect(page.locator('[data-testid="player-error"]')).toContainText(
      "Pauza zlyhala",
      { timeout: 5000 },
    );
  });
});
