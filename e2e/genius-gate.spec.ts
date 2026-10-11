/**
 * Unit tests for the Genius live gate `post-deploy-genius.spec.ts` applies
 * on the box (#232). Runs in the ubuntu mock suite (playwright.config.ts):
 * no browser and no deployed box; these `test()` blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { GENIUS_ROWS, LyricsSongRow, ProbeReport, geniusGateFailures, geniusHit, geniusRows } from "./genius-gate";

function row(video_id: number, source: string | null): LyricsSongRow {
  return { video_id, youtube_id: `yt${video_id}`, song: `Song ${video_id}`, source };
}

/** A probe answer whose Genius line is `available` with `lines`, and `note`
 * (the notes the worker's fetch gives). */
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

  test("one song Genius answers with lyrics passes, though another has no artist match", () => {
    const asked = [row(129, "genius"), row(174, "genius")];
    const reports = [
      report(174, false, 0, "no matching-artist song hit"),
      report(129, true, 67, "hit (67 lines, strict artist match)"),
    ];
    expect(geniusGateFailures(asked, reports)).toEqual([]);
  });

  test("no artist match on every song passes: each search answered, so the token works", () => {
    const asked = [row(1, "genius"), row(2, "genius")];
    const reports = [
      report(1, false, 0, "no matching-artist song hit"),
      report(2, false, 0, "no matching-artist song hit"),
    ];
    expect(geniusGateFailures(asked, reports)).toEqual([]);
  });

  test("a refused search fails, naming each error", () => {
    const asked = [row(129, "genius"), row(136, "genius")];
    const reports = [
      report(129, false, 0, "error: Genius search answered 401 Unauthorized"),
      report(136, false, 0, "error: Genius search answered 401 Unauthorized"),
    ];
    expect(geniusGateFailures(asked, reports)).toEqual([
      'yt129 "Song 129": error: Genius search answered 401 Unauthorized',
      'yt136 "Song 136": error: Genius search answered 401 Unauthorized',
    ]);
  });

  test("an error fails even next to a hit", () => {
    const asked = [row(1, "genius"), row(2, "genius")];
    const reports = [
      report(1, false, 0, "error: Genius song page holds no lyric: https://genius.com/x-lyrics"),
      report(2, true, 12, "hit (12 lines, strict artist match)"),
    ];
    expect(geniusGateFailures(asked, reports)).toEqual([
      'yt1 "Song 1": error: Genius song page holds no lyric: https://genius.com/x-lyrics',
    ]);
  });

  test("a probe with no Genius line fails", () => {
    const missing: ProbeReport = { ...report(3, false, 0, ""), probes: [] };
    expect(geniusGateFailures([row(3, "genius")], [missing])).toEqual([
      'yt3 "Song 3": no genius line in the probe',
    ]);
  });

  test("a hit needs lines: an available line with none is no hit", () => {
    expect(geniusHit(report(1, true, 0, "hit (0 lines)"))).toBe(false);
    expect(geniusHit(report(1, true, 3, "hit (3 lines)"))).toBe(true);
  });

  test("a catalog with no genius-labelled song fails: the gate checks nothing", () => {
    expect(geniusGateFailures([], [])).toEqual([
      "no catalog song's lyrics carry the genius label: nothing to ask",
    ]);
  });
});
