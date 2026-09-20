/**
 * Deterministic waits for an OBS program-scene switch to actually take effect
 * (#170).
 *
 * OBS on win-resolume runs **Studio Mode** with a **`Fade` transition of
 * 2000ms**. Two consequences the naive "SetCurrentProgramScene then sleep"
 * approach races (round 3):
 *
 *  1. A SAME-scene `SetCurrentProgramScene` still runs a real 2 s transition,
 *     leaving `preview == program == that scene`. From that state OBS then
 *     DROPS the next `SetCurrentProgramScene`'s `CurrentProgramSceneChanged`
 *     event (reproduced live) — the driver must never issue a same-scene
 *     switch (`shouldSkipSceneSwitch`).
 *  2. `GetCurrentProgramScene` reports the target *during* the fade, so a
 *     name-only wait returns before the transition has actually ended and
 *     before SongPlayer's event has fired — the wait must require BOTH the
 *     target reached AND the transition ended (`sceneSwitchSettled` /
 *     `waitForSceneSwitchApplied`).
 *
 * These decisions are separated from Playwright / obs-websocket I/O (like
 * `ndi-health-gate.ts`) so they are unit-testable in the ubuntu mock suite
 * without a browser or the box.
 */

/** True when the program scene is already the target — issuing the switch
 * would run a pointless same-scene transition, so the driver skips it. */
export function shouldSkipSceneSwitch(current: string, target: string): boolean {
  return current === target;
}

/** True once the switch has fully applied: the program scene equals the target
 * AND no transition is still running. A name-only check is satisfied mid-fade,
 * so `transitionActive` is essential. */
export function sceneSwitchSettled(
  programScene: string,
  target: string,
  transitionActive: boolean,
): boolean {
  return programScene === target && !transitionActive;
}

export interface WaitForSceneSwitchOptions {
  /** Give up (throw) after this many ms. Default 8000 (covers the 2 s fade). */
  timeoutMs?: number;
  /** Delay between polls. Default 150 ms. */
  pollMs?: number;
}

/**
 * Poll until the scene switch is settled (`sceneSwitchSettled`) — the program
 * scene is the target AND the transition has ended — or throw loudly once the
 * deadline passes. Robust to any transition duration (a 0 ms cut or a 2 s
 * fade) and turns a dropped/stuck switch into a clear error instead of a
 * mysterious downstream failure.
 */
export async function waitForSceneSwitchApplied(
  getProgramScene: () => Promise<string>,
  isTransitionActive: () => boolean,
  target: string,
  opts: WaitForSceneSwitchOptions = {},
): Promise<void> {
  const timeoutMs = opts.timeoutMs ?? 8000;
  const pollMs = opts.pollMs ?? 150;
  const deadline = Date.now() + timeoutMs;

  for (;;) {
    const current = await getProgramScene();
    if (sceneSwitchSettled(current, target, isTransitionActive())) return;
    if (Date.now() >= deadline) {
      throw new Error(
        `OBS scene switch to "${target}" did not settle within ${timeoutMs}ms ` +
          `(last program "${current}", transitionActive=${isTransitionActive()}). ` +
          `On win-resolume OBS runs Studio Mode with a 2000ms Fade — the switch ` +
          `must reach the target AND the transition must end; a dropped/stuck ` +
          `transition or a missing scene surfaces here instead of as a ` +
          `mysterious downstream failure. (#170)`,
      );
    }
    await new Promise((r) => setTimeout(r, pollMs));
  }
}

/**
 * Poll until OBS reports `target` as the PREVIEW scene, or throw (round 4,
 * 19.9.2026 — three red post-deploy runs in one day).
 *
 * `SetCurrentPreviewScene` returns before OBS's UI thread has applied it. A
 * `TriggerStudioModeTransition` sent right behind it can therefore still see the
 * OLD preview; when that old preview equals the program scene OBS runs a real
 * 2 s fade from the scene to ITSELF: `SceneTransitionEnded` fires, the program
 * never changes, and the switch "did not settle" (last program = the old scene,
 * `transitionActive=false`, preview = the target — exactly what the box showed).
 * So the driver must see the preview applied BEFORE it triggers.
 */
export async function waitForPreviewApplied(
  getPreviewScene: () => Promise<string>,
  target: string,
  opts: WaitForSceneSwitchOptions = {},
): Promise<void> {
  const timeoutMs = opts.timeoutMs ?? 3000;
  const pollMs = opts.pollMs ?? 50;
  const deadline = Date.now() + timeoutMs;

  for (;;) {
    const current = await getPreviewScene();
    if (current === target) return;
    if (Date.now() >= deadline) {
      throw new Error(
        `OBS preview scene did not become "${target}" within ${timeoutMs}ms ` +
          `(last preview "${current}") — refusing to trigger the studio-mode ` +
          `transition against a stale preview (it would fade the program scene ` +
          `to itself).`,
      );
    }
    await new Promise((r) => setTimeout(r, pollMs));
  }
}
