import { test, expect } from "@playwright/test";

// E2E coverage for #134: every playlist (not just /live's ytlive) gets a
// dashboard picker to jump straight to a specific cached song. The mock's
// "Worship" playlist (id=1) has two videos: id=1 (normalized, cached) and
// id=2 (cached=false, not yet normalized) — this spec exercises both the
// happy path and the disabled-until-ready state.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
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

test("play button on a regular (non-live) playlist's song list dispatches play-video (#134)", async ({
  page,
}) => {
  await page.goto("/");
  const card = page.locator(".playlist-card", { hasText: "Worship" });
  await expect(card).toBeVisible({ timeout: 10000 });

  // Song list is collapsed by default — expand it.
  await card.locator('[data-testid="playlist-songs-toggle"]').click();
  await expect(card.locator(".video-list")).toBeVisible({ timeout: 5000 });

  const row = card.locator(".video-list tbody tr", {
    hasText: "Never Gonna Give You Up",
  });
  await expect(row).toBeVisible();

  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/playlists/1/play-video") &&
      req.method() === "POST",
  );
  await row.locator('[data-testid="video-list-play"]').click();
  const req = await postPromise;
  const body = JSON.parse(req.postData() ?? "{}");
  expect(body.video_id).toBe(1);
  expect(body.position_ms).toBeUndefined();
});

test("play button is disabled for a not-yet-normalized song (#134)", async ({
  page,
}) => {
  await page.goto("/");
  const card = page.locator(".playlist-card", { hasText: "Worship" });
  await expect(card).toBeVisible({ timeout: 10000 });

  await card.locator('[data-testid="playlist-songs-toggle"]').click();
  const row = card.locator(".video-list tbody tr", {
    hasText: "Amazing Grace",
  });
  await expect(row).toBeVisible({ timeout: 5000 });
  await expect(row.locator('[data-testid="video-list-play"]')).toBeDisabled();
});
