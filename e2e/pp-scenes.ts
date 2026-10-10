/**
 * #229: which scenes the PP post-deploy subset presses through SongPlayer's
 * facade, and what it puts back, pure so the mock suite tests them
 * (`pp-scenes.spec.ts`); the box reads are `post-deploy-pp.spec.ts`.
 *
 * - The PLAYLIST scene comes from SongPlayer's own scene catalog, never from
 *   the facade's scene list: that list is cg OBS's, forwarded
 *   (`remote/protocol.rs::with_songplayer_scenes`), and PP's cg OBS has no
 *   `sp-*` scene of its own. The catalog (`playback/scene_catalog.rs`): an
 *   ACTIVE playlist's `ndi_output_name`, ASCII-lowercased, is its scene; a
 *   blank one, or one another active playlist shares, names none.
 * - The MANUAL scene is the scene cg OBS already has on program: pressing it
 *   cuts SP-program to "OBS manuál" and leaves what cg OBS shows as it is.
 *   The repo variable `PP_MANUAL_SCENE` overrides it. The gate never guesses
 *   another cg OBS scene: that would change what PP's wall shows. A manual
 *   scene that itself shows `SP-program` would loop the program into itself
 *   once it is on "OBS manuál"; nothing here can see that (a precondition,
 *   `peer-exchange.md` "PP deploy").
 * - What goes BACK after the gate: SP-program's start source only while the
 *   gate's own last press is still the latest switch (`programRestoreTarget`),
 *   cg OBS's scene only while the gate's move is still there
 *   (`cgRestoreTarget`). An operator's press meanwhile is kept.
 */

import { AV_PROBE_SCENE } from "./av-sync-probe";
import { pickBaselineScene } from "./obs-baseline-scene";
import { isDabing, type PlaylistRow } from "./program-state";

/** "OBS manuál" (sp-core `PROGRAM_INPUT_LABEL`): the NDI input's name in the
 *  scene resolver, not a cg OBS scene. */
export const OBS_MANUAL = "OBS manuál";

/** Rust's `to_ascii_lowercase` (the catalog's case rule). */
function asciiLower(s: string): string {
  return s.replace(/[A-Z]/g, (c) => c.toLowerCase());
}

/** The scene catalog: scene name → playlist id, sorted by scene name. */
export function catalogScenes(rows: PlaylistRow[]): Map<string, number> {
  const named = new Map<string, number[]>();
  for (const r of rows) {
    if (!r.is_active || r.ndi_output_name.trim() === "") continue;
    const scene = asciiLower(r.ndi_output_name);
    named.set(scene, [...(named.get(scene) ?? []), r.id]);
  }
  const scenes = [...named.entries()]
    .filter(([, ids]) => ids.length === 1)
    .map(([scene, ids]): [string, number] => [scene, ids[0]])
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return new Map(scenes);
}

/** Every name an active playlist carries (lowercased): a press of one of
 *  them is never the manual-scene test. */
export function playlistNames(rows: PlaylistRow[]): Set<string> {
  return new Set(
    rows
      .filter((r) => r.is_active && r.ndi_output_name.trim() !== "")
      .map((r) => asciiLower(r.ndi_output_name)),
  );
}

/**
 * The playlist scene the gate puts on program: a catalog scene whose
 * playlist is not refused (`GET /api/v1/program` → `cut_refused`), not the
 * Dabing one, and has a playable video (`playable`: playlist id → its
 * normalized videos; missing = none). Among those, the SNV suites' baseline
 * discipline (`pickBaselineScene`): `sp-slow`, else any `sp-*` but `sp-fast`,
 * `sp-warmup` and `sp-90s` (#230: a cut to it holds the node's background
 * jobs for 4 h), else a catalog scene that is not `sp-*`, else the first that
 * is not `sp-90s` (so `sp-fast` or `sp-warmup` when they are all that is
 * left: a playlist is still tested; `sp-90s` only when it is the one there
 * is). `null` when there is none.
 */
export function pickPlaylistScene(
  rows: PlaylistRow[],
  refused: number[],
  playable: Map<number, number>,
): { scene: string; playlistId: number } | null {
  const byId = new Map(rows.map((r) => [r.id, r]));
  const candidates = [...catalogScenes(rows).entries()].filter(([, id]) => {
    const row = byId.get(id);
    return row !== undefined && !refused.includes(id) && !isDabing(row) && (playable.get(id) ?? 0) > 0;
  });
  if (candidates.length === 0) return null;
  const names = candidates.map(([scene]) => scene);
  const scene = pickBaselineScene(names);
  return { scene, playlistId: candidates[names.indexOf(scene)][1] };
}

/** What the manual-scene pick reads. */
export interface ManualSceneInput {
  /** `PP_MANUAL_SCENE` (may be blank). */
  configured: string;
  /** cg OBS's own program scene, read on cg OBS just before the press. */
  cgProgram: string;
  /** cg OBS's scenes (the facade's forwarded `GetSceneList`). */
  scenes: string[];
  /** `playlistNames(rows)`. */
  playlistNames: Set<string>;
}

/** A manual scene: not blank, none of the active playlists' names
 *  (`playlistNames`), not the A/V gate's probe scene, not "OBS manuál". */
export function isManualScene(scene: string, playlistNames: Set<string>): boolean {
  return (
    scene.trim() !== "" &&
    !playlistNames.has(asciiLower(scene)) &&
    scene !== AV_PROBE_SCENE &&
    scene !== OBS_MANUAL
  );
}

