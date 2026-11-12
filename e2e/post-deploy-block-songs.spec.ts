/**
 * Post-deploy gate: the song block page `/blok` (#265), on SNV and in PP's
 * subset (post-deploy-pp.config.ts), READ-ONLY.
 *
 * - `GET /api/v1/blocked` answers 200 with a list of `{youtube_id, title,
 *   blocked_at_ms}`.
 * - `/blok` loads in a phone-sized browser with its search, its blocked
 *   list LOADED (`data-loaded`, no error line) showing exactly the server's
 *   blocked songs, and its "Teraz na programe" area past "Načítavam…":
 *   a song on a youtube playlist on program has an enabled "Zablokovať v
 *   …" (its YouTube id found among the node's songs) or "Už je
 *   zablokovaná" (`block-gate.ts::currentAreaProblem`). Never clicked.
 *
 * It NEVER blocks or unblocks a song on the box: a block would stop that
 * song being picked on the node. The block / unblock flow is the mock
 * suite's (`block-songs.spec.ts`).
 */

import { test, expect } from "@playwright/test";
import { currentAreaProblem } from "./block-gate";

type Blocked = { youtube_id: string; title: string; blocked_at_ms: number };

test.use({ viewport: { width: 390, height: 844 } });

test.describe("the song block page post-deploy verification (#265)", () => {
  let consoleErrors: string[] = [];

  test.beforeEach(({ page }) => {
    consoleErrors = [];
    page.on("console", (msg) => {
      const type = msg.type();
      if (type === "error" || type === "warning") {
        const text = msg.text();
        // Chromium emits a benign SRI warning on the preloaded WASM bundle.
        if (/integrity.*attribute.*ignored/i.test(text)) return;
        consoleErrors.push(`[${type}] ${text}`);
      }
    });
  });

  test("the block list answers and /blok shows it, nothing blocked by the gate", async ({
    page,
    request,
  }) => {
    const resp = await request.get("/api/v1/blocked", { timeout: 15_000 });
    expect(resp.status(), "GET /api/v1/blocked").toBe(200);
    const blocked = (await resp.json()) as Blocked[];
    expect(Array.isArray(blocked), "the block list is a list").toBe(true);
    for (const b of blocked) {
      expect(typeof b.youtube_id, "youtube_id").toBe("string");
      expect(typeof b.title, "title").toBe("string");
      expect(typeof b.blocked_at_ms, "blocked_at_ms").toBe("number");
    }
    console.log(`[#265 block] ${blocked.length} blocked song(s) on this node`);

    await page.goto("/blok");
    await expect(page.getByTestId("block-page")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("block-current")).toBeVisible();
    await expect(page.getByTestId("block-search")).toBeVisible();
    const list = page.getByTestId("block-list");
    await expect(list).toHaveAttribute("data-loaded", "true", { timeout: 15_000 });
    await expect(page.getByTestId("block-message"), "no error line").toHaveText("");
    if (blocked.length === 0) {
      await expect(list).toContainText("Žiadna pieseň nie je zablokovaná.", { timeout: 15_000 });
    } else {
      await expect(page.getByTestId("block-unblock")).toHaveCount(blocked.length, {
        timeout: 15_000,
      });
    }
    const current = page.getByTestId("block-current");
    await expect(current).not.toHaveAttribute("data-state", "loading", { timeout: 15_000 });
    // The on-program playlist's kind, read AFTER the page settled.
    const status = (await (await request.get("/api/v1/status")).json()) as {
      active_playlist_ids?: number[];
    };
    const playlists = (await (await request.get("/api/v1/playlists")).json()) as {
      id: number;
      kind?: string;
    }[];
    const onAir = status.active_playlist_ids ?? [];
    const programKind =
      onAir.length === 1 ? (playlists.find((p) => p.id === onAir[0])?.kind ?? "youtube") : null;
    const button = page.getByTestId("block-current-btn");
    // The node's song lists load after the program state: poll until the
    // area holds (or 15 s), then log the read it ended on.
    let area = { state: "", buttonText: null as string | null, buttonDisabled: false };
    await expect
      .poll(
        async () => {
          try {
            const hasButton = (await button.count()) === 1;
            area = {
              state: (await current.getAttribute("data-state")) ?? "",
              buttonText: hasButton ? (await button.innerText()).trim() : null,
              buttonDisabled: hasButton ? await button.isDisabled() : false,
            };
            return currentAreaProblem(area, programKind);
          } catch (e) {
            return `the area could not be read: ${e}`;
          }
        },
        { timeout: 15_000, message: "the song on program's area" },
      )
      .toBeNull();
    console.log(
      `[#265 block] on program (${programKind ?? "none"}): ` +
        `${(await current.innerText()).replace(/\s+/g, " ")} [${JSON.stringify(area)}]`,
    );

    // Read-only: the list is still what it was.
    const after = (await (await request.get("/api/v1/blocked")).json()) as Blocked[];
    expect(after.map((b) => b.youtube_id).sort()).toEqual(blocked.map((b) => b.youtube_id).sort());
    expect(consoleErrors, "browser console").toEqual([]);
  });
});
