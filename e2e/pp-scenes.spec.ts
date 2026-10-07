import { test, expect } from "@playwright/test";
import {
  OBS_MANUAL,
  catalogScenes,
  cgRestoreTarget,
  manualCutLanded,
  pickManualScene,
  pickPlaylistScene,
  playlistNames,
  programRestoreTarget,
} from "./pp-scenes";
import { AV_PROBE_SCENE } from "./av-sync-probe";
import type { PlaylistRow } from "./program-state";

function row(id: number, ndi: string, over: Partial<PlaylistRow> = {}): PlaylistRow {
  return { id, name: `pl${id}`, ndi_output_name: ndi, is_active: true, kind: "youtube", ...over };
}

const ROWS: PlaylistRow[] = [
  row(1, "SP-fast"),
  row(2, "SP-slow"),
  row(3, "SP-worship"),
  row(4, "SP-warmup"),
  row(5, "SP-dabing", { kind: "dabing" }),
];
const VIDEOS = new Map<number, number>([
  [1, 40],
  [2, 30],
  [3, 25],
  [4, 5],
  [5, 3],
]);

test.describe("PP's playlist scene (#229)", () => {
  test("the scene catalog: active, named, lowercased, a shared name is nobody's", () => {
    const rows = [
      row(1, "SP-Fast"),
      row(2, " "),
      row(3, "SP-slow", { is_active: false }),
      row(4, "SP-x"),
      row(5, "sp-X"),
      row(6, "SP-worship"),
    ];
    expect([...catalogScenes(rows).entries()]).toEqual([
      ["sp-fast", 1],
      ["sp-worship", 6],
    ]);
  });

  test("sp-slow first, as the SNV suites do", () => {
    expect(pickPlaylistScene(ROWS, [], VIDEOS)).toEqual({ scene: "sp-slow", playlistId: 2 });
  });

  test("a refused playlist, one with no playable video, and the Dabing one are never picked", () => {
    expect(pickPlaylistScene(ROWS, [2], VIDEOS)).toEqual({ scene: "sp-worship", playlistId: 3 });
    const noSlowVideos = new Map(VIDEOS).set(2, 0);
    expect(pickPlaylistScene(ROWS, [], noSlowVideos)).toEqual({
      scene: "sp-worship",
      playlistId: 3,
    });
    const onlyDabing = [row(5, "SP-dabing", { kind: "dabing" })];
    expect(pickPlaylistScene(onlyDabing, [], VIDEOS)).toBeNull();
  });

  test("a playlist missing from the playable counts has none", () => {
    expect(pickPlaylistScene([row(9, "SP-slow")], [], new Map())).toBeNull();
  });

  test("one playable video is enough", () => {
    expect(pickPlaylistScene([row(9, "SP-slow")], [], new Map([[9, 1]]))).toEqual({
      scene: "sp-slow",
      playlistId: 9,
    });
  });

  test("nothing to play is null", () => {
    expect(pickPlaylistScene([], [], VIDEOS)).toBeNull();
  });
});

test.describe("PP's manual scene (#229)", () => {
  const scenes = ["Blank", "Svedectvo", "sp-slow", AV_PROBE_SCENE, OBS_MANUAL];
  const names = playlistNames(ROWS);

  test("the active playlists' names, lowercased", () => {
    expect([...names].sort()).toEqual(["sp-dabing", "sp-fast", "sp-slow", "sp-warmup", "sp-worship"]);
    expect(playlistNames([row(1, "SP-Fast", { is_active: false }), row(2, "  ")]).size).toBe(0);
  });

  test("by default the scene cg OBS already has on program", () => {
    const pick = pickManualScene({ configured: "", cgProgram: "Svedectvo", scenes, playlistNames: names });
    expect(pick).toEqual({ scene: "Svedectvo" });
  });

  test("cg OBS on a playlist's, the probe's or an unlisted scene: no guess, PP_MANUAL_SCENE is named", () => {
    // Pressing some other cg OBS scene would change what PP's wall shows.
    for (const cgProgram of ["SP-Slow", AV_PROBE_SCENE, OBS_MANUAL, "gone"]) {
      const pick = pickManualScene({ configured: "", cgProgram, scenes, playlistNames: names });
      expect(pick, cgProgram).toEqual({
        error:
          `cg OBS's program scene "${cgProgram}" is not a manual scene: set the repo variable ` +
          "PP_MANUAL_SCENE to the manual scene the PP gate may press",
      });
    }
  });

  test("PP_MANUAL_SCENE wins when it is a manual cg OBS scene", () => {
    const pick = pickManualScene({
      configured: " Blank ",
      cgProgram: "Svedectvo",
      scenes,
      playlistNames: names,
    });
    expect(pick).toEqual({ scene: "Blank" });
  });

  test("a PP_MANUAL_SCENE cg OBS does not have fails", () => {
    const pick = pickManualScene({ configured: "Nope", cgProgram: "Blank", scenes, playlistNames: names });
    expect(pick).toEqual({
      error: `PP_MANUAL_SCENE "Nope" is not one of cg OBS's scenes ${JSON.stringify(scenes)}`,
    });
  });

  test("a PP_MANUAL_SCENE that is a playlist's, the probe's or OBS manual fails", () => {
    for (const configured of ["sp-slow", AV_PROBE_SCENE, OBS_MANUAL]) {
      const pick = pickManualScene({ configured, cgProgram: "Blank", scenes, playlistNames: names });
      expect(pick, configured).toEqual({
        error: `PP_MANUAL_SCENE "${configured}" is not a manual scene (a playlist's, the A/V probe's or "${OBS_MANUAL}")`,
      });
    }
  });

  test("a blank PP_MANUAL_SCENE is unset", () => {
    const pick = pickManualScene({ configured: "  ", cgProgram: "Svedectvo", scenes, playlistNames: names });
    expect(pick).toEqual({ scene: "Svedectvo" });
  });
});

