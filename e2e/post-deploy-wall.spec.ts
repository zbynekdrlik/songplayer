/**
 * Post-deploy gate: the LED wall report reads the box (#259).
 *
 * SongPlayer compares the box with Resolume Arena's OWN saved output setup
 * (`crate::wall`, every 5 s) and publishes `GET /api/v1/wall`. The owner's
 * rule (ROZHODNUTÉ 13.10.2026): SongPlayer never enforces a display or
 * Resolume configuration, so the wall's state — the screens off, Resolume
 * closed, a display missing — is LOGGED here and never fails a deploy. This
 * spec (SNV's suite and PP's subset, read-only) polls the report until a
 * check reads the box, and fails only when it cannot for the whole window
 * (`unknown` throughout, no answer, never run: `wall-gate.ts`); then the
 * dashboard's marker and its segment must agree, read in one poll, with a
 * clean console.
 *
 * #275: whether SongPlayer's clip runs in Resolume (`songplayer_clip`) is
 * LOGGED (`[#275 clip] …`) and never fails the deploy: after an Arena
 * relaunch no clip runs until the operator presses a Stream Deck scene, and
 * he may show anything else on purpose. While Resolume runs the clip is
 * polled until it reads or its reader fails (`pollClip`, ≤ 90 s, usually at
 * once); only a server that does not carry the fact, or a reader that
 * cannot read Arena's answer, fails (`clipFailures`).
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import {
  CLIP_WAIT_MS,
  FIRST_CHECK_WAIT_MS,
  clipFailures,
  clipSummary,
  pollClip,
  pollWall,
  wallDashboardMismatch,
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
    test.setTimeout(FIRST_CHECK_WAIT_MS + CLIP_WAIT_MS + 60_000);
    // Polled until a check reads the box; the read the poll ended on is the
    // one judged (`unknown` fails only when it held the whole window).
    const wall = await pollWall(
      () => readWall(request),
      FIRST_CHECK_WAIT_MS,
      (ms) => page.waitForTimeout(ms),
    );
    // The wall's state is information, never a failure (the operator's
    // setup is the truth): logged for the reader of the run.
    console.log(`[#259 wall] ${wallSummary(wall)}`);
    console.log(`[#259 wall] ${JSON.stringify(wall)}`);
    expect(wallFailures(wall), "the wall report").toEqual([]);
    // #275: SongPlayer's clip in Resolume — information, never a failure,
    // unless SongPlayer's own reader cannot read Arena's answer. While
    // Resolume runs, polled until it reads (it reads at the driver's start).
    const clipRead =
      wall?.read?.arena_running === true
        ? await pollClip(
            () => readWall(request),
            CLIP_WAIT_MS,
            (ms) => page.waitForTimeout(ms),
          )
        : wall;
    console.log(`[#275 clip] ${clipSummary(clipRead?.songplayer_clip) || "not carried"}`);
    expect(clipFailures(clipRead), "the #275 clip fact").toEqual([]);

    // The dashboard: its marker and its segment, read in ONE poll so both
    // are the same moment's (the bar re-polls every 5 s).
    await page.goto("/");
    const bar = page.getByTestId("health-bar");
    await expect(bar).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("health-version")).toBeVisible();
    const segment = page.getByTestId("health-wall");
    // The last poll's reads (an object: a closure's writes stay visible).
    const seen: { marker: string; segment: string | null } = { marker: "", segment: null };
    await expect
      .poll(
        async () => {
          // A read that throws (the page mid-render) is one more poll:
          // `expect.poll` does not retry a generator that throws.
          try {
            seen.marker = (await bar.getAttribute("data-wall", { timeout: 1_000 })) ?? "";
            seen.segment =
              (await segment.count()) === 0
                ? null
                : await segment.getAttribute("data-state", { timeout: 1_000 });
          } catch (e) {
            return `read failed: ${e}`;
          }
          return wallDashboardMismatch(seen.marker, seen.segment);
        },
        { message: "the dashboard's wall marker and segment agree", timeout: 30_000 },
      )
      .toBeNull();
    console.log(`[#259 wall] dashboard data-wall=${seen.marker} segment=${seen.segment ?? "none"}`);
    // #275: the bar's clip marker and the segment's note, logged only (a
    // read that fails, the bar mid-render, logs null: never a failure).
    const clipMarker = await bar.getAttribute("data-clip", { timeout: 1_000 }).catch(() => null);
    const clipNote =
      seen.segment === null
        ? "no segment"
        : await segment.getAttribute("data-clip-missing", { timeout: 1_000 }).catch(() => null);
    console.log(`[#275 clip] dashboard data-clip=${clipMarker} note=${clipNote}`);
    if (seen.segment !== null) await expect(segment).toContainText("Stena: ");
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
