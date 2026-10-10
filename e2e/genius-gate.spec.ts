/**
 * Unit tests for the Genius live gate `post-deploy-genius.spec.ts` applies
 * on the box (#232). Runs in the ubuntu mock suite (playwright.config.ts):
 * no browser and no deployed box; these `test()` blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { GENIUS_ROWS, LyricsSongRow, ProbeReport, geniusGateFailures, geniusRows } from "./genius-gate";

function row(video_id: number, source: string | null): LyricsSongRow {
  return { video_id, youtube_id: `yt${video_id}`, song: `Song ${video_id}`, source };
}

/** A probe answer whose Genius line is `available` with `lines`, and `note`
 * (the live SNV answers of 10.10.2026). */
function report(video_id: number, available: boolean, lines: number, note: string): ProbeReport {
  return {
    video_id,
    youtube_id: `yt${video_id}`,
    song: `Song ${video_id}`,
    artist: "Artist",
    probes: [
      { provider: "lyrics_ovh", provider_url: "", available: false, line_count: 0, note: "no lyrics found" },
      { provider: "genius", provider_url: "", available, line_count: lines, note },
    ],
  };
}

test.describe("Genius live gate (#232)", () => {
  test("the songs asked are the Genius ones, lowest row first, at most six", () => {
    const songs = [
      row(174, "genius+mtl@rev1/g35t-ok"),
      row(12, "lrclib+mtl@rev1/g35t-ok"),
      row(136, "genius+mtl@rev1/g35t-ok"),
      row(129, "genius+mtl@rev1/g35t-ok"),
      row(300, "genius+mtl@rev1/g35t-ok"),
      row(301, "genius+mtl@rev1/g35t-ok"),
      row(302, "genius+mtl@rev1/g35t-ok"),
      row(303, "genius+mtl@rev1/g35t-ok"),
      row(5, null),
    ];
    expect(geniusRows(songs).map((s) => s.video_id)).toEqual([129, 136, 174, 300, 301, 302]);
    expect(GENIUS_ROWS).toBe(6);
  });

  test("one song Genius answers with lyrics passes, though another left Genius", () => {
    const asked = [row(129, "genius"), row(174, "genius")];
    const reports = [
      report(174, false, 0, "no matching-artist song hit"),
      report(129, true, 67, "hit (67 lines, strict artist match)"),
    ];
    expect(geniusGateFailures(asked, reports)).toEqual([]);
  });

  test("no song answered from Genius fails, naming each probe's note", () => {
    const asked = [row(129, "genius"), row(136, "genius")];
    const reports = [
      report(129, false, 0, "error: HTTP 401 Unauthorized"),
      report(136, false, 0, "error: HTTP 401 Unauthorized"),
    ];
    expect(geniusGateFailures(asked, reports)).toEqual([
      'yt129 "Song 129": error: HTTP 401 Unauthorized',
      'yt136 "Song 136": error: HTTP 401 Unauthorized',
    ]);
  });

  test("an available line with no lines is no hit", () => {
    expect(geniusGateFailures([row(1, "genius")], [report(1, true, 0, "hit (0 lines)")])).toHaveLength(1);
  });

  test("a catalog with no Genius song fails: the gate checks nothing", () => {
    expect(geniusGateFailures([], [])).toEqual([
      "no catalog song's served lyrics came from Genius: nothing to ask",
    ]);
  });
});
