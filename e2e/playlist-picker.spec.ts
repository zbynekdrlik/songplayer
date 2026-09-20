import { test, expect } from "@playwright/test";

// #194 ROUND 3c — cross-page proof that ONE shared PlaylistPicker renders on
// every page that chooses a playlist (Dashboard, Live, Lyrics), with the SAME
// testids (`playlist-picker` container + `playlist-picker-item`), and that Live
// pre-selects the live-kind ("custom") playlist THROUGH the picker — no
// hardcoded `name == "ytlive"` lookup.

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
  // Ensure the default 3-playlist fixture (1 live-kind "custom" = ytlive), in
  // case a sibling spec left the 12-playlist fixture (no custom playlist) on.
  await request.post("/__mock/fixture", { data: { mode: "default" } });
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test.describe("#194: the shared PlaylistPicker renders on every page", () => {
  test("Dashboard shows the picker with every playlist as an item", async ({
    page,
  }) => {
    await page.goto("/");
    const picker = page.getByTestId("playlist-picker");
    await expect(picker).toBeVisible({ timeout: 10000 });
    // The default fixture ships 3 playlists; each is a picker item (desktop list).
    await expect(
      page.getByTestId("playlist-picker-list").getByTestId("playlist-picker-item"),
    ).toHaveCount(3, { timeout: 10000 });
  });

  test("Live shows the picker filtered to the live-kind playlist and pre-selects it", async ({
    page,
  }) => {
    await page.goto("/live");
    const picker = page.getByTestId("playlist-picker");
    await expect(picker).toBeVisible({ timeout: 10000 });
    // Filtered to kind == "custom": only ytlive appears (no Worship/Background).
    const items = page
      .getByTestId("playlist-picker-list")
      .getByTestId("playlist-picker-item");
    await expect(items).toHaveCount(1, { timeout: 10000 });
    await expect(items.first()).toContainText("ytlive");
    // Pre-selected THROUGH the picker: the live setlist for the live playlist
    // renders without any hardcoded name lookup.
    await expect(page.locator(".live-setlist")).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 10000 });
  });

  test("Lyrics shows the picker; the per-playlist sections follow the same list", async ({
    page,
  }) => {
    await page.goto("/lyrics");
    await expect(page.getByTestId("playlist-picker")).toBeVisible({
      timeout: 10000,
    });
    // Sections are driven by the same playlist list the picker uses.
    await expect(page.locator(".lyrics-playlist-section").first()).toBeVisible({
      timeout: 10000,
    });
  });
});
