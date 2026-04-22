import { test, expect } from '@playwright/test';

test.use({ viewport: { width: 375, height: 667 } });

test.describe('/live mobile (iPhone-SE viewport)', () => {
  let consoleErrors: string[] = [];

  test.beforeEach(async ({ page }) => {
    consoleErrors = [];
    page.on('console', msg => {
      const t = msg.type();
      if (t === 'error' || t === 'warning') {
        const text = msg.text();
        // Chromium emits a benign SRI warning on preloaded WASM bundles.
        if (/integrity.*attribute.*ignored/i.test(text)) return;
        consoleErrors.push(`[${t}] ${text}`);
      }
    });
  });

  test.afterEach(() => {
    expect(consoleErrors).toEqual([]);
  });

  test('page renders with scrubber visible and 44 px+ touch targets', async ({ page }) => {
    await page.goto('/live');
    const scrubber = page.locator('.np-scrubber');
    await expect(scrubber).toBeVisible({ timeout: 15_000 });
    const bb = await scrubber.boundingBox();
    expect(bb, 'scrubber must have a bounding box').not.toBeNull();
    expect(bb!.height).toBeGreaterThanOrEqual(44);

    // Lyrics lines, when rendered, must also be tall enough to tap.
    const lines = page.locator('.lyr-line');
    if (await lines.count() > 0) {
      const first = lines.first();
      const lbb = await first.boundingBox();
      expect(lbb!.height).toBeGreaterThanOrEqual(44);
    }
  });

  test('tap a lyrics line fires a seek request', async ({ page }) => {
    const seekCalls: { playlist_id: string; body: string }[] = [];
    await page.route('**/api/v1/playlists/*/seek', async route => {
      const req = route.request();
      seekCalls.push({
        playlist_id: req.url().match(/playlists\/(\d+)\/seek/)![1],
        body: req.postData() ?? '',
      });
      await route.fulfill({ status: 204 });
    });

    await page.goto('/live');

    const line = page.locator('.lyr-line').first();
    if ((await line.count()) === 0) {
      test.skip(
        true,
        'no lyrics loaded in this environment — seek UI is wired but untestable without a cached song'
      );
    }

    await line.click();
    await expect.poll(() => seekCalls.length, { timeout: 5_000 }).toBeGreaterThan(0);
    expect(seekCalls[0].body).toMatch(/"position_ms":\s*\d+/);
  });
});
