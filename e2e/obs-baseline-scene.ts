/**
 * The off-program BASELINE scene the post-deploy suites park OBS on.
 *
 * Shared by `post-deploy.spec.ts` and `post-deploy-av-sync.spec.ts` (#147) so
 * every suite that drives the live win-resolume OBS obeys ONE discipline
 * (CLAUDE.md "E2E must not switch to disruptive OBS scenes").
 *
 * Picking it was historically `find((s) => !s.startsWith("sp-"))`, which on
 * win-resolume resolved to a sound-sync QR-code "test" scene. That scene
 * disrupts the wall and the LED audience whenever E2E runs against a live
 * machine. So the picker chooses another sp-* scene instead: any one that is
 * not sp-fast (under test) and not sp-warmup (also disturbing, per the
 * operator). It falls back to a non-sp scene only if no other sp-* exists.
 *
 * #230: never sp-90s either — a cut to it holds every background job of the
 * node for 4 h (until sp-slow is cut), so a gate that parked on it would
 * stop the box's downloads and processing after each deploy.
 *
 * #285: the A/V gate compares the program with the song's original sidecars,
 * so it measures only a playlist with no gain or EQ of its own
 * (`pickFlatAudioScene`). The operator set ytslow to -21 dB + high-pass during
 * the 1.11.2026 service: correlation 0.669, a red deploy, nothing wrong. His
 * settings stay his: the gate moves to another scene, never changes them.
 */

import { AV_PROBE_SCENE } from "./av-sync-probe";

export const DISALLOWED_BASELINE_SCENES = new Set(["sp-fast", "sp-warmup", "sp-90s"]);

export function pickBaselineScene(scenes: string[]): string {
  // Prefer sp-slow specifically — it's a quiet music scene operators
  // routinely use as a "background" state.
  if (scenes.includes("sp-slow")) return "sp-slow";
  // Fall back to any other sp-* that isn't disallowed.
  const otherSp = scenes.find(
    (s) => s.startsWith("sp-") && !DISALLOWED_BASELINE_SCENES.has(s),
  );
  if (otherSp) return otherSp;
  // Last resort — non-sp scene. This may be the disruptive QR-code
  // test scene, but it's better than running a test where the baseline
  // and the sp-fast probe scene collide. Never the A/V gate's own probe
  // scene (#221 lane 3): it shows SP-program, i.e. the program itself.
  const nonSp = scenes.find((s) => !s.startsWith("sp-") && s !== AV_PROBE_SCENE);
  if (nonSp) return nonSp;
  // #230: even then, sp-90s only when it is the one scene there is.
  return scenes.find((s) => s !== "sp-90s") ?? scenes[0];
}

/** A playlist's `GET /api/v1/playlists/{id}/audio`, the fields read here. */
export interface PlaylistAudio {
  gain_db: number;
  eq: Array<{ enabled: boolean }>;
}

export function playlistAudioIsFlat(audio: PlaylistAudio): boolean {
  return audio.gain_db === 0 && !audio.eq.some((band) => band.enabled);
}

const FILLED = ["sp-slow", "sp-worship", "sp-presence"];

/** The baseline among `flat`, the scenes of flat playlists; null with none. */
export function pickFlatAudioScene(scenes: string[], flat: string[]): string | null {
  const usable = scenes.filter((s) => flat.includes(s) && !DISALLOWED_BASELINE_SCENES.has(s));
  if (usable.length === 0) return null;
  // sp-live and the like can have no song.
  return FILLED.find((s) => usable.includes(s)) ?? pickBaselineScene(usable);
}
