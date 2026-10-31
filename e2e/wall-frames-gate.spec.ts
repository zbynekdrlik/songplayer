/**
 * Unit tests for the post-deploy wall frame gate's decision (#272 lane 2,
 * `wall-frames-gate.ts`). Runs in the ubuntu mock suite
 * (playwright.config.ts): no browser and no box; these `test()` blocks never
 * touch the `page` fixture.
 */

import { test, expect } from "@playwright/test";
import {
  EXIT_CAPTURE_FAILED,
  EXIT_CLEAN,
  EXIT_DEFECTS,
  EXIT_INCOMPLETE,
  EXIT_NOTHING_DECODED,
  EXIT_USAGE,
  ArenaLayer,
  WallFramesReport,
  coveringLayers,
  wallFramesFailures,
} from "./wall-frames-gate";

/** A clean 40 s take of the 30 fps clip on a 60 Hz wall. */
function clean(): WallFramesReport {
  return {
    verdict: "clean",
    fps: 30,
    samples: 2400,
    decoded: 2398,
    boundaries: 1200,
    lost_frames: 0,
    lost_boundaries: 0,
    jumps_back: 0,
    cadence_errors: 0,
    frozen: false,
    weights: { mismatches: 0 },
    refreshes: { judged: 1196, uneven: 0, unresolved: 0 },
    unverified: { steps: 0 },
  };
}

test("a clean take passes", () => {
  expect(wallFramesFailures("wall-30", EXIT_CLEAN, clean())).toEqual([]);
});

test("judder fails and names the uneven boundaries", () => {
  const report = { ...clean(), verdict: "defects" as const };
  report.refreshes = { judged: 1196, uneven: 536, unresolved: 0 };
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([
    "wall-30: JUDDER: 536 of 1196 judged boundaries were held the wrong number of refreshes",
  ]);
});

/** Uneven boundaries held as a slip: long, then short, in turn. */
function held(...at: number[]) {
  return at.map((boundary, i) => ({ boundary, refreshes: i % 2 === 0 ? 3 : 1 }));
}

/** A take whose only defect is one slip at boundaries 708 and 709. */
function oneSlip(): WallFramesReport {
  return {
    ...clean(),
    verdict: "defects",
    refreshes: { judged: 1166, uneven: 2, unresolved: 0, uneven_boundaries: held(708, 709) },
  };
}

test("one slip per take passes (owner, #272, 30.10.2026)", () => {
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, oneSlip())).toEqual([]);
});

test("a lone uneven boundary is one slip and passes (#272, CI 41189187432)", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 3;
  report.refreshes!.uneven_boundaries = held(8, 708, 709);
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([]);
});

test("a freeze is no slip, however few boundaries it touches (#272)", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 1;
  report.refreshes!.uneven_boundaries = [{ boundary: 708, refreshes: 4 }];
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toHaveLength(1);
});

test("three slips per take pass (owner, #272, 31.10.2026)", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 6;
  report.refreshes!.uneven_boundaries = held(8, 9, 708, 709, 900, 901);
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([]);
});

test("four slips fail", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 8;
  report.refreshes!.uneven_boundaries = held(8, 9, 708, 709, 900, 901, 1000, 1001);
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([
    "wall-30: JUDDER: 8 of 1166 judged boundaries were held the wrong number of refreshes",
  ]);
});

test("four lone uneven boundaries are four slips and fail", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 4;
  report.refreshes!.uneven_boundaries = held(8, 208, 708, 900);
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toHaveLength(1);
});

test("a run of seven neighbours is four slips and fails", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 7;
  report.refreshes!.uneven_boundaries = held(700, 701, 702, 703, 704, 705, 706);
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toHaveLength(1);
});

test("a run of six neighbours is three slips and passes", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 6;
  report.refreshes!.uneven_boundaries = held(700, 701, 702, 703, 704, 705);
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([]);
});

test("fewer listed boundaries than uneven ones fail", () => {
  const report = oneSlip();
  report.refreshes!.uneven = 4;
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toHaveLength(1);
});

