import { test, expect } from '@playwright/test';

// Mock data shared across tests in this file.
const MOCK_VIDEO_ID = 42;

const MOCK_LIVE_ITEM = {
  position: 0,
  video_id: MOCK_VIDEO_ID,
};

const MOCK_LYRICS_SONG = {
  video_id: MOCK_VIDEO_ID,
  youtube_id: 'aaaaaaaaaaa',
  title: 'Test Song',
  song: 'Test Song',
  artist: 'Test Artist',
  source: null,
  pipeline_version: 0,
  quality_score: null,
  has_lyrics: false,
  is_stale: false,
  manual_priority: false,
  suppress_resolume_en: false,
  spotify_track_id: null,
};

test.describe('/live Spotify URL input (#67)', () => {
  let consoleMessages: string[] = [];

  test.beforeEach(async ({ page }) => {
    consoleMessages = [];
    page.on('console', (msg) => {
      const t = msg.type();
      if (t === 'error' || t === 'warning') {
        const text = msg.text();
        // Chromium emits a benign SRI warning on preloaded WASM bundles.
        if (/integrity.*attribute.*ignored/i.test(text)) return;
        consoleMessages.push(`[${t}] ${text}`);
      }
    });
  });

  test.afterEach(async () => {
    expect(consoleMessages, 'browser console must be clean').toEqual([]);
  });

  test('paste Spotify URL via prompt → PATCH issued with spotify_url field', async ({ page }) => {
    // Mock the live items endpoint so one row renders.
    await page.route('**/api/v1/playlists/184/items', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([MOCK_LIVE_ITEM]),
      });
    });

    // Mock lyrics/songs so the component can join video_id → song/artist/spotify_track_id.
    await page.route('**/api/v1/lyrics/songs**', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([MOCK_LYRICS_SONG]),
      });
    });

    // Mock the PATCH endpoint and capture request bodies.
    const patchBodies: unknown[] = [];
    await page.route(`**/api/v1/videos/${MOCK_VIDEO_ID}`, async (route) => {
      const req = route.request();
      if (req.method() === 'PATCH') {
        const body = req.postData();
        patchBodies.push(body ? JSON.parse(body) : null);
        await route.fulfill({ status: 204 });
        return;
      }
      await route.fallback();
    });

    // Register the dialog handler BEFORE clicking (prompt fires synchronously
    // in the click handler — Playwright buffers it until page.on fires).
    const SPOTIFY_URL = 'https://open.spotify.com/track/3n3Ppam7vgaVa1iaRUc9Lp?si=ab';
    page.once('dialog', async (dialog) => {
      expect(dialog.type()).toBe('prompt');
      await dialog.accept(SPOTIFY_URL);
    });

    // Navigate and activate the Live tab.
    await page.goto('/');
    await page.getByRole('button', { name: 'Live', exact: true }).click();

    // Wait for the setlist row with our mocked song title to render.
    await expect(page.getByText('Test Song').first()).toBeVisible({ timeout: 30_000 });

    // Click the Spotify button on the row.
    const spotifyBtn = page.locator('button.live-setlist-btn-spotify').first();
    await expect(spotifyBtn).toBeVisible();
    await spotifyBtn.click();

    // Wait for the PATCH to be recorded.
    await expect.poll(() => patchBodies.length, { timeout: 5_000 }).toBeGreaterThan(0);

    // Assert the body shape exactly.
    expect(patchBodies).toHaveLength(1);
    expect(patchBodies[0]).toEqual({ spotify_url: SPOTIFY_URL });
  });

  test('cancel prompt → no PATCH issued', async ({ page }) => {
    await page.route('**/api/v1/playlists/184/items', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([MOCK_LIVE_ITEM]),
      });
    });

    await page.route('**/api/v1/lyrics/songs**', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([MOCK_LYRICS_SONG]),
      });
    });

    const patchBodies: unknown[] = [];
    await page.route(`**/api/v1/videos/${MOCK_VIDEO_ID}`, async (route) => {
      const req = route.request();
      if (req.method() === 'PATCH') {
        const body = req.postData();
        patchBodies.push(body ? JSON.parse(body) : null);
        await route.fulfill({ status: 204 });
        return;
      }
      await route.fallback();
    });

    // Dismiss the prompt (operator pressed Cancel).
    page.once('dialog', async (dialog) => {
      expect(dialog.type()).toBe('prompt');
      await dialog.dismiss();
    });

    await page.goto('/');
    await page.getByRole('button', { name: 'Live', exact: true }).click();
    await expect(page.getByText('Test Song').first()).toBeVisible({ timeout: 30_000 });

    await page.locator('button.live-setlist-btn-spotify').first().click();

    // Allow a moment to ensure no PATCH fires after dismiss.
    await page.waitForTimeout(500);
    expect(patchBodies).toHaveLength(0);
  });
});
