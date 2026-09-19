import { test, expect } from "@playwright/test";

// E2E coverage for #177: the playlist song list marks each song's karaoke-stems
// state and offers a "len so stemami" filter so the operator can pick a
// stems-ready song to try karaoke with. The mock's "Worship" playlist (id=1)
// carries three videos with distinct stems states: id=1 ready, id=2 queued,
// id=3 unavailable.

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

async function openSongList(page) {
  await page.goto("/");
  const card = page.locator(".playlist-card", { hasText: "Worship" });
  await expect(card).toBeVisible({ timeout: 10000 });
  // #194: the song list is OPEN by default for the selected playlist.
  await expect(card.locator(".video-list")).toBeVisible({ timeout: 5000 });
  return card;
}

test("#177/#194: the song list marks each song's stems state via chip-stems", async ({
  page,
}) => {
  const card = await openSongList(page);

  // #194: the per-row stems marker is now the shared `chip-stems` inside the
  // row's status chips, with the Slovak label from `sp_core::status_chip`.
  const readyRow = card.locator('.song-row[data-video-id="1"]');
  await expect(readyRow.locator('[data-testid="chip-stems"]')).toHaveText(
    "hotové",
  );

  // video_id 3 carries the terminal-unsupported stems state.
  const unsupRow = card.locator('.song-row[data-video-id="3"]');
  await expect(unsupRow.locator('[data-testid="chip-stems"]')).toHaveText(
    "nedostupné",
  );
});

test("#177: the 'len so stemami' filter keeps only stems-ready songs", async ({
  page,
}) => {
  const card = await openSongList(page);

  // All three rows present before filtering.
  await expect(card.locator(".song-list .song-row")).toHaveCount(3);

  await card.locator('[data-testid="video-list-stems-filter"]').check();

  // Only the stems-ready song (id=1) remains.
  const rows = card.locator(".song-list .song-row");
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("Never Gonna Give You Up");

  // Unchecking restores the full list.
  await card.locator('[data-testid="video-list-stems-filter"]').uncheck();
  await expect(card.locator(".song-list .song-row")).toHaveCount(3);
});
