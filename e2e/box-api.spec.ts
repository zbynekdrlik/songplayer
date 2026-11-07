/**
 * Unit tests for the post-deploy specs' dub pick (`box-api.ts`). Runs in the
 * ubuntu mock suite (playwright.config.ts): no browser and no box; these
 * `test()` blocks never touch the `page` fixture.
 */

import { test, expect } from "@playwright/test";
import { pickReadyDub, type DabingList } from "./box-api";

const DABING = 648;
const list = (...videos: DabingList["videos"]): DabingList => ({ playlist_id: DABING, videos });

test.describe("pickReadyDub", () => {
  test("a ready dub of another playlist is never the Dabing sample (#184)", () => {
    // The live list on 6.11.2026: a song of playlist 516 with a dub came first.
    // ▶ on its row plays it on 516, so the Dabing pipeline never decodes.
    const body = list(
      { video_id: 351, playlist_id: 516, dub_status: "ready" },
      { video_id: 344, playlist_id: DABING, dub_status: "ready" },
    );
    expect(pickReadyDub(body)).toBe(344);
    expect(pickReadyDub(body, 351)).toBe(344);
  });

  test("the preferred dub wins among the Dabing playlist's ready ones", () => {
    const body = list(
      { video_id: 1, playlist_id: DABING, dub_status: "ready" },
      { video_id: 2, playlist_id: DABING, dub_status: "ready" },
      { video_id: 3, playlist_id: DABING, dub_status: "synth" },
    );
    expect(pickReadyDub(body, 2)).toBe(2);
    expect(pickReadyDub(body, 3)).toBe(1);
    expect(pickReadyDub(body)).toBe(1);
  });

  test("no ready dub on the Dabing playlist picks nothing", () => {
    expect(pickReadyDub(list({ video_id: 351, playlist_id: 516, dub_status: "ready" }))).toBeUndefined();
    expect(pickReadyDub(list())).toBeUndefined();
  });
});
