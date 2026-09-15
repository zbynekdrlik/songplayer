import { test, expect } from "@playwright/test";

// #165: dashboard redesign — a playlist SELECTOR + ONE working area instead of
// a grid of every playlist card. The owner's complaint: 9 cards today, 50
// tomorrow = unmanageable; the normal state is "one playlist plays" and the
// operator wants to pick the one they work with and see which one is playing.
//
// These specs run against the mock's opt-in 12-playlist fixture (playlist id 1
// "Playlist 01" is the currently-playing one — the mock's WS marks playlist 1
// Playing). Selection lives in `store.selected_playlist`, mirrored to the URL
// query `?playlist=<id>` and localStorage.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  // Opt into the 12-playlist fixture for this file only.
  const set = await request.post("/__mock/fixture", { data: { mode: "twelve" } });
  expect(set.ok()).toBeTruthy();
});

test.afterEach(async ({ request }) => {
  // Reset so the serially-run sibling spec files see the default 3 playlists.
  await request.post("/__mock/fixture", { data: { mode: "default" } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

// The playing playlist's row: playlist 1 = "Playlist 01".
const PLAYING_NAME = "Playlist 01";
const PLAYING_SONG = "Never Gonna Give You Up";

async function waitForSelector12(page: import("@playwright/test").Page) {
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 15000,
  });
  await expect(page.getByTestId("playlist-selector-row")).toHaveCount(12, {
    timeout: 15000,
  });
}

test("renders exactly one work area and a selector with 12 rows (#165)", async ({
  page,
}) => {
  await page.goto("/");
  await waitForSelector12(page);

  // Exactly ONE working area on the page — not a grid of cards.
  await expect(page.getByTestId("playlist-workspace")).toHaveCount(1);
  // And exactly one playlist card inside it (the selected playlist).
  await expect(page.locator(".playlist-card")).toHaveCount(1);
});

test("the playing playlist is preselected and marked ▶ (#165)", async ({
  page,
}) => {
  await page.goto("/");
  await waitForSelector12(page);

  const playingRow = page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: PLAYING_NAME });
  // The playing row is marked ▶ and selected once the WS Playing state lands.
  await expect(playingRow).toContainText("▶", { timeout: 10000 });
  await expect(playingRow).toHaveClass(/selected/, { timeout: 10000 });

  // The work area shows the playing playlist.
  await expect(page.getByTestId("workspace-title")).toHaveText(PLAYING_NAME, {
    timeout: 10000,
  });
});

test("clicking another row switches the work area and the URL (#165)", async ({
  page,
}) => {
  await page.goto("/");
  await waitForSelector12(page);

  // Preselected playing playlist first shows its now-playing song.
  await expect(page.getByTestId("workspace-title")).toHaveText(PLAYING_NAME, {
    timeout: 10000,
  });
  await expect(page.locator(".playlist-card .np-song")).toContainText(
    PLAYING_SONG,
    { timeout: 10000 },
  );

  // Click a different playlist row.
  await page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Playlist 05" })
    .click();

  // Work area switches to that playlist; URL mirrors the selection.
  await expect(page.getByTestId("workspace-title")).toHaveText("Playlist 05");
  await expect(page).toHaveURL(/[?&]playlist=5\b/);
  // Playlist 05 is not playing, so its work-area card shows the idle state
  // (not the playing playlist's song).
  await expect(page.locator(".playlist-card")).toContainText("Nothing playing");
});

test("the Práve hrá strip leads back to the playing playlist (#165)", async ({
  page,
}) => {
  await page.goto("/");
  await waitForSelector12(page);
  await expect(page.getByTestId("workspace-title")).toHaveText(PLAYING_NAME, {
    timeout: 10000,
  });

  // Select a DIFFERENT playlist so a different one is playing than selected.
  await page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Playlist 07" })
    .click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Playlist 07");

  // The strip now advertises the playing playlist + a "Prejsť" button.
  const strip = page.getByTestId("now-playing-strip");
  await expect(strip).toContainText(PLAYING_NAME);
  await expect(strip).toContainText("▶");

  // Clicking "Prejsť" selects the playing playlist.
  await page.getByTestId("strip-goto").click();
  await expect(page.getByTestId("workspace-title")).toHaveText(PLAYING_NAME);
  await expect(page).toHaveURL(/[?&]playlist=1\b/);
});

test("reload keeps the selected playlist (#165)", async ({ page }) => {
  await page.goto("/");
  await waitForSelector12(page);

  await page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Playlist 09" })
    .click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Playlist 09");
  await expect(page).toHaveURL(/[?&]playlist=9\b/);

  // Reload — the selection survives (URL query + localStorage), NOT reset to
  // the playing playlist.
  await page.reload();
  await waitForSelector12(page);
  await expect(page.getByTestId("workspace-title")).toHaveText("Playlist 09");
  await expect(
    page.getByTestId("playlist-selector-row").filter({ hasText: "Playlist 09" }),
  ).toHaveClass(/selected/);
});

test("at 400px the selector becomes a <select> above the work area (#165)", async ({
  page,
}) => {
  await page.setViewportSize({ width: 400, height: 900 });
  await page.goto("/");
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 15000,
  });

  // The mobile dropdown is visible; the desktop row list is hidden.
  const select = page.getByTestId("playlist-select");
  await expect(select).toBeVisible({ timeout: 10000 });
  await expect(select.locator("option")).toHaveCount(12);
  await expect(page.getByTestId("playlist-selector-list")).toBeHidden();

  // Picking another playlist in the dropdown switches the work area.
  await select.selectOption("6");
  await expect(page.getByTestId("workspace-title")).toHaveText("Playlist 06");
  await expect(page).toHaveURL(/[?&]playlist=6\b/);

  // The page body must never scroll horizontally on a phone: the top navbar
  // (page buttons + ws dot + version) overflowed to 588 px on the live box.
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - window.innerWidth,
  );
  expect(overflow, "horizontal overflow in px at 400px width").toBeLessThanOrEqual(0);
});
