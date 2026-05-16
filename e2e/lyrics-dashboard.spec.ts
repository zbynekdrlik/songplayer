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
  await page.goto("/");
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });
  await page.getByRole("button", { name: "Lyrics" }).click();
  await expect(page.getByText("Lyrics Pipeline")).toBeVisible({ timeout: 10000 });
}

test.describe("Lyrics dashboard — queue visibility", () => {
  test("queue card renders all three bucket counts and pipeline version", async ({ page }) => {
    await navigateToLyrics(page);
    // Each list item contains label + value; match by containing text
    await expect(page.locator(".lyrics-queue-counts li").nth(0)).toContainText("Manual:");
    await expect(page.locator(".lyrics-queue-counts li").nth(0)).toContainText("2");
    await expect(page.locator(".lyrics-queue-counts li").nth(1)).toContainText("New:");
    await expect(page.locator(".lyrics-queue-counts li").nth(1)).toContainText("12");
    await expect(page.locator(".lyrics-queue-counts li").nth(2)).toContainText("Stale:");
    await expect(page.locator(".lyrics-queue-counts li").nth(2)).toContainText("187");
    await expect(page.locator(".lyrics-pipeline-version")).toContainText("Pipeline version:");
    await expect(page.locator(".lyrics-pipeline-version")).toContainText("2");
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
    await page.locator(".lyrics-song-row button").filter({ hasText: "Reprocess" }).first().click();
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
    await page.getByRole("button", { name: "Reprocess all stale" }).click();
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
        .locator(".lyrics-song-row button")
        .filter({ hasText: "Reprocess" })
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
      .locator(".lyrics-song-row button")
      .filter({ hasText: "Reprocess" })
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
    await page.locator(".lyrics-song-row button").filter({ hasText: "Details" }).first().click();
    // <details><summary>Raw audit log</summary> — the summary is visible by default
    await expect(page.locator("details summary").filter({ hasText: "Raw audit log" })).toBeVisible({ timeout: 5000 });
    await expect(page.locator(".modal p")).toContainText("Source:");
    await expect(page.locator(".modal p")).toContainText("ensemble:qwen3+autosub");
    await expect(page.locator(".modal p")).toContainText("Quality:");
    await expect(page.locator(".modal p")).toContainText("0.82");
  });

  test("close button dismisses the modal", async ({ page }) => {
    await navigateToLyrics(page);
    await page.locator(".lyrics-song-row button").filter({ hasText: "Details" }).first().click();
    await expect(page.locator("details summary").filter({ hasText: "Raw audit log" })).toBeVisible({ timeout: 5000 });
    await page.locator(".modal-close").click();
    await expect(page.locator(".modal-backdrop")).toBeHidden({ timeout: 5000 });
  });
});

test.describe("Lyrics dashboard — status badges", () => {
  test("song with lyrics shows status-ok; song without shows status-none", async ({ page }) => {
    await navigateToLyrics(page);
    // Wait for at least one row to render (async fetch from mock)
    await expect(page.locator(".lyrics-song-row").first()).toBeVisible({ timeout: 10000 });
    // The first row (has_lyrics: true) gets status-ok
    await expect(page.locator(".lyrics-song-row").nth(0)).toHaveClass(/status-ok/);
    // The second row (has_lyrics: false) gets status-none
    await expect(page.locator(".lyrics-song-row").nth(1)).toHaveClass(/status-none/);
  });
});