test("a slip with no listed boundaries fails", () => {
  const report = oneSlip();
  delete report.refreshes!.uneven_boundaries;
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toHaveLength(1);
});

test("a slip beside any other defect fails, and the slip is named", () => {
  for (const other of [
    { jumps_back: 1 },
    { lost_frames: 1 },
    { lost_boundaries: 1 },
    { cadence_errors: 1 },
    { frozen: true },
    { weights: { mismatches: 1 } },
  ]) {
    const failures = wallFramesFailures("wall-30", EXIT_DEFECTS, { ...oneSlip(), ...other });
    expect(failures).toHaveLength(2);
    expect(failures[0]).toContain("JUDDER");
  }
});

test("a slip in a take a clean verdict could not hold fails", () => {
  for (const weak of [
    { refreshes: { judged: 1166, uneven: 2, unresolved: 0, inconsistent: 1, uneven_boundaries: held(8, 9) } },
    { unverified: { steps: 1 } },
    { decoded: 2000 },
    { refreshes: { judged: 299, uneven: 2, unresolved: 0, uneven_boundaries: held(8, 9) } },
    { refreshes: { judged: 1166, uneven: 2, unresolved: 1, uneven_boundaries: held(8, 9) } },
  ]) {
    expect(wallFramesFailures("wall-30", EXIT_DEFECTS, { ...oneSlip(), ...weak })).not.toEqual([]);
  }
});

test("every defect counter is named, in one failure each", () => {
  const report: WallFramesReport = {
    ...clean(),
    verdict: "defects",
    lost_frames: 3,
    lost_boundaries: 4,
    jumps_back: 5,
    cadence_errors: 6,
    frozen: true,
    weights: { mismatches: 7 },
    refreshes: { judged: 900, uneven: 8, unresolved: 0 },
  };
  expect(wallFramesFailures("wall-25", EXIT_DEFECTS, report)).toEqual([
    "wall-25: JUDDER: 8 of 900 judged boundaries were held the wrong number of refreshes",
    "wall-25: 5 jump(s) back: an older frame shown after a newer one",
    "wall-25: 3 frame(s) of the clip never shown",
    "wall-25: 4 boundary(ies) lost",
    "wall-25: 6 cadence error(s): a step off the 30 fps grid",
    "wall-25: 7 blend weight(s) off the pacer's rule",
    "wall-25: the wall froze: fewer new pictures than half the grid's boundaries",
  ]);
});

test("a defects verdict with no boundary read still fails", () => {
  const report: WallFramesReport = {
    ...clean(),
    verdict: "defects",
    boundaries: 0,
    refreshes: null,
  };
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([
    "wall-30: no boundary of the clip was read",
  ]);
});

test("an exit 1 whose counters read zero still fails", () => {
  const report = { ...clean(), verdict: "defects" as const };
  expect(wallFramesFailures("wall-30", EXIT_DEFECTS, report)).toEqual([
    "wall-30: the reader reported defects (exit 1) that no counter names",
  ]);
});

test("an incomplete take fails as incomplete, never as judder or clean", () => {
  const report: WallFramesReport = {
    ...clean(),
    verdict: "incomplete",
    decoded: 2000,
    unverified: { steps: 12 },
    refreshes: { judged: 250, uneven: 0, unresolved: 3 },
  };
  expect(wallFramesFailures("wall-30", EXIT_INCOMPLETE, report)).toEqual([
    "wall-30: the measurement is INCOMPLETE (not judder, not clean): 12 step(s) the capture did not see, 2000 of 2400 pictures decoded, 3 boundary(ies) unresolved, 250 boundaries judged",
  ]);
});

test("an incomplete take with no refresh check says so", () => {
  const report: WallFramesReport = {
    ...clean(),
    verdict: "incomplete",
    samples: 0,
    refreshes: null,
  };
  expect(wallFramesFailures("wall-30", EXIT_INCOMPLETE, report)).toEqual([
    "wall-30: the measurement is INCOMPLETE (not judder, not clean): no refresh check",
  ]);
});

