/**
 * Post-deploy gate: a playlist's own volume + EQ round-trips on the box (#242,
 * #278).
 *
 * The deployed server answers `GET /api/v1/playlists/{id}/audio` for a real
 * playlist; its Dashboard card's "Zvuk playlistu" panel shows exactly the
 * stored volume and bands (#278: the live panel, every band a card with its
 * numbers, the curve with one handle per band). The panel sends every edit
 * live and has no "save unchanged" action, and any edit would be heard on the
 * box, so the gate touches nothing in it: the unchanged settings go back
 * through the route itself (`PUT …/audio` → 204, the sound does not change),
 * and the row reads them unchanged with the live generation moved by one —
 * the write path (row first, then the live register the decode thread polls)
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

/** As the panel shows a gain (`sp_core::audio_fx_view::gain_label`: 0,1 dB
 * steps rounded half away from zero, as Rust's `f64::round`). */
function gainLabel(db: number): string {
  const tenths = Math.sign(db) * Math.round(Math.abs(db) * 10);
  const snapped = Math.min(12, Math.max(-30, tenths / 10));
  const sign = snapped > 0 ? "+" : snapped < 0 ? "−" : "";
  return `${sign}${Math.abs(snapped).toFixed(1).replace(".", ",")} dB`;
}

test.describe("playlist audio post-deploy verification (#242, #278)", () => {
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
    const workspace = page.getByTestId("playlist-workspace");
    const toggle = workspace.getByTestId("playlist-audio-toggle");
    await expect(toggle).toBeVisible({ timeout: 20_000 });
    await toggle.click();
    await expect(workspace.getByTestId("playlist-audio-gain-value")).toHaveText(
      gainLabel(stored.gain_db),
      { timeout: 10_000 },
    );
    const cards = workspace.getByTestId("playlist-audio-band");
    await expect(cards).toHaveCount(stored.eq.length);
    await expect(workspace.getByTestId("playlist-audio-handle")).toHaveCount(stored.eq.length);
    for (const [i, b] of stored.eq.entries()) {
      // A card names its band's index in the stored list (the cards go by
      // frequency) and carries the band's numbers.
      const card = workspace.locator(
        `[data-testid="playlist-audio-band"][data-index="${i}"]`,
      );
      await expect(card.getByTestId("band-kind")).toHaveValue(b.kind);
      expect(Number(await card.getAttribute("data-freq")), `band ${i} frequency`).toBe(b.freq_hz);
      expect(Number(await card.getAttribute("data-gain")), `band ${i} gain`).toBe(b.gain_db);
      expect(Number(await card.getAttribute("data-q")), `band ${i} Q`).toBe(b.q);
      await expect(card).toHaveAttribute("data-enabled", String(b.enabled));
    }
    await expect(workspace.getByTestId("playlist-audio-path")).toHaveAttribute("d", /^M0\.0 /);
    // The panel itself sent nothing: opening it is no edit.
    const after_open = (await (await request.get(url)).json()) as AudioView;
    expect(after_open.generation, "opening the panel saved something").toBe(stored.generation);

    const put = await request.put(url, { data: { gain_db: stored.gain_db, eq: stored.eq } });
    expect(put.status(), `PUT ${url} (unchanged)`).toBe(204);

    const after = (await (await request.get(url)).json()) as AudioView;
    expect(after.gain_db).toBe(stored.gain_db);
    expect(after.eq).toEqual(stored.eq);
    expect(after.generation, "the live register took the save").toBe(stored.generation + 1);

    expect(consoleErrors).toEqual([]);
  });
});
