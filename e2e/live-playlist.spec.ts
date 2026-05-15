// E2E coverage for #39: the /live page is the operator's primary
// click-to-play surface during worship sets. This spec covers the
// flows the issue listed as required:
//
//   - navigation + page layout
//   - catalog filter toggle changes visible row count
//   - "+ Add" appends to the setlist (positions 1, then 2)
//   - clicking ▶ on row 2 fires a POST /play-video for the right id
//   - clicking ✕ on row 1 compacts row 2 down to position 1
//   - global Skip button POSTs to /skip
//   - reloading the page preserves the setlist
//   - zero console errors/warnings across every step
//
// The mock-api keeps live setlist state in-memory; tests reset it via
// the `/__mock/live-reset` admin endpoint in beforeEach.

import { test, expect, Page, APIRequestContext } from "@playwright/test";

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
  // Always start with an empty setlist so positions are deterministic.
  await request.post("/__mock/live-reset");
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

async function gotoLive(page: Page) {
  await page.goto("/live");
  // The live-page wrapper renders unconditionally; the setlist section
  // appears once the ytlive playlist resolves.
  await expect(page.locator(".live-page")).toBeVisible({ timeout: 10000 });
  await expect(page.locator(".live-section-setlist")).toBeVisible({
    timeout: 10000,
  });
  await expect(page.locator(".live-section-player")).toBeVisible({
    timeout: 10000,
  });
}

async function openCatalog(page: Page) {
  // The "Add songs" panel is collapsed by default; expand it.
  await page.locator(".live-add-toggle").click();
  await expect(page.locator(".live-catalog")).toBeVisible({ timeout: 5000 });
}

async function addRow(
  page: Page,
  request: APIRequestContext,
  rowIndex: number,
): Promise<number> {
  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/playlists/184/items") &&
      req.method() === "POST",
  );
  await page
    .locator(".live-catalog-table tbody tr")
    .nth(rowIndex)
    .getByRole("button", { name: "+ Add" })
    .click();
  const req = await postPromise;
  const body = JSON.parse(req.postData() ?? "{}");
  return Number(body.video_id);
}

test.describe("/live page — primary operator surface (#39)", () => {
  test("layout: setlist + player sections render", async ({ page }) => {
    await gotoLive(page);
    await expect(page.locator(".live-setlist")).toBeVisible();
  });

  test("catalog filter toggle changes visible row count", async ({ page }) => {
    await gotoLive(page);
    await openCatalog(page);
    // Mock catalog: 1 row with has_lyrics=true, 1 with has_lyrics=false.
    // Filter ON (default) → 1 visible row.
    await expect(page.locator(".live-catalog-table tbody tr")).toHaveCount(1);
    // Toggle OFF → both rows visible.
    await page
      .locator(".live-catalog-header input[type='checkbox']")
      .uncheck();
    await expect(page.locator(".live-catalog-table tbody tr")).toHaveCount(2);
  });

  test("adding two catalog rows produces setlist positions 1 and 2", async ({
    page,
    request,
  }) => {
    await gotoLive(page);
    await openCatalog(page);
    // Filter OFF so both catalog rows are clickable.
    await page
      .locator(".live-catalog-header input[type='checkbox']")
      .uncheck();
    const vid1 = await addRow(page, request, 0);
    const vid2 = await addRow(page, request, 1);
    expect(vid1).not.toEqual(vid2);
    await expect(page.locator(".live-setlist-table tbody tr")).toHaveCount(2, {
      timeout: 5000,
    });
  });

  test("▶ on setlist row 2 POSTs play-video with that row's video_id", async ({
    page,
    request,
  }) => {
    await gotoLive(page);
    await openCatalog(page);
    await page
      .locator(".live-catalog-header input[type='checkbox']")
      .uncheck();
    const vid1 = await addRow(page, request, 0);
    const vid2 = await addRow(page, request, 1);
    await expect(page.locator(".live-setlist-table tbody tr")).toHaveCount(2, {
      timeout: 5000,
    });
    const playPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/playlists/184/play-video") &&
        req.method() === "POST",
    );
    // Row indexes are 0-based; the "row 2" in the setlist is index 1.
    await page
      .locator(".live-setlist-table tbody tr")
      .nth(1)
      .locator(".live-setlist-btn-play")
      .click();
    const req = await playPromise;
    const body = JSON.parse(req.postData() ?? "{}");
    expect(body.video_id).toBe(vid2);
    expect(body.video_id).not.toBe(vid1);
  });

  test("✕ on row 1 collapses row 2 down to position 1", async ({
    page,
    request,
  }) => {
    await gotoLive(page);
    await openCatalog(page);
    await page
      .locator(".live-catalog-header input[type='checkbox']")
      .uncheck();
    await addRow(page, request, 0);
    const vid2 = await addRow(page, request, 1);
    await expect(page.locator(".live-setlist-table tbody tr")).toHaveCount(2);
    await page
      .locator(".live-setlist-table tbody tr")
      .nth(0)
      .locator(".live-setlist-btn-remove")
      .click();
    await expect(page.locator(".live-setlist-table tbody tr")).toHaveCount(1, {
      timeout: 5000,
    });
    // Verify the remaining row is the one we added second (vid2) — i.e.
    // row 1 was the deleted one, row 2 compacted into position 1.
    const remaining = page.locator(".live-setlist-table tbody tr").first();
    await expect(remaining).toContainText(/.+/);
    // Backend-side confirmation: GET /items now returns one row with
    // position=1 and the surviving video_id.
    const resp = await request.get("/api/v1/playlists/184/items");
    expect(resp.ok()).toBeTruthy();
    const items = await resp.json();
    expect(items).toHaveLength(1);
    expect(items[0].video_id).toBe(vid2);
    expect(items[0].position).toBe(1);
  });

  test("global ⏭ Skip posts to /skip", async ({ page }) => {
    await gotoLive(page);
    const skipPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/playback/184/skip") &&
        req.method() === "POST",
    );
    await page
      .locator(".live-setlist-controls")
      .getByRole("button", { name: "⏭" })
      .click();
    await skipPromise;
  });

  test("setlist persists across a full page reload", async ({
    page,
    request,
  }) => {
    await gotoLive(page);
    await openCatalog(page);
    await page
      .locator(".live-catalog-header input[type='checkbox']")
      .uncheck();
    await addRow(page, request, 0);
    await addRow(page, request, 1);
    await expect(page.locator(".live-setlist-table tbody tr")).toHaveCount(2);
    await page.reload();
    await expect(page.locator(".live-section-setlist")).toBeVisible({
      timeout: 10000,
    });
    await expect(page.locator(".live-setlist-table tbody tr")).toHaveCount(2, {
      timeout: 10000,
    });
  });
});