test("nothing decoded = the wall does not show SongPlayer's program", () => {
  const failures = wallFramesFailures("wall-30", EXIT_NOTHING_DECODED, null);
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain("does not show SongPlayer's program");
  // #272 v2: the reader also exits 4 when it finds no copy of the clip.
  expect(failures[0]).toContain("no whole copy of the clip's strip and its ruler is in view");
  const report = { ...clean(), decoded: 0, located: null };
  expect(wallFramesFailures("wall-30", EXIT_NOTHING_DECODED, report)).toEqual(failures);
});

test("a failed capture fails with no verdict", () => {
  const failures = wallFramesFailures("wall-30", EXIT_CAPTURE_FAILED, null);
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain("capture of the wall output failed");
});

test("a usage error fails", () => {
  expect(wallFramesFailures("wall-30", EXIT_USAGE, null)).toEqual([
    "wall-30: the reader refused its arguments (exit 2)",
  ]);
});

test("no report fails, whatever the exit", () => {
  expect(wallFramesFailures("wall-30", EXIT_CLEAN, null)).toEqual([
    "wall-30: the reader exited 0 with no report",
  ]);
  expect(wallFramesFailures("wall-30", null, null)).toEqual([
    "wall-30: the reader exited null with no report",
  ]);
});

test("an exit 0 whose verdict is not clean fails", () => {
  const report = { ...clean(), verdict: "incomplete" as const };
  expect(wallFramesFailures("wall-30", EXIT_CLEAN, report)[0]).toContain("INCOMPLETE");
});

test("an unknown exit with a clean report fails", () => {
  expect(wallFramesFailures("wall-30", 9, clean())).toEqual([
    "wall-30: the reader exited 9 with the verdict clean",
  ]);
});

/** One Arena layer with `clips` as [name, connected, description]. */
function layer(clips: Array<[string, boolean, string]>, bypassed = false): ArenaLayer {
  return {
    bypassed: { value: bypassed },
    clips: clips.map(([name, on, description]) => ({
      name: { value: name },
      connected: { value: on ? "Connected" : "Disconnected" },
      video: { description },
    })),
  };
}

test("the layers drawn above the MAX clip are named, bypassed ones left out (#272)", () => {
  const layers = [
    layer([["Botanical", false, "Botanical.mov"]]),
    layer([["Yt music", true, "SP-program-MAX"]]),
    layer([["Yt music", true, "SP-program-MAX"]]),
    layer([["Text Block", true, "Text Block"], ["vers", false, "Text Block"]]),
    layer([["holubica_B", true, "holubica_B.png"]], true),
    layer([["SNV BG", true, "NDI Source"]]),
    layer([]),
  ];
  expect(coveringLayers(layers)).toEqual(["L4 'Text Block'", "L6 'SNV BG'"]);
});

test("nothing above the MAX clip, or no MAX clip at all, is said as such (#272)", () => {
  expect(coveringLayers([layer([["Blank", true, "Blank"]]), layer([["Yt music", true, "SP-program-MAX"]])])).toEqual([]);
  expect(coveringLayers([layer([["Yt music", false, "SP-program-MAX"]]), layer([["Text", true, "Text Block"]])])).toEqual([
    "no connected Arena clip receives SP-program-MAX",
  ]);
  expect(coveringLayers([])).toEqual(["no connected Arena clip receives SP-program-MAX"]);
});

test("Arena's Blank generator draws nothing, so a Blank clip above the MAX clip is not named (#272)", () => {
  // SNV, 20.10.2026: L17-19 hold Arena's Blank generator, connected, and the
  // gate named them as covering the clip ("L17 'Blank', L18 'Blank', ...").
  const layers = [
    layer([["Yt music", true, "SP-program-MAX"]]),
    layer([["Blank", true, "Blank"]]),
    layer([["Blank", true, "Blank\nGenerator"]]),
    layer([["SNV BG", true, "NDI Source"]]),
  ];
  expect(coveringLayers(layers)).toEqual(["L4 'SNV BG'"]);
});
