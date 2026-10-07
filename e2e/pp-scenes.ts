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
 *   another cg OBS scene: that would change what PP's wall shows.
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
 * discipline (`pickBaselineScene`): `sp-slow`, else any `sp-*` but `sp-fast`
 * and `sp-warmup`. `null` when there is none.
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

/**
 * The manual scene the gate presses: `PP_MANUAL_SCENE` when set (it must be
 * one of cg OBS's scenes and a manual one), else cg OBS's own program scene
 * when that is a manual scene. Anything else is an error naming
 * `PP_MANUAL_SCENE`: the gate never picks another cg OBS scene by itself. A
 * manual scene is none of: an active playlist's name, the A/V gate's probe
 * scene, "OBS manuál".
 */
export function pickManualScene(o: ManualSceneInput): { scene: string } | { error: string } {
  const manual = (s: string) =>
    s.trim() !== "" &&
    !o.playlistNames.has(asciiLower(s)) &&
    s !== AV_PROBE_SCENE &&
    s !== OBS_MANUAL;
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
    cut.scene === scene &&
    cut.action === "input" &&
    cut.source === -1 &&
    cut.cg_forward === "ok"
  );
}
