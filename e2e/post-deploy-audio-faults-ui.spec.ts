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
 *
 * The ledger is read once before the page and once after its segment
 * renders (#257 review): a fault recorded between the two reads changes
 * what the segment may say, so the segment must agree with EITHER read.
 */

import { test, expect } from "@playwright/test";

/** The levels of an answer with a loss in its window, and of one without. */
const LOSSY = /^(loss|recent)$/;
const CLEAN = /^(ok|risk)$/;

interface PathFaults {
  path: string;
  last_loss_ms: number | null;
}

interface Answer {
  worst: string | null;
  paths: PathFaults[];
}

/** One line of a read for the log and the failure message. */
function summary(a: Answer): string {
  const lossy = a.paths.filter((p) => p.last_loss_ms !== null).length;
  return `${a.paths.length} paths, ${lossy} with a loss in 24 h, worst ${a.worst}`;
}

/** Whether the segment's level and text agree with one read of the ledger. */
function agrees(a: Answer, level: string, text: string): boolean {
  if (a.paths.some((p) => p.last_loss_ms !== null)) {
    return LOSSY.test(level) && /^Zvuk: \d+ (chyba|chyby|chýb) za 24 h · posledná pred .+ · .+$/.test(text);
  }
  return CLEAN.test(level) && /^Zvuk: bez chyby .+$/.test(text);
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
    const read = async (): Promise<Answer> => {
      const resp = await request.get("/api/v1/audio/faults?hours=24", { timeout: 15_000 });
      expect(resp.status(), "GET /api/v1/audio/faults?hours=24").toBe(200);
      return (await resp.json()) as Answer;
    };
    const before = await read();
    console.log(`[#257 dashboard] before: ${summary(before)}`);

    await page.goto("/");
    const segment = page.getByTestId("health-audio");
    // Rendered from an answer (the program is always listed, so a read
    // answer is never "unknown"; "Zvuk: nedostupné" is, and fails here).
    await expect(segment).toHaveAttribute("data-level", /^(ok|risk|loss|recent)$/, { timeout: 30_000 });
    const level = (await segment.getAttribute("data-level")) ?? "";
    const text = ((await segment.textContent()) ?? "").trim();
    const after = await read();
    console.log(`[#257 dashboard] segment: ${level} · ${text}`);
    console.log(`[#257 dashboard] after: ${summary(after)}`);
    expect(
      [before, after].some((a) => agrees(a, level, text)),
      `the segment (${level}, "${text}") agrees with neither read: before ${summary(before)}; after ${summary(after)}`,
    ).toBe(true);

    await segment.click();
    const panel = page.getByTestId("audio-faults-panel");
    await expect(panel).toBeVisible();
    const rows = panel.getByTestId("audio-path-row");
    await expect(rows.first()).toBeVisible({ timeout: 30_000 });
    const counts = [before.paths.length, after.paths.length];
    expect(counts, "a row per path of either read").toContain(await rows.count());
    await expect(panel.locator('[data-testid="audio-path-row"][data-path="program"]')).toHaveCount(1);
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
