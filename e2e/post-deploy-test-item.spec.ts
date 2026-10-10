/**
 * Post-deploy gate (#228): the SP-program burn 911014 and the local test
 * item, on the REAL deployed server.
 *
 * READ-ONLY: it never turns the burn on, never imports, starts or stops the
 * test item, never cuts the program (`post-deploy-program-state.md`). The
 * burn is camera-box's instrument, switched only inside its own run.
 *
 * - `GET /api/v1/program`: `burn_on` is false (a deploy restarts SongPlayer
 *   and the switch lives in memory only, default off), `burned_boundaries`
 *   is a count, and `on_air_item` is null or the item on the wire (its ids,
 *   `started_at_utc_ns`, `position_ms`, `frame`, `frame_utc_ns`).
 * - `GET /api/v1/test-item`: the clip camera-box delivers and its pinned
 *   sha256; once the clip is imported (a manual step on the box, the rule
 *   `.claude/rules/test-item-burn.md`), the item's ids and its scene
 *   `sp-test`, else no item.
 * - `GET /api/v1/playlists` lists no playlist of kind `test` (it is not an
 *   operator playlist).
 *
 * API-only (no page is opened, so there is no browser console to read).
 */

import { test, expect } from "@playwright/test";

test.describe("SP-program burn + the test item (#228)", () => {
  test("the 911014 burn is off after the deploy and the on-air item is reported", async ({
    request,
  }) => {
    const res = await request.get("/api/v1/program", { timeout: 10_000 });
    expect(res.ok(), `GET /api/v1/program → ${res.status()}`).toBe(true);
    const program = await res.json();
    expect(program.burn_on, "the burn starts OFF").toBe(false);
    expect(typeof program.burned_boundaries).toBe("number");
    expect("on_air_item" in program, "the on-air item is reported").toBe(true);
    const item = program.on_air_item;
    if (item !== null) {
      for (const key of [
        "playlist_id",
        "video_id",
        "started_at_utc_ns",
        "position_ms",
        "frame",
        "frame_utc_ns",
      ]) {
        expect(typeof item[key], `on_air_item.${key}`).toBe("number");
      }
      expect(item.frame).toBeGreaterThanOrEqual(0);
      expect(item.position_ms).toBeGreaterThanOrEqual(0);
    }
  });

  test("the test item's route names the clip, and its ids once imported", async ({
    request,
  }) => {
    const res = await request.get("/api/v1/test-item", { timeout: 10_000 });
    expect(res.ok(), `GET /api/v1/test-item → ${res.status()}`).toBe(true);
    const answer = await res.json();
    expect(answer.clip).toBe("measurement-clip-v1-128s.mp4");
    expect(answer.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(answer.sha256.startsWith("a0118ad7")).toBe(true);
    if (answer.imported) {
      expect(answer.item.youtube_id).toBe("measure-v01");
      expect(answer.item.ndi_output_name).toBe("SP-test");
      expect(answer.item.scene).toBe("sp-test");
      expect(answer.item.duration_ms).toBe(128_000);
      expect(typeof answer.item.playlist_id).toBe("number");
      expect(typeof answer.item.video_id).toBe("number");
    } else {
      expect(answer.item).toBeNull();
    }
  });

  test("the playlist list leaves the test playlist out", async ({ request }) => {
    const res = await request.get("/api/v1/playlists", { timeout: 10_000 });
    expect(res.ok(), `GET /api/v1/playlists → ${res.status()}`).toBe(true);
    const playlists: { kind?: string; name: string }[] = await res.json();
    const tests = playlists.filter((p) => p.kind === "test").map((p) => p.name);
    expect(tests, "no test playlist among the operator's playlists").toEqual([]);
  });
});
