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
  await card.locator('[data-testid="playlist-songs-toggle"]').click();
  await expect(card.locator(".video-list")).toBeVisible({ timeout: 5000 });
  return card;
}

test("#177: the song list marks each song's stems state", async ({ page }) => {
  const card = await openSongList(page);

  const readyRow = card.locator(".video-list tbody tr", {
    hasText: "Never Gonna Give You Up",
  });
  await expect(
    readyRow.locator('[data-testid="video-list-stems-marker"]'),
  ).toHaveAttribute("data-stems-state", "ready");

  const unsupRow = card.locator(".video-list tbody tr", {
    hasText: "Break Every Chain",
  });
  await expect(
    unsupRow.locator('[data-testid="video-list-stems-marker"]'),
  ).toHaveAttribute("data-stems-state", "unavailable");
});

test("#177: the 'len so stemami' filter keeps only stems-ready songs", async ({
  page,
}) => {
  const card = await openSongList(page);

  // All three rows visible before filtering.
  await expect(card.locator(".video-list tbody tr")).toHaveCount(3);

  await card.locator('[data-testid="video-list-stems-filter"]').check();

  // Only the stems-ready song (id=1) remains.
  const rows = card.locator(".video-list tbody tr");
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText("Never Gonna Give You Up");

  // Unchecking restores the full list.
  await card.locator('[data-testid="video-list-stems-filter"]').uncheck();
  await expect(card.locator(".video-list tbody tr")).toHaveCount(3);
});
