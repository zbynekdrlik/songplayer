/**
 * Post-deploy: the dashboard's audio fault history on the box (#257 lane 2;
 * SNV's suite and PP's subset, read-only).
 *
 * The owner wants to SEE in SongPlayer how many audio problems there were,
 * how long the chain has been clean and when each happened. After the
 * deploy's restart the HealthBar's one audio segment must read the box's
 * own ledger: a loss in the last 24 h → amber / red, naming its count and
 * the worst path; none → green "Zvuk: bez chyby …". One click must list a
 * row for every path the ledger answers, the program's among them. Nothing
 * is written: the spec reads `GET /api/v1/audio/faults` and the dashboard.
 */

import { test, expect } from "@playwright/test";

/** The levels of an answer with a loss in its window, and of one without. */
const LOSSY = /^(loss|recent)$/;
const CLEAN = /^(ok|risk)$/;

interface PathFaults {
  path: string;
  last_loss_ms: number | null;
}

test.describe("audio fault history on the dashboard (#257)", () => {
  let consoleErrors: string[] = [];

  test.beforeEach(({ page }) => {
    consoleErrors = [];
    page.on("console", (msg) => {
      const type = msg.type();
      if (type === "error" || type === "warning") {
        const text = msg.text();
        // Chromium emits a benign SRI warning on the preloaded WASM bundle.
        if (/integrity.*attribute.*ignored/i.test(text)) return;
        // #178: a page torn down mid-handshake of the preview WebSocket.
        if (/WebSocket is closed before the connection is established/i.test(text)) return;
        consoleErrors.push(`[${type}] ${text}`);
      }
    });
  });

  test("the audio segment agrees with the box's ledger and its panel lists every path", async ({
    page,
    request,
  }) => {
    const resp = await request.get("/api/v1/audio/faults?hours=24", { timeout: 15_000 });
    expect(resp.status(), "GET /api/v1/audio/faults?hours=24").toBe(200);
    const answer = (await resp.json()) as { worst: string | null; paths: PathFaults[] };
    const lossy = answer.paths.filter((p) => p.last_loss_ms !== null);
    console.log(
      `[#257 dashboard] ${answer.paths.length} paths, ${lossy.length} with a loss in 24 h, worst ${answer.worst}`,
    );

    await page.goto("/");
    const segment = page.getByTestId("health-audio");
    await expect(segment).toHaveAttribute("data-level", lossy.length > 0 ? LOSSY : CLEAN, {
      timeout: 30_000,
    });
    const text = ((await segment.textContent()) ?? "").trim();
    console.log(`[#257 dashboard] segment: ${text}`);
    if (lossy.length > 0) {
      expect(text).toMatch(/^Zvuk: \d+ (chyba|chyby|chýb) za 24 h · posledná pred .+ · .+$/);
    } else {
      expect(text).toMatch(/^Zvuk: bez chyby .+$/);
    }

    await segment.click();
    const panel = page.getByTestId("audio-faults-panel");
    await expect(panel).toBeVisible();
    await expect(panel.getByTestId("audio-path-row")).toHaveCount(answer.paths.length, {
      timeout: 30_000,
    });
    await expect(panel.locator('[data-testid="audio-path-row"][data-path="program"]')).toHaveCount(1);
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
