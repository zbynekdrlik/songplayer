/**
 * Post-deploy gate: the box is a production environment (#233).
 *
 * Windows Defender's real-time scan of the processes SongPlayer launches
 * made PP's Waves SoundGrid driver resync (an audible pop per scan, proven
 * A/B/A). The owner's rule: a production PC runs Windows LTSC or Windows IoT
 * Enterprise, with Defender's real-time protection and every firewall
 * profile OFF, and nothing lets a box run otherwise. SongPlayer checks it
 * itself at the start and every minute (`crate::prod_env`) and publishes
 * `GET /api/v1/status.environment`; this spec (SNV's suite and PP's subset,
 * read-only) waits for the first check after the deploy's restart and FAILS
 * on any problem, each named by its code and Slovak text
 * (`environment-gate.ts`, unit-tested in the mock suite). There is no
 * allowance: an edition that is not allowed, Defender on, a firewall profile
 * on, or a fact the check could not read are all fatal.
 *
 * The dashboard then shows no "Prostredie" segment on the box, and the
 * console is clean (the same benign filters as the other post-deploy specs).
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  environmentChecked,
  environmentFailures,
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
    await expect
      .poll(async () => environmentChecked(await readEnvironment(request)), {
        message: "SongPlayer's first production-environment check",
        timeout: FIRST_CHECK_WAIT_MS,
      })
      .toBe(true);
    const env = await readEnvironment(request);
    expect(env, "GET /api/v1/status.environment").not.toBeNull();
    console.log(`[#233 environment] ${JSON.stringify(env)}`);
    expect(environmentFailures(env), "the production environment").toEqual([]);

    // The dashboard agrees: no red "Prostredie" segment once its poll landed.
    const polled = page.waitForResponse((r) => r.url().endsWith("/api/v1/environment"), {
      timeout: 30_000,
    });
    await page.goto("/");
    expect((await polled).status(), "GET /api/v1/environment").toBe(200);
    await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("health-version")).toBeVisible();
    await expect(page.getByTestId("health-environment")).toHaveCount(0);
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
