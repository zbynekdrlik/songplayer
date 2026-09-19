import { test, expect } from "@playwright/test";

// E2E coverage for #136 T1: the dashboard video list carries an inline
// song/artist editor so operators correct wall metadata the pipeline wrote
// wrong (Gemini-failed regex-fallback swaps / mojibake). The mock's
// "Worship" playlist (id=1) has a dedicated row (id=3, youtube nwmrD1k6yNE)
// seeded with the exact swap shape: song="planetboom", artist="P. Break!".
// This spec drives edit -> type -> save -> reload in the real browser and
// verifies both the PATCH request and the corrected values on the row.

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

async function openWorshipList(page) {
  await page.goto("/");
  const card = page.locator(".playlist-card", { hasText: "Worship" });
  await expect(card).toBeVisible({ timeout: 10000 });
  // #194: the song list is OPEN by default for the selected playlist.
  await expect(card.locator(".video-list")).toBeVisible({ timeout: 5000 });
  return card;
}

test("inline edit corrects a swapped song+artist and persists on reload (#136 T1)", async ({
  page,
}) => {
  const card = await openWorshipList(page);
  // #194: rows are `.song-row`; the edit-mode row keeps the same data-video-id
  // (as `.song-row.song-row-editing`), so this locator holds across the swap.
  const row = card.locator('.song-row[data-video-id="3"]');
  await expect(row).toBeVisible();

  // Enter edit mode.
  await row.locator('[data-testid="video-list-edit"]').click();

  const songInput = row.locator('[data-testid="video-list-edit-song"]');
  const artistInput = row.locator('[data-testid="video-list-edit-artist"]');
  await expect(songInput).toBeVisible();
  // Prefilled with the current (wrong) stored values.
  await expect(songInput).toHaveValue("planetboom");
  await expect(artistInput).toHaveValue("P. Break!");

  // Correct: the real song is the second half, the artist is the band.
  await songInput.fill("Break Every Chain");
  await artistInput.fill("planetboom");

  const patchPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/videos/3") && req.method() === "PATCH",
  );
  await row.locator('[data-testid="video-list-edit-save"]').click();

  const req = await patchPromise;
  const body = JSON.parse(req.postData() ?? "{}");
  expect(body.song).toBe("Break Every Chain");
  expect(body.artist).toBe("planetboom");

  // After the save-triggered reload the row shows the corrected values and
  // has left edit mode (the edit ✎ button is back).
  await expect(row.locator('[data-testid="video-list-edit"]')).toBeVisible({
    timeout: 5000,
  });
  // #194: the row is a flex `.song-row`, not a `<table>` — the corrected
  // song + artist render together in the shared `song-row-title`
  // ("Break Every Chain — planetboom").
  const titleLine = row.locator('[data-testid="song-row-title"]');
  await expect(titleLine).toContainText("Break Every Chain");
  await expect(titleLine).toContainText("planetboom");
});

test("cancel leaves the stored song/artist unchanged (#136 T1)", async ({
  page,
}) => {
  const card = await openWorshipList(page);
  const row = card.locator('.song-row[data-video-id="3"]');
  await expect(row).toBeVisible();

  await row.locator('[data-testid="video-list-edit"]').click();
  const songInput = row.locator('[data-testid="video-list-edit-song"]');
  await expect(songInput).toBeVisible();
  await songInput.fill("Should Not Persist");

  await row.locator('[data-testid="video-list-edit-cancel"]').click();

  // Back to read mode with the original value shown, no PATCH sent.
  await expect(songInput).toHaveCount(0);
  await expect(row.locator('[data-testid="video-list-edit"]')).toBeVisible();
  await expect(row).not.toContainText("Should Not Persist");
});
