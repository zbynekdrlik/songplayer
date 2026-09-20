import { test, expect, Page } from "@playwright/test";

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

async function navigateToLyrics(page: Page) {
  // #194 r3: open /lyrics DIRECTLY (deep link) — the page must load its own
  // playlists via the app-level store, not depend on the Dashboard having
  // mounted first. Slovak pipeline heading = "Spracovanie textov".
  await page.goto("/lyrics");
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });
  await expect(page.getByText("Spracovanie textov")).toBeVisible({
    timeout: 10000,
  });
}

test.describe("Lyrics dashboard — direct deep link (#194 r3)", () => {
  test("opened directly at /lyrics, playlist sections render SongRow + StatusChips", async ({
    page,
  }) => {
    // Regression for the ROUND-2 REVIEW rework #2: the page must NOT depend on
    // the Dashboard having filled store.playlists. A cold /lyrics deep link
    // must show the per-playlist sections with the shared row + chips.
    await page.goto("/lyrics");
    await expect(page.getByText("Spracovanie textov")).toBeVisible({
      timeout: 10000,
    });
    // At least one playlist section with at least one shared song row.
    await expect(page.locator(".lyrics-playlist-section").first()).toBeVisible({
      timeout: 10000,
    });
    const row = page.locator(".lyrics-playlist-section .song-row").first();
    await expect(row).toBeVisible({ timeout: 10000 });
    await expect(row.locator('[data-testid="status-chips"]')).toBeVisible();
    await expect(row.locator('[data-testid="chip-text"]')).toBeVisible();
  });
});

test.describe("Lyrics dashboard — queue visibility", () => {
  test("queue card renders all three bucket counts and pipeline version", async ({ page }) => {
    await navigateToLyrics(page);
    // Each list item contains label + value; match by containing text
    await expect(page.locator(".lyrics-queue-counts li").nth(0)).toContainText("Ručne:");
    await expect(page.locator(".lyrics-queue-counts li").nth(0)).toContainText("2");
    await expect(page.locator(".lyrics-queue-counts li").nth(1)).toContainText("Nové:");
    await expect(page.locator(".lyrics-queue-counts li").nth(1)).toContainText("12");
    await expect(page.locator(".lyrics-queue-counts li").nth(2)).toContainText("Zastarané:");
    await expect(page.locator(".lyrics-queue-counts li").nth(2)).toContainText("187");
    await expect(page.locator(".lyrics-pipeline-version")).toContainText("Verzia spracovania:");
    await expect(page.locator(".lyrics-pipeline-version")).toContainText("2");
  });

  test("#154 idle gate: a song-less processing state renders a 'waiting — wall in use' badge", async ({
    page,
  }) => {
    await navigateToLyrics(page);
    // The mock pushes a LyricsQueueUpdate whose processing entry has empty
    // song/artist and the gate stage — the card must render it as a status
    // badge (just the stage), not "Currently processing:  — ".
    await expect(page.locator(".lyrics-processing")).toContainText("wall in use", {
      timeout: 10000,
    });
    await expect(page.locator(".lyrics-processing")).toContainText("SP-fast Playing");
    // The song-less badge must NOT show the "Currently processing:" prefix.
    await expect(page.locator(".lyrics-processing")).not.toContainText("Currently processing");
  });
});

test.describe("Lyrics dashboard — reprocess triggers", () => {
  test("single-song Reprocess button posts to /reprocess with video_ids", async ({ page }) => {
    await navigateToLyrics(page);
    const postPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/lyrics/reprocess") &&
        !req.url().includes("stale") &&
        req.method() === "POST",
    );
    await page.locator(".song-row button").filter({ hasText: "Preprac." }).first().click();
    const req = await postPromise;
    const body = JSON.parse(req.postData() ?? "{}");
    expect(body).toHaveProperty("video_ids");
    expect(Array.isArray(body.video_ids)).toBe(true);
  });

  test("Reprocess all stale button posts to /reprocess-all-stale", async ({ page }) => {
    await navigateToLyrics(page);
    const postPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/lyrics/reprocess-all-stale") &&
        req.method() === "POST",
    );
    await page.getByRole("button", { name: "Spracovať všetky zastarané" }).click();
    await postPromise;
  });

  // Regression for #98: the WASM dashboard must surface
  // `blocked_by_asr_gap` from the reprocess response (PR #97 shipped the
  // backend field). Operator needs to see when N rows in the request set
  // were parked at `lyrics_source='asr_gap'` and require a pipeline
  // version bump to retry — not a silent `queued: 1`.
  test("reprocess banner surfaces blocked_by_asr_gap from response", async ({
    page,
    request,
  }) => {
    const set = await request.post("/__mock/reprocess-result", {
      data: { queued: 1, blocked_by_asr_gap: 2 },
    });
    expect(set.ok()).toBeTruthy();
    try {
      await navigateToLyrics(page);
      await page
        .locator(".song-row button")
        .filter({ hasText: "Preprac." })
        .first()
        .click();
      await expect(page.locator(".reprocess-asr-gap-banner")).toBeVisible({
        timeout: 5000,
      });
      await expect(page.locator(".reprocess-asr-gap-banner")).toContainText(
        "asr_gap",
      );
      await expect(page.locator(".reprocess-asr-gap-banner")).toContainText("2");
    } finally {
      // Always reset so later tests start clean.
      await request.post("/__mock/reprocess-result", {
        data: { queued: 1, blocked_by_asr_gap: 0 },
      });
    }
  });

  test("reprocess banner stays hidden when blocked_by_asr_gap=0", async ({
    page,
    request,
  }) => {
    // Default mock state already returns blocked_by_asr_gap=0, but be
    // explicit for the assertion's intent.
    await request.post("/__mock/reprocess-result", {
      data: { queued: 1, blocked_by_asr_gap: 0 },
    });
    await navigateToLyrics(page);
    await page
      .locator(".song-row button")
      .filter({ hasText: "Preprac." })
      .first()
      .click();
    // Give the response time to flow through; banner must NOT appear.
    await page.waitForTimeout(500);
    await expect(page.locator(".reprocess-asr-gap-banner")).toHaveCount(0);
  });
});

