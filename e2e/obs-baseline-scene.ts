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
 */

export const DISALLOWED_BASELINE_SCENES = new Set(["sp-fast", "sp-warmup"]);

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
  // and the sp-fast probe scene collide.
  const nonSp = scenes.find((s) => !s.startsWith("sp-"));
  if (nonSp) return nonSp;
  return scenes[0];
}
