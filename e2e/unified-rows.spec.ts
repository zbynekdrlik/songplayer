import { test, expect, Page, APIRequestContext } from "@playwright/test";

// #194 ROUND 2 — cross-page proof that the ONE shared row / chips / import /
// player render identically on every page (Dashboard, Live, Lyrics, Dabing).
//
// A song looks and behaves the same everywhere because ONE component draws it:
//   - `SongRow`      → `[data-testid="song-row"]` + `song-row-title`
//   - `StatusChips`  → `[data-testid="status-chips"]` with per-kind
//                       `chip-stems`/`chip-text`/`chip-dub`/`chip-file`
//   - `ImportBox`    → `import-box` / `import-input` / `import-btn` (Live + Dabing)
//   - `Player`       → the idle mixer collapses to `player-mixer-idle`, a failed
//                       transport command surfaces `player-error`.
//
// Runs under the default `chromium` project (no H.264 preview needed). The file
// name lands it in `chromium` automatically — the `chrome` project only matches
// `preview.spec.ts` and post-deploy runs under its own config.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

const IMPORT_PLACEHOLDER = "Vlož URL videa (napr. https://youtu.be/…)";
const IMPORT_BUTTON = "Pridať";

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  // Clean, deterministic shared-mock state (serial workers).
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/live-reset");
  await request.post("/__mock/fail-mode", {
    data: { kind: "skip", enabled: false },
  });
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  // Zero console errors/warnings on every page (browser-console-zero-errors.md).
  expect(real).toEqual([]);
});

async function navigateToLyrics(page: Page) {
  // A direct /lyrics deep link renders no sections (the page iterates
  // store.playlists, seeded by the Dashboard's own fetch); always go via nav.
  await page.goto("/");
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });
  await page.getByRole("button", { name: "Lyrics", exact: true }).click();
  await expect(page.getByText("Lyrics Pipeline")).toBeVisible({ timeout: 10000 });
}

async function seedDabingReady(request: APIRequestContext, videoId: number) {
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: videoId,
      title: "Kázeň hotová",
      dub_status: "ready",
      chain_state: "ready",
      stem_status: null,
      dub_file_path: "/c/dub.flac",
    },
  });
}

test.describe("#194: the shared SongRow + StatusChips render on every page", () => {
  test("Dashboard rows carry status chips with the mock's file/stems/text states", async ({
    page,
  }) => {
    await page.goto("/");
    const card = page.locator(".playlist-card", { hasText: "Worship" });
    await expect(card).toBeVisible({ timeout: 10000 });
    // The song list is OPEN by default for the selected (playing) playlist.
    const row = card.locator('.song-row[data-video-id="1"]');
    await expect(row).toBeVisible({ timeout: 10000 });
    await expect(row.locator('[data-testid="status-chips"]')).toBeVisible();

    // video 1: normalized + stems ready + verified reference lyrics.
    await expect(row.locator('[data-testid="chip-file"]')).toHaveText(
      "stiahnuté",
    );
    await expect(row.locator('[data-testid="chip-stems"]')).toHaveText("hotové");
    await expect(row.locator('[data-testid="chip-text"]')).toHaveText(
      "★ overený",
    );
  });

  test("Live set-list rows carry the shared row + text chip", async ({
    page,
    request,
  }) => {
    // Give the ytlive (184) set list one row so a SongRow renders.
    await request.post("/api/v1/playlists/184/items", {
      data: { video_id: 1 },
    });
    await page.goto("/live");
    const row = page.locator(".live-setlist .song-row").first();
    await expect(row).toBeVisible({ timeout: 10000 });
    await expect(row.locator('[data-testid="status-chips"]')).toBeVisible();
    await expect(row.locator('[data-testid="chip-text"]')).toHaveText(
      "★ overený",
    );
  });

  test("Lyrics rows carry the shared row + text chip", async ({ page }) => {
    await navigateToLyrics(page);
    const row = page.locator(".song-row").first();
    await expect(row).toBeVisible({ timeout: 10000 });
    await expect(row.locator('[data-testid="status-chips"]')).toBeVisible();
    await expect(row.locator('[data-testid="chip-text"]')).toHaveText(
      "★ overený",
    );
  });

  test("Dabing rows carry the shared row + dub chip", async ({
    page,
    request,
  }) => {
    await seedDabingReady(request, 700);
    await page.goto("/dabing");
    const row = page
      .locator('[data-testid="dabing-list"] [data-testid="song-row"]')
      .first();
    await expect(row).toBeVisible({ timeout: 10000 });
    await expect(row.locator('[data-testid="status-chips"]')).toBeVisible();
    await expect(row.locator('[data-testid="chip-dub"]')).toHaveText("hotový");
  });
});