/**
 * The manual scene the gate presses: `PP_MANUAL_SCENE` when set (it must be
 * one of cg OBS's scenes and a manual one, `isManualScene`), else cg OBS's
 * own program scene when that is a manual scene. Anything else is an error
 * naming `PP_MANUAL_SCENE`: the gate never picks another cg OBS scene by
 * itself.
 */
export function pickManualScene(o: ManualSceneInput): { scene: string } | { error: string } {
  const manual = (s: string) => isManualScene(s, o.playlistNames);
  const configured = o.configured.trim();
  if (configured !== "") {
    if (!o.scenes.includes(configured)) {
      return {
        error: `PP_MANUAL_SCENE "${configured}" is not one of cg OBS's scenes ${JSON.stringify(o.scenes)}`,
      };
    }
    if (!manual(configured)) {
      return {
        error: `PP_MANUAL_SCENE "${configured}" is not a manual scene (a playlist's, the A/V probe's or "${OBS_MANUAL}")`,
      };
    }
    return { scene: configured };
  }
  if (o.scenes.includes(o.cgProgram) && manual(o.cgProgram)) return { scene: o.cgProgram };
  return {
    error:
      `cg OBS's program scene "${o.cgProgram}" is not a manual scene: set the repo variable ` +
      "PP_MANUAL_SCENE to the manual scene the PP gate may press",
  };
}

/** The cg OBS scene the gate puts back after its manual press: the scene cg
 *  OBS was on (`moved.from`) when the gate moved it to `moved.to` and cg OBS
 *  is still there (`now`); `null` otherwise — a gate that never moved cg OBS
 *  restores nothing, and an operator's later change is kept. */
export function cgRestoreTarget(
  moved: { from: string; to: string } | null,
  now: string,
): string | null {
  return moved !== null && now === moved.to ? moved.from : null;
}

/** A facade press the gate sent: the scene and the instant it was sent
 *  (Unix ms; the runner and SongPlayer share PP's clock). */
export interface GatePress {
  scene: string;
  sentAtMs: number;
  /** The source the press puts on program when it is not refused: the
   *  playlist's id, or -1 ("OBS manuál") for a manual scene. */
  source: number;
}

/** `remote.last_remote_cut` of `GET /api/v1/program`, the fields the
 *  restore reads (`at_ms`: Unix ms, when the switch was recorded). */
export interface LastCutView {
  scene: string;
  action: string;
  at_ms: number;
}

/** A pressed scene as the server records it (`remote::clip`: its first 64
 *  characters, Unicode scalar values). */
export function recordedScene(scene: string): string {
  return [...scene].slice(0, 64).join("");
}

/** Whether the latest recorded switch (`remote.last_remote_cut`) is the
 *  gate's own last press: its scene (as recorded), recorded at or after the
 *  instant the gate sent it (a refused keep included). */
export function gateIsLatest(presses: GatePress[], cut: LastCutView | null): boolean {
  const last = presses.at(-1);
  return (
    last !== undefined &&
    cut !== null &&
    cut.scene === recordedScene(last.scene) &&
    cut.at_ms >= last.sentAtMs
  );
}

/**
 * The source SP-program goes back to after the gate (`POST
 * /api/v1/program/cut`, which tells cg OBS nothing): the start source, only
 * while the program is still what the gate left — the latest recorded
 * switch is the gate's own last press (`gateIsLatest`) and the program is on
 * that press's source, or, when that last press was refused (a keep: the NDI
 * input inactive, cg OBS refusing), on the source of the press before it —
 * and the program is not on the start source already. `null` otherwise: no
 * press, nothing was on program at the start, or someone switched since (an
 * operator's press or cut is kept).
 */
export function programRestoreTarget(
  start: number | null,
  presses: GatePress[],
  now: { source: number | null; last_remote_cut: LastCutView | null },
): number | null {
  const cut = now.last_remote_cut;
  if (start === null || cut === null || !gateIsLatest(presses, cut)) return null;
  const left = cut.action === "keep" ? presses.at(-2) : presses.at(-1);
  if (left === undefined || now.source !== left.source) return null;
  return now.source === start ? null : start;
}

/**
 * How cg OBS goes back on its own scene after the gate moved it
 * (`cgRestoreTarget`): through the facade when the gate left SP-program on
 * "OBS manuál" (-1), no cut back is due, nothing switched since and that
 * scene is a manual one (`isManualScene`) — the press moves cg OBS and
 * SongPlayer names the program by that scene again — else on cg OBS
 * directly (SP-program is cut back to a playlist, or someone switched since:
 * a facade press would cut SP-program to "OBS manuál"; a playlist's name
 * would cut SP-program to that playlist).
 */
export function cgRestoreVia(o: {
  programBack: number | null;
  gateIsLatest: boolean;
  source: number | null;
  backIsManual: boolean;
}): "facade" | "cg" {
  const facade = o.programBack === null && o.gateIsLatest && o.source === -1 && o.backIsManual;
  return facade ? "facade" : "cg";
}

/** `GET /api/v1/program`, the fields the manual-cut check reads. */
export interface ManualCutView {
  source: number | null;
  remote: {
    last_remote_cut: {
      scene: string;
      action: string;
      source: number | null;
      cg_forward: string | null;
    } | null;
  };
}

/** The press of manual scene `scene` landed: "OBS manuál" (-1) is on
 *  program, and the last facade switch cut the input for that scene after
 *  cg OBS accepted it (`remote.last_remote_cut`). */
export function manualCutLanded(p: ManualCutView, scene: string): boolean {
  const cut = p.remote.last_remote_cut;
  return (
    p.source === -1 &&
    cut !== null &&
    cut.scene === recordedScene(scene) &&
    cut.action === "input" &&
    cut.source === -1 &&
    cut.cg_forward === "ok"
  );
}
