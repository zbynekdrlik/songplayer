import { test, expect } from "@playwright/test";

// E2E coverage for #180 (dubbing D1): the Dabing section page (paste-URL add →
// queued row + Prehrať) and the per-row Dabing toggle on a playlist's song list.
// Opens the real dashboard, interacts with the UI, and asserts BOTH the visible
// result AND the backend effect the mock recorded, per e2e-real-user-testing.md.

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
  // Clean slate for the shared mock process (serial workers).
  await request.post("/__mock/dabing-reset");
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("Dabing page renders with the nav entry and empty state (#180)", async ({
  page,
}) => {
  await page.goto("/");
  await page.locator(".navbar button", { hasText: "Dabing" }).click();
  await expect(page.locator(".dabing-page h2")).toHaveText("Dabing");
  await expect(page.locator('[data-testid="dabing-import-input"]')).toBeVisible();
  await expect(page.locator(".dabing-empty")).toBeVisible();
});

test("pasting a URL adds a queued row and Prehrať dispatches play (#180)", async ({
  page,
}) => {
  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  await page
    .locator('[data-testid="dabing-import-input"]')
    .fill("https://youtu.be/AvWOCj48pGw");
  await page.locator('[data-testid="dabing-import-btn"]').click();

  // A queued row appears (import + poll refresh).
  const row = page.locator('[data-testid="dabing-list"] .dabing-row').first();
  await expect(row).toBeVisible({ timeout: 5000 });
  // A queued row renders the glyph chain (not an error) with the first step
  // ("stiahnuté") marked done — the `.dabing-step-done` class is what actually
  // conveys the queued state, so assert it rather than just the label text.
  const chain = row.locator('[data-testid="dabing-chain"]');
  await expect(chain.locator(".dabing-chain-error")).toHaveCount(0);
  await expect(chain.locator(".dabing-step-done").first()).toHaveText(
    "stiahnuté",
  );

  // Prehrať dispatches a play-video POST to the Dabing playlist.
  const postPromise = page.waitForRequest(
    (req) =>
      /\/api\/v1\/playlists\/\d+\/play-video/.test(req.url()) &&
      req.method() === "POST",
  );
  await row.locator('[data-testid="dabing-play"]').click();
  const req = await postPromise;
  expect(req.url()).toMatch(/\/play-video$/);
});

test("a dub-ready video WITHOUT stems renders pripravené (#183 round 2)", async ({
  page,
  request,
}) => {
  // Round 2: long videos the stem worker cannot separate are dubbed via the
  // 2-stream mix and reach `ready` with NO stems. The Dabing page must render
  // `pripravené` for such a row exactly like a stemmed one — the ready state is
  // first-class regardless of stems.
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 344,
      title: "Morning Prayer & Devotion",
      dub_status: "ready",
      chain_state: "ready",
      stem_status: null, // no stems — the 2-stream DubOverOriginal case
      dub_file_path: "/c/morning_dub.flac",
    },
  });

  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  // The row appears via the 2 s poll; its glyph chain is NOT an error and its
  // final step (`pripravené`) is marked done.
  const row = page.locator('[data-testid="dabing-list"] .dabing-row').first();
  await expect(row).toBeVisible({ timeout: 5000 });
  const chain = row.locator('[data-testid="dabing-chain"]');
  await expect(chain.locator(".dabing-chain-error")).toHaveCount(0);
  await expect(chain.locator(".dabing-step-done").last()).toHaveText(
    "pripravené",
  );
});

test("the Dabing row toggle flips dub_requested via PATCH (#180)", async ({
  page,
  request,
}) => {
  await page.goto("/");
  const card = page.locator(".playlist-card", { hasText: "Worship" });
  await expect(card).toBeVisible({ timeout: 10000 });

  // Expand the song list.
  await card.locator('[data-testid="playlist-songs-toggle"]').click();
  await expect(card.locator(".video-list")).toBeVisible({ timeout: 5000 });

  const row = card.locator(".video-list tbody tr", {
    hasText: "Never Gonna Give You Up",
  });
  await expect(row).toBeVisible();

  const patchPromise = page.waitForRequest(
    (req) =>
      /\/api\/v1\/videos\/\d+\/dub$/.test(req.url()) && req.method() === "PATCH",
  );
  await row.locator('[data-testid="dub-toggle"]').check();
  await patchPromise;

  // Backend effect: the mock recorded the toggle as requested=true.
  const last = await (await request.get("/__mock/dub-toggle-last")).json();
  expect(last.toggle.requested).toBe(true);
});
