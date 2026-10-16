/**
 * Post-deploy gate: a playlist's own volume + EQ on the box, through the
 * owner's own path (#242, #278).
 *
 * The deployed server answers `GET /api/v1/playlists/{id}/audio` for a real
 * playlist; its Dashboard card's "Zvuk playlistu" panel shows exactly the
 * stored volume and bands (#278: the live panel — a card per band with its
 * numbers, the curve with a handle per band) and opening it saves nothing.
 *
 * Then the panel's own live path, browser → send gate → `PUT …/audio` →
 * 204 → the row and the live register: "+ Pásmo (zvon)" adds a peak at
 * 1 kHz and 0 dB, the panel sends it at once (generation +1, the row holds
 * it), and its "Odstrániť" takes it away again (generation +2, the row reads
 * exactly as stored). A 0 dB peak is a unity filter (the RBJ peak at 0 dB has
 * b = a, so its TDF-II stage passes every sample exactly); on a playlist
 * with enabled bands the change is, as for any EQ edit, the DSP's 50 ms
 * crossfade into a fresh cascade of the same filters (their state starting
 * from zero). The gate picks a playlist that is not decoding when there is
 * one, so nothing is heard. The stored settings are put back in `finally` if the
 * row differs from them. Never write any other value on the box from a gate.
 *
 * Zero console errors is the last assertion (the same benign filters as
 * `post-deploy-preview.spec.ts`).
 */

import { test, expect, APIRequestContext } from "@playwright/test";

import { healthRows } from "./box-api";

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

/** The band "+ Pásmo (zvon)" adds (`sp_core::audio_fx::default_band(Peak)`). */
const UNITY_PEAK: Band = { kind: "peak", freq_hz: 1000, gain_db: 0, q: 1, enabled: true };

/** As the panel shows a gain (`sp_core::audio_fx_view::gain_label`: 0,1 dB
 * steps rounded half away from zero, as Rust's `f64::round`). */
function gainLabel(db: number): string {
  const tenths = Math.sign(db) * Math.round(Math.abs(db) * 10);
  const snapped = Math.min(12, Math.max(-30, tenths / 10));
  const sign = snapped > 0 ? "+" : snapped < 0 ? "−" : "";
  return `${sign}${Math.abs(snapped).toFixed(1).replace(".", ",")} dB`;
}

async function readAudio(request: APIRequestContext, url: string): Promise<AudioView> {
  const res = await request.get(url);
  expect(res.status(), `GET ${url}`).toBe(200);
  return (await res.json()) as AudioView;
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

  test("the panel shows the stored sound and its live edit round-trips on the box", async ({
    page,
    request,
  }) => {
    const lists = (await (await request.get("/api/v1/playlists")).json()) as {
      id: number;
      name: string;
    }[];
    expect(lists.length, "the box lists no playlist").toBeGreaterThan(0);
    // A playlist that is not decoding when there is one (the unity peak is
    // silent either way), with room for one more band.
    const decoding = new Set(
      (await healthRows(request))
        .filter((r) => r.transport === "Playing")
        .map((r) => r.playlist_id),
    );
    const stores = new Map<number, AudioView>();
    for (const list of lists) {
      stores.set(list.id, await readAudio(request, `/api/v1/playlists/${list.id}/audio`));
    }
    const roomy = lists.filter((l) => stores.get(l.id)!.eq.length < 8);
    expect(roomy.length, "every playlist already has 8 bands").toBeGreaterThan(0);
    const chosen = roomy.find((l) => !decoding.has(l.id)) ?? roomy[0];
    const pid = chosen.id;
    const url = `/api/v1/playlists/${pid}/audio`;
    const stored = stores.get(pid)!;
    console.log(
      `playlist ${pid} (${chosen.name}, ${decoding.has(pid) ? "decoding" : "not decoding"}): ${stored.gain_db} dB, ${stored.eq.length} band(s), generation ${stored.generation}`,
    );
    const missing = await request.get("/api/v1/playlists/987654321/audio");
    expect(missing.status(), "an unknown playlist").toBe(404);

    try {
      // Select the card explicitly (post-deploy-program-state.md).
      await page.goto("/");
      await page
        .locator(`[data-testid="playlist-picker-item"][data-playlist-id="${pid}"]`)
        .click({ timeout: 20_000 });
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
        const card = workspace.locator(`[data-testid="playlist-audio-band"][data-index="${i}"]`);
        await expect(card.getByTestId("band-kind")).toHaveValue(b.kind);
        expect(Number(await card.getAttribute("data-freq")), `band ${i} frequency`).toBe(b.freq_hz);
        expect(Number(await card.getAttribute("data-gain")), `band ${i} gain`).toBe(b.gain_db);
        expect(Number(await card.getAttribute("data-q")), `band ${i} Q`).toBe(b.q);
        await expect(card).toHaveAttribute("data-enabled", String(b.enabled));
      }
      await expect(workspace.getByTestId("playlist-audio-path")).toHaveAttribute("d", /^M0\.0 /);
      expect((await readAudio(request, url)).generation, "opening the panel saved something").toBe(
        stored.generation,
      );

      // The live path: a unity peak added through the panel is on the box at once.
      await workspace.getByTestId("playlist-audio-add-peak").click();
      await expect
        .poll(async () => (await readAudio(request, url)).eq, { timeout: 10_000 })
        .toEqual([...stored.eq, UNITY_PEAK]);
      expect((await readAudio(request, url)).generation, "one PUT for the add").toBe(
        stored.generation + 1,
      );
      await expect(workspace.getByTestId("playlist-audio-status")).toHaveText("Uložené");

      // ...and removed through the panel, the row reads exactly as stored.
      const added = workspace.locator(
        `[data-testid="playlist-audio-band"][data-index="${stored.eq.length}"]`,
      );
      await added.getByTestId("band-remove").click();
      await expect
        .poll(async () => (await readAudio(request, url)).eq, { timeout: 10_000 })
        .toEqual(stored.eq);
      const after = await readAudio(request, url);
      expect(after.gain_db).toBe(stored.gain_db);
      expect(after.generation, "one PUT for the remove").toBe(stored.generation + 2);
    } finally {
      const now = await readAudio(request, url);
      if (now.gain_db !== stored.gain_db || JSON.stringify(now.eq) !== JSON.stringify(stored.eq)) {
        console.log(`playlist ${pid}: putting the stored sound back`);
        await request.put(url, { data: { gain_db: stored.gain_db, eq: stored.eq } });
      }
    }

    expect(consoleErrors).toEqual([]);
  });
});