test.describe("PP's manual cut (#229)", () => {
  const landed = {
    source: -1,
    remote: {
      last_remote_cut: { scene: "Blank", action: "input", source: -1, cg_forward: "ok" },
    },
  };

  test("OBS manual on program, cut for the scene, cg OBS accepted", () => {
    expect(manualCutLanded(landed, "Blank")).toBe(true);
  });

  test("any other state is not landed", () => {
    expect(manualCutLanded({ ...landed, source: 2 }, "Blank")).toBe(false);
    expect(manualCutLanded(landed, "Svedectvo")).toBe(false);
    const refused = { ...landed.remote.last_remote_cut, cg_forward: "error 600" };
    expect(manualCutLanded({ ...landed, remote: { last_remote_cut: refused } }, "Blank")).toBe(false);
    const kept = { ...landed.remote.last_remote_cut, action: "keep" };
    expect(manualCutLanded({ ...landed, remote: { last_remote_cut: kept } }, "Blank")).toBe(false);
    expect(manualCutLanded({ source: -1, remote: { last_remote_cut: null } }, "Blank")).toBe(false);
  });
});

test.describe("SP-program's restore after the PP gate (#229)", () => {
  const press = { scene: "sp-slow", sentAtMs: 1_000 };
  const ours = { scene: "sp-slow", action: "playlist", at_ms: 1_200 };

  test("the gate's own cut still on program goes back to the start source", () => {
    expect(programRestoreTarget(-1, press, { source: 4, last_remote_cut: ours })).toBe(-1);
  });

  test("a cut sent at the same millisecond as the press is the gate's", () => {
    const sameMs = { ...ours, at_ms: 1_000 };
    expect(programRestoreTarget(-1, press, { source: 4, last_remote_cut: sameMs })).toBe(-1);
  });

  test("a cut from before the press is not the gate's", () => {
    const earlier = { ...ours, at_ms: 999 };
    expect(programRestoreTarget(-1, press, { source: 4, last_remote_cut: earlier })).toBeNull();
  });

  test("an operator's press since the gate's is kept", () => {
    const operator = { scene: "Svedectvo", action: "input", at_ms: 1_500 };
    expect(programRestoreTarget(4, press, { source: -1, last_remote_cut: operator })).toBeNull();
  });

  test("a kept (refused) press restores nothing", () => {
    const kept = { ...ours, action: "keep" };
    expect(programRestoreTarget(-1, press, { source: 4, last_remote_cut: kept })).toBeNull();
  });

  test("nothing to put back: no press, no start, or already on the start source", () => {
    expect(programRestoreTarget(-1, null, { source: 4, last_remote_cut: ours })).toBeNull();
    expect(programRestoreTarget(null, press, { source: 4, last_remote_cut: ours })).toBeNull();
    expect(programRestoreTarget(4, press, { source: 4, last_remote_cut: ours })).toBeNull();
    expect(programRestoreTarget(-1, press, { source: 4, last_remote_cut: null })).toBeNull();
  });
});

test.describe("cg OBS's restore after the PP gate (#229)", () => {
  test("cg OBS is put back only when the gate moved it and it is still there", () => {
    expect(cgRestoreTarget({ from: "Svedectvo", to: "Blank" }, "Blank")).toBe("Svedectvo");
  });

  test("an operator's change after the press is kept", () => {
    expect(cgRestoreTarget({ from: "Svedectvo", to: "Blank" }, "Bannery")).toBeNull();
  });

  test("a gate that never moved cg OBS restores nothing", () => {
    expect(cgRestoreTarget(null, "Bannery")).toBeNull();
  });
});
