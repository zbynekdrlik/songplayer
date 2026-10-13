import { test, expect } from "@playwright/test";
import type { HealthRow } from "./box-api";
import { classifyProgram, type PlaylistRow } from "./program-state";
import { previewTarget } from "./pp-preview-gate";

// #266: the decision of PP's post-deploy preview gate
// (`post-deploy-pp-preview.spec.ts`): the card of the playlist ON PROGRAM,
// while it decodes; anything else fails naming why, never a skip.

function row(id: number, name: string, ndi: string, kind = "youtube"): PlaylistRow {
  return { id, name, ndi_output_name: ndi, is_active: true, kind };
}

const ROWS: PlaylistRow[] = [
  row(4, "ytslow", "SP-slow"),
  row(7, "ytfast", "SP-fast"),
  row(9, "Dabing", "SP-dabing", "dabing"),
];

function health(pid: number, state: string, transport?: string): HealthRow {
  return {
    playlist_id: pid,
    ndi_name: `SP-${pid}`,
    frames_submitted_last_5s: 150,
    state,
    transport,
  };
}

test.describe("PP's preview gate opens the card on program (#266)", () => {
  test("the playlist on program, while it decodes", () => {
    const program = classifyProgram("sp-slow", [4], ROWS);
    const got = previewTarget(program, [
      health(4, "Playing", "Playing"),
      health(7, "Paused", "Paused"),
    ]);
    expect(got).toEqual({ target: ROWS[0] });
  });

  test("the Dabing playlist on program is opened like any other, while it decodes", () => {
    const program = classifyProgram("sp-dabing", [9], ROWS);
    expect(previewTarget(program, [health(9, "Playing", "Playing")])).toEqual({
      target: ROWS[2],
    });
  });

  test("nothing on program is the operator's branch, never a failure — no scene is switched", () => {
    const program = classifyProgram("OBS manuál", [], ROWS);
    const got = previewTarget(program, [health(7, "Paused", "Playing")]);
    expect("none" in got && got.none).toContain(
      "nothing is on SongPlayer's program (scene OBS manuál)",
    );
    expect("error" in got).toBe(false);
  });

  test("the playlist on program that does not decode is opened to check its explanation", () => {
    const program = classifyProgram("sp-slow", [4], ROWS);
    for (const transport of ["Paused", "Idle", undefined]) {
      const got = previewTarget(program, [health(4, "Paused", transport)]);
      expect("explained" in got && got.explained, `transport ${transport}`).toEqual(ROWS[0]);
      expect("why" in got && got.why, `transport ${transport}`).toContain(
        `the playlist on program, 4 (ytslow), does not decode (transport ${transport ?? "none"}, state Paused)`,
      );
    }
  });

  test("the playlist on program with no health row fails naming it", () => {
    const program = classifyProgram("sp-slow", [4], ROWS);
    const got = previewTarget(program, [health(7, "Paused", "Playing")]);
    expect("error" in got && got.error).toContain(
      "the playlist on program, 4 (ytslow), has no row on /api/v1/ndi/health",
    );
  });
});
