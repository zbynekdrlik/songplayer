/**
 * Post-deploy gate: the dashboard's usage log (#256), on SNV and in PP's
 * subset (post-deploy-pp.config.ts), read-only.
 *
 * Each node records how its own dashboard is used, the data the #248
 * redesign starts from. On the box:
 *
 * - `GET /api/v1/ui-usage/summary?days=1` answers the server's shape (per
 *   page, control and kind: count, days used, first and last use, uses
 *   while a playlist was on air; `ui-usage-gate.ts::summaryProblems`), and
 *   a `days` out of range is refused with its fixed text.
 * - This spec's own browser session, an automated one
 *   (`navigator.webdriver`, which Playwright sets), adds NOTHING: it
 *   switches Prehľad → Texty → Prehľad `ROUNDS` times, then hides the page:
 *   a log installed in it would hand everything it recorded to
 *   `sendBeacon` at once. Had the log recorded it, each of its three rows
 *   (the two tab clicks and the Texty page view) would have grown by
 *   `ROUNDS`; an operator's own use of the box meanwhile adds fewer
 *   (`ui-usage-gate.ts::recordedSession`).
 * - Nastavenia's line reads the summary ("Záznam používania: …", never
 *   "načítavam" or "nedostupný").
 *
 * A real person's session being counted is never driven here: it would
 * write made-up uses into the node's own statistics. The mock suite proves
 * it (`ui-usage.spec.ts`); on the box it is checked by hand.
 */

import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  recordedSession,
  summaryProblems,
  type RowKey,
  type UsageSummary,
} from "./ui-usage-gate";

/** Tab round trips the session makes. */
const ROUNDS = 4;

/** How long a beacon sent at the hide may take to be stored. */
const BEACON_SETTLE_MS = 3_000;

/** The rows this session would make if it were recorded. */
const SESSION_ROWS: RowKey[] = [
  ["/", "nav-lyrics", "click"],
  ["/lyrics", "nav-dashboard", "click"],
  ["/lyrics", "", "page"],
];

async function summary(request: APIRequestContext): Promise<UsageSummary> {
  const resp = await request.get("/api/v1/ui-usage/summary?days=1", { timeout: 15_000 });
  expect(resp.status(), "GET /api/v1/ui-usage/summary?days=1").toBe(200);
  const answer = (await resp.json()) as UsageSummary;
  expect(summaryProblems(answer, 1), "the summary's shape").toEqual([]);
  return answer;
}

/** The page is being hidden (a tab switched away or closed). */
async function hide(page: Page) {
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "hidden",
    });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

test.describe("the dashboard's usage log post-deploy verification (#256)", () => {
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

  test("the summary answers and this automated session is not recorded", async ({
    page,
    request,
  }) => {
    const before = await summary(request);
    const refused = await request.get("/api/v1/ui-usage/summary?days=0");
    expect(refused.status()).toBe(400);
    expect(await refused.text()).toBe("days must be a whole number from 1 to 400");

    await page.goto("/");
    await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("health-version")).toBeVisible();
    for (let i = 0; i < ROUNDS; i++) {
      await page.getByTestId("nav-lyrics").click();
      await expect(page.getByTestId("nav-lyrics")).toHaveClass(/active/);
      await page.getByTestId("nav-dashboard").click();
      await expect(page.getByTestId("nav-dashboard")).toHaveClass(/active/);
    }
    // A log installed in this session would send everything it holds now.
    await hide(page);
    await page.waitForTimeout(BEACON_SETTLE_MS);

    const after = await summary(request);
    console.log(`[#256 usage] ${after.events} events in the last day, ${after.rows.length} rows`);
    expect(
      recordedSession(before, after, SESSION_ROWS, ROUNDS),
      "rows that grew by this automated session's own uses",
    ).toEqual([]);

    // Nastavenia's line reads the node's own summary.
    await page.goto("/settings");
    const line = page.getByTestId("ui-usage-line");
    await expect(line).toHaveText(/^Záznam používania: /, { timeout: 15_000 });
    await expect(line).not.toHaveText(/načítavam|nedostupný/, { timeout: 15_000 });
    console.log(`[#256 usage] ${await line.textContent()}`);
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