test.describe("#194: the shared ImportBox is identical on Live and Dabing", () => {
  test("same testids, placeholder and button text on both pages", async ({
    page,
  }) => {
    // Dabing: the import box is directly visible under the header.
    await page.goto("/dabing");
    const dabingBox = page.locator('[data-testid="import-box"]');
    await expect(dabingBox).toBeVisible({ timeout: 10000 });
    await expect(dabingBox.locator('[data-testid="import-input"]')).toHaveAttribute(
      "placeholder",
      IMPORT_PLACEHOLDER,
    );
    await expect(dabingBox.locator('[data-testid="import-btn"]')).toHaveText(
      IMPORT_BUTTON,
    );

    // Live: same box, tucked in the collapsible "Add songs" panel.
    await page.goto("/live");
    await page.locator(".live-add-toggle").click();
    const liveBox = page.locator('[data-testid="import-box"]');
    await expect(liveBox).toBeVisible({ timeout: 10000 });
    await expect(liveBox.locator('[data-testid="import-input"]')).toHaveAttribute(
      "placeholder",
      IMPORT_PLACEHOLDER,
    );
    await expect(liveBox.locator('[data-testid="import-btn"]')).toHaveText(
      IMPORT_BUTTON,
    );
  });
});

test.describe("#194: the shared Player mixer + error surface", () => {
  test("the mixer collapses to player-mixer-idle when nothing plays (Live)", async ({
    page,
  }) => {
    // The ytlive playlist (184) has no now-playing content in the mock, so the
    // Player's mixer slot shows the one-line idle placeholder.
    await page.goto("/live");
    const idle = page.locator('[data-testid="player-mixer-idle"]');
    await expect(idle).toBeVisible({ timeout: 10000 });
    await expect(idle).toHaveText("Mixér — nič nehrá");
  });

  test("the mixer expands with a playing item (Dashboard)", async ({ page }) => {
    // Playlist 1 (Worship) is marked Playing by the mock WS → the Player's mixer
    // slot mounts the karaoke adapter, so the idle line is gone.
    await page.goto("/");
    const card = page.locator(".playlist-card", { hasText: "Worship" });
    await expect(card).toBeVisible({ timeout: 10000 });
    await expect(card.getByTestId("player-title")).toContainText(
      "Never Gonna Give You Up",
      { timeout: 15000 },
    );
    await expect(card.locator('[data-testid="player-mixer-idle"]')).toHaveCount(0);
    await expect(card.getByTestId("karaoke-now-playing")).toBeVisible();
  });

  test("player-error appears after a failed transport POST and clears on success", async ({
    page,
    request,
  }) => {
    // Force the /skip command to 500 via the mock's fail-mode control hook, then
    // drive the shared Player's ⏭ and assert the Slovak error line surfaces.
    await request.post("/__mock/fail-mode", {
      data: { kind: "skip", enabled: true },
    });
    try {
      await page.goto("/live");
      const skip = page.getByTestId("player-skip");
      await expect(skip).toBeVisible({ timeout: 10000 });
      // No error before the first command.
      await expect(page.locator('[data-testid="player-error"]')).toHaveCount(0);

      await skip.click();
      const err = page.locator('[data-testid="player-error"]');
      await expect(err).toBeVisible({ timeout: 5000 });
      await expect(err).toContainText("zlyhal"); // "Ďalšia zlyhala"

      // A successful command clears the error line.
      await request.post("/__mock/fail-mode", {
        data: { kind: "skip", enabled: false },
      });
      await skip.click();
      await expect(page.locator('[data-testid="player-error"]')).toHaveCount(0, {
        timeout: 5000,
      });
    } finally {
      // Never leak the fail-mode into a serially-run sibling spec.
      await request.post("/__mock/fail-mode", {
        data: { kind: "skip", enabled: false },
      });
    }
  });
});
