/**
 * Post-deploy gate: the box is a production environment (#233).
 *
 * Windows Defender's real-time scan of the processes SongPlayer launches
 * made PP's Waves SoundGrid driver resync (an audible pop per scan, proven
 * A/B/A). The owner's rule: a production PC runs Windows LTSC or Windows 11
 * IoT Enterprise, with Defender's real-time protection (and any passive /
 * EDR block mode), every other antivirus and every firewall profile OFF,
 * and nothing lets a box run otherwise. SongPlayer checks it
 * itself at the start and every minute (`crate::prod_env`) and publishes
 * `GET /api/v1/status.environment`; this spec (SNV's suite and PP's subset,
 * read-only) waits for the first check after the deploy's restart and FAILS
 * on any problem, each named by its code and Slovak text
 * (`environment-gate.ts`, unit-tested in the mock suite). There is no
 * allowance: an edition that is not allowed, Defender on, a firewall profile
 * on, or a fact the check could not read are all fatal.
 *
 * The dashboard's health bar then says so (`data-environment="ok"`, its
 * last poll) and shows no "Prostredie" segment, and the console is clean
 * (the same benign filters as the other post-deploy specs).
 *
 * #255: the same gate covers the production PC's audio profile (every
 * SoundGrid adapter on NDIS with Waves' settings off, automatic updates off,
 * MMCSS for Pro Audio). Its report-only facts (the Dante adapter's driver,
 * the Windows Update driver exclusion, power) never fail: they are logged
 * and shown as the grey "Profil PC" segment.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  environmentChecked,
  environmentFailures,
  environmentNotes,
  type EnvironmentStatus,
} from "./environment-gate";

/** `/api/v1/status.environment` now, or null when the read failed (a poll
 * tries again: `expect.poll` does not retry a generator that throws). */
async function readEnvironment(request: APIRequestContext): Promise<EnvironmentStatus | null> {
  try {
    const resp = await request.get("/api/v1/status", { timeout: 10_000 });
    if (resp.status() !== 200) return null;
    return ((await resp.json()).environment ?? null) as EnvironmentStatus | null;
  } catch {
    return null;
  }
}

test.describe("production environment post-deploy verification (#233)", () => {
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

  test("the box runs an allowed edition with Defender real-time and the firewall off", async ({
    page,
    request,
  }) => {
    test.setTimeout(FIRST_CHECK_WAIT_MS + 60_000);
    // The status the poll saw checked is the one judged (no second read
    // that could answer something else, review finding 10).
    let env: EnvironmentStatus | null = null;
    await expect
      .poll(
        async () => {
          env = await readEnvironment(request);
          return environmentChecked(env);
        },
        {
          message: "SongPlayer's first production-environment check",
          timeout: FIRST_CHECK_WAIT_MS,
        },
      )
      .toBe(true);
    console.log(`[#233 environment] ${JSON.stringify(env)}`);
    const notes = environmentNotes(env);
    console.log(`[#255 report-only] ${notes.length === 0 ? "none" : notes.join("; ")}`);
    expect(environmentFailures(env), "the production environment").toEqual([]);

    // The dashboard agrees: its poll's marker says "ok" (asserted FIRST: an
    // absent segment proves nothing before the WASM rendered the poll's
    // answer, review finding 5), then no red "Prostredie" segment.
    await page.goto("/");
    const bar = page.getByTestId("health-bar");
    await expect(bar).toBeVisible({ timeout: 15_000 });
    await expect(bar).toHaveAttribute("data-environment", "ok", { timeout: 30_000 });
    await expect(page.getByTestId("health-version")).toBeVisible();
    await expect(page.getByTestId("health-environment")).toHaveCount(0);
    // #255: the report-only notes are the grey segment, never a failure.
    const notesSegment = page.getByTestId("health-environment-notes");
    if (notes.length > 0) {
      await expect(notesSegment).toContainText(`Profil PC: ${notes.length} `, { timeout: 15_000 });
    } else {
      await expect(notesSegment).toHaveCount(0);
    }
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
