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

// #170: the selector must NOT re-order when a playlist's playback state
// changes. Rows are alphabetical and STABLE; the ▶ glyph marks the playing
// one. Before the fix the selector sorted playing-first and full-re-rendered
// on every now_playing tick, so a row that flipped to Playing jumped to the
// top — the click race that failed post-deploy test 16.
test("row order stays alphabetical when a non-first playlist starts playing (#170)", async ({
  page,
  request,
}) => {
  await page.goto("/");
  await waitForSelector12(page);

  const rowNames = async () =>
    page
      .getByTestId("playlist-selector-row")
      .evaluateAll((els) =>
        els.map((e) => e.querySelector(".sel-name")?.textContent?.trim() ?? ""),
      );

  // Baseline order is the alphabetical Playlist 01..12 (playlist 1 already
  // plays, but it is also first alphabetically, so the order is unambiguous).
  const before = await rowNames();
  expect(before).toEqual([...before].sort());

  // Flip a NON-first playlist (Playlist 07) to Playing.
  const set = await request.post("/__mock/set-playing", {
    data: { playlist_id: 7 },
  });
  expect(set.ok()).toBeTruthy();

  // Its row gains the ▶ glyph in place...
  const playing07 = page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Playlist 07" });
  await expect(playing07).toContainText("▶", { timeout: 10000 });

  // ...but the row ORDER is unchanged — Playlist 07 did NOT jump to the top.
  await expect
    .poll(async () => (await rowNames()).join("|"), { timeout: 5000 })
    .toBe(before.join("|"));
});

// #170 round 3: a playlist that has a live playback STATE but no NowPlaying
// (the PlaybackStateChanged-only shape — video_id 0, empty song, zero
// duration) must render the idle "Nothing playing" state, NOT a bogus
// np-info "0:00 / 0:00" block. Otherwise the post-deploy position-advance
// check reads that empty entry (0 → 0) as if a song were playing, and the
// operator sees a card claiming playback for a paused source.
test("an entry with no song and zero duration renders idle, not 0:00/0:00 (#170)", async ({
  page,
  request,
}) => {
  await page.goto("/");
  await waitForSelector12(page);

  // Give a non-playing playlist (Playlist 08) a live state with NO preceding
  // NowPlaying, so the store inserts the empty zero entry.
  const set = await request.post("/__mock/set-playing", {
    data: { playlist_id: 8, state: "WaitingForScene" },
  });
  expect(set.ok()).toBeTruthy();

  // Select that playlist's work area.
  await page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Playlist 08" })
    .click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Playlist 08");

  const card = page.locator(".playlist-card");
  // #194: the empty entry must render the idle Player — the title shows the
  // "Nič nehrá" idle label and the seek bar is disabled (no now-playing
  // content), NOT a bogus "0:00 / 0:00" now-playing block.
  await expect(card.getByTestId("player-title")).toHaveText("Nič nehrá");
  await expect(card.getByTestId("player-seek")).toBeDisabled();
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
  await expect(
    page.locator(".playlist-card").getByTestId("player-title"),
  ).toContainText(PLAYING_SONG, { timeout: 10000 });

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
  await expect(page.locator(".playlist-card")).toContainText("Nič nehrá");
});

// #194: the dashboard "Práve hrá" jump-strip (`now-playing-strip` + `strip-goto`
// "Prejsť") is REMOVED — the playing playlist is auto-selected on load and the
// shared Player shows what plays, so there is no separate jump-to-playing strip.
// The auto-select + playing-song surfacing is covered by "clicking another row"
// (player-title) and "the playing playlist is preselected and marked ▶" above.

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