test.describe("Lyrics dashboard — song detail modal", () => {
  test("Details button opens modal with audit breakdown", async ({ page }) => {
    await navigateToLyrics(page);
    await page.locator(".song-row button").filter({ hasText: "Detail" }).first().click();
    // <details><summary>Raw audit log</summary> — the summary is visible by default
    await expect(page.locator("details summary").filter({ hasText: "Surový audit" })).toBeVisible({ timeout: 5000 });
    await expect(page.locator(".modal p")).toContainText("Zdroj:");
    await expect(page.locator(".modal p")).toContainText("ensemble:qwen3+autosub");
    await expect(page.locator(".modal p")).toContainText("Kvalita:");
    await expect(page.locator(".modal p")).toContainText("0.82");
  });

  test("close button dismisses the modal", async ({ page }) => {
    await navigateToLyrics(page);
    await page.locator(".song-row button").filter({ hasText: "Detail" }).first().click();
    await expect(page.locator("details summary").filter({ hasText: "Surový audit" })).toBeVisible({ timeout: 5000 });
    await page.locator(".modal-close").click();
    await expect(page.locator(".modal-backdrop")).toBeHidden({ timeout: 5000 });
  });
});

test.describe("Lyrics dashboard — status chips", () => {
  test("#194: song with lyrics shows the ★ text chip; song without shows chýba", async ({ page }) => {
    await navigateToLyrics(page);
    // #194: the row is the shared `.song-row` and its lyrics state is the shared
    // `chip-text` (the old status-ok/status-none classes + status-icon are gone).
    await expect(page.locator(".song-row").first()).toBeVisible({ timeout: 10000 });
    // The first row (video 1 — has_lyrics + reference) reads "★ overený".
    await expect(
      page.locator(".song-row").nth(0).locator('[data-testid="chip-text"]'),
    ).toHaveText("★ overený");
    // The second row (video 2 — no lyrics) reads "chýba".
    await expect(
      page.locator(".song-row").nth(1).locator('[data-testid="chip-text"]'),
    ).toHaveText("chýba");
  });
});

// #142/#194: the ★ reference marker is now the `chip-text` "★ overený"; the
// „Nesedí" reject button posts the note.
test.describe("Lyrics dashboard — ★ reference marker", () => {
  test("starred song shows the ★ text chip; Nesedí feedback posts a note", async ({
    page,
  }) => {
    await navigateToLyrics(page);
    await expect(page.locator(".song-row").first()).toBeVisible({ timeout: 10000 });

    // Mock video_id 1 ("Song One") carries lyrics_reference: true → chip-text
    // "★ overený" (the old `.reference-badge` element is gone).
    const starredRow = page.locator(".song-row").nth(0);
    await expect(starredRow.locator('[data-testid="chip-text"]')).toHaveText(
      "★ overený",
    );

    // The un-referenced row (video_id 2) is never starred and never shows the
    // reject button.
    const otherRow = page.locator(".song-row").nth(1);
    await expect(otherRow.locator('[data-testid="chip-text"]')).not.toHaveText(
      "★ overený",
    );
    await expect(otherRow.locator("button", { hasText: "Nesedí" })).toHaveCount(0);

    page.once("dialog", (dialog) => dialog.accept("refrén nesedí s videom"));
    const postPromise = page.waitForRequest(
      (req) => req.url().includes("/reference-feedback") && req.method() === "POST",
    );
    await starredRow.locator("button").filter({ hasText: "Nesedí" }).click();
    const req = await postPromise;
    const body = JSON.parse(req.postData() ?? "{}");
    expect(body.note).toBe("refrén nesedí s videom");
  });
});
