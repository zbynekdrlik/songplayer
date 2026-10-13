/**
 * Post-deploy gate: the LED wall report reads the box (#259).
 *
 * SongPlayer compares the box with Resolume Arena's OWN saved output setup
 * (`crate::wall`, every 5 s) and publishes `GET /api/v1/wall`. The owner's
 * rule (ROZHODNUTÉ 13.10.2026): SongPlayer never enforces a display or
 * Resolume configuration, so the wall's state — the screens off, Resolume
 * closed, a display missing — is LOGGED here and never fails a deploy. This
 * spec (SNV's suite and PP's subset, read-only) fails only when the report
 * cannot read the box (`unknown`, no answer, never run:
 * `wall-gate.ts`), then checks the dashboard shows the state it read, with a
 * clean console.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  FIRST_CHECK_WAIT_MS,
  wallChecked,
  wallFailures,
  wallSummary,
  type WallStatus,
} from "./wall-gate";

/** `/api/v1/wall` now, or null when the read failed (a poll tries again). */
async function readWall(request: APIRequestContext): Promise<WallStatus | null> {
  try {
    const resp = await request.get("/api/v1/wall", { timeout: 10_000 });
    if (resp.status() !== 200) return null;
    return (await resp.json()) as WallStatus;
  } catch {
    return null;
  }
}

/** The states the health bar shows no segment for. */
const NO_SEGMENT = ["simple_output", "no_display_output"];

test.describe("LED wall report post-deploy verification (#259)", () => {
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

  test("the wall report reads the box and the dashboard shows what it read", async ({
    page,
    request,
  }) => {
    test.setTimeout(FIRST_CHECK_WAIT_MS + 60_000);
    // The report the poll saw checked is the one judged.
    let wall: WallStatus | null = null;
    await expect
      .poll(
        async () => {
          wall = await readWall(request);
          return wallChecked(wall);
        },
        { message: "SongPlayer's first wall check", timeout: FIRST_CHECK_WAIT_MS },
      )
      .toBe(true);
    // The wall's state is information, never a failure (the operator's
    // setup is the truth): logged for the reader of the run.
    console.log(`[#259 wall] ${wallSummary(wall)}`);
    console.log(`[#259 wall] ${JSON.stringify(wall)}`);
    expect(wallFailures(wall), "the wall report").toEqual([]);

    // The dashboard: its poll landed (the marker), and a state with a
    // segment shows it with the same state.
    await page.goto("/");
    const bar = page.getByTestId("health-bar");
    await expect(bar).toBeVisible({ timeout: 15_000 });
    await expect(bar).not.toHaveAttribute("data-wall", "unchecked", { timeout: 30_000 });
    await expect(page.getByTestId("health-version")).toBeVisible();
    const marker = (await bar.getAttribute("data-wall")) ?? "";
    console.log(`[#259 wall] dashboard data-wall=${marker}`);
    const segment = page.getByTestId("health-wall");
    if (NO_SEGMENT.includes(marker)) {
      await expect(segment).toHaveCount(0);
    } else {
      await expect(segment).toBeVisible({ timeout: 15_000 });
      // The segment follows the same poll as the marker.
      await expect(segment).toHaveAttribute("data-state", marker, { timeout: 15_000 });
      await expect(segment).toContainText("Stena: ");
    }
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
