/**
 * Post-deploy gate: a playlist's own volume + EQ round-trips on the box (#242).
 *
 * The deployed server answers `GET /api/v1/playlists/{id}/audio` for a real
 * playlist; its Dashboard card's "Zvuk playlistu" panel shows exactly the
 * stored volume and bands; "Použiť" with nothing edited sends the SAME
 * settings back (`PUT …/audio` → 204, the sound does not change), and the
 * row reads them unchanged with the live generation moved by one — the
 * write path (row first, then the live register the decode thread polls)
 * works on the box.
 *
 * Zero console errors is the last assertion (the same benign filters as
 * `post-deploy-preview.spec.ts`).
 */

import { test, expect } from "@playwright/test";

/** The benign messages every Dashboard post-deploy spec allows
 * (`post-deploy-preview.spec.ts`). */
const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /WebSocket is closed before the connection is established/i,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/i,
];

type Band = {
  kind: string;
  freq_hz: number;
  gain_db: number;
  q: number;
  enabled: boolean;
};
type AudioView = { gain_db: number; eq: Band[]; generation: number };

/** As the panel shows a number (`playlist_audio.rs::shown`, 2 decimals). */
function shown(value: number): string {
  return String(Math.round(value * 100) / 100);
}

test.describe("playlist audio post-deploy verification (#242)", () => {
  let consoleErrors: string[] = [];

  test.beforeEach(({ page }) => {
    consoleErrors = [];
    page.on("console", (msg) => {
      const type = msg.type();
      if (type === "error" || type === "warning") {
        const text = msg.text();
        if (ALLOWED_CONSOLE.some((r) => r.test(text))) return;
        consoleErrors.push(`[${type}] ${text}`);
      }
    });
  });

  test("the panel shows the stored sound and an unchanged save round-trips", async ({
    page,
    request,
  }) => {
    const lists = (await (await request.get("/api/v1/playlists")).json()) as {
      id: number;
      name: string;
    }[];
    expect(lists.length, "the box lists no playlist").toBeGreaterThan(0);
    const pid = lists[0].id;
    const url = `/api/v1/playlists/${pid}/audio`;

    const before = await request.get(url);
    expect(before.status(), `GET ${url}`).toBe(200);
    const stored = (await before.json()) as AudioView;
    console.log(
      `playlist ${pid} (${lists[0].name}): ${stored.gain_db} dB, ${stored.eq.length} band(s), generation ${stored.generation}`,
    );
    const missing = await request.get("/api/v1/playlists/987654321/audio");
    expect(missing.status(), "an unknown playlist").toBe(404);

    // Select the card explicitly (post-deploy-program-state.md).
    await page.goto("/");
    await page.locator(`[data-testid="playlist-picker-item"][data-playlist-id="${pid}"]`).click({
      timeout: 20_000,
    });
    const toggle = page.getByTestId("playlist-workspace").getByTestId("playlist-audio-toggle");
    await expect(toggle).toBeVisible({ timeout: 20_000 });
    await toggle.click();
    await expect(page.getByTestId("playlist-audio-gain")).toHaveValue(
      shown(stored.gain_db),
      { timeout: 10_000 },
    );
    const bands = page.getByTestId("playlist-audio-band");
    await expect(bands).toHaveCount(stored.eq.length);
    for (const [i, b] of stored.eq.entries()) {
      await expect(bands.nth(i).getByTestId("band-kind")).toHaveValue(b.kind);
      await expect(bands.nth(i).getByTestId("band-freq")).toHaveValue(shown(b.freq_hz));
    }
    await expect(page.getByTestId("playlist-audio-path")).toHaveAttribute("d", /^M0\.0 /);

    await page.getByTestId("playlist-audio-save").click();
    await expect(page.getByTestId("playlist-audio-status")).toHaveText("Použité", {
      timeout: 10_000,
    });

    const after = (await (await request.get(url)).json()) as AudioView;
    expect(after.gain_db).toBe(stored.gain_db);
    expect(after.eq).toEqual(stored.eq);
    expect(after.generation, "the live register took the save").toBe(
      stored.generation + 1,
    );

    expect(consoleErrors).toEqual([]);
  });
});
