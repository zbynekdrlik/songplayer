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
  // RED: never skips — a same-scene switch still runs the disruptive
  // studio-mode transition. GREEN skips when already on the target.
  return false;
}

/** True once the switch has fully applied: the program scene equals the target
 * AND no transition is still running. A name-only check is satisfied mid-fade,
 * so `transitionActive` is essential. */
export function sceneSwitchSettled(
  programScene: string,
  target: string,
  transitionActive: boolean,
): boolean {
  // RED: name-only — reports settled while the fade is still running (the
  // round-2 bug). GREEN also requires the transition to have ended.
  return programScene === target;
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

export interface WaitForProgramSceneOptions {
  /** Give up (throw) after this many ms. Default 8000 (covers the 2 s fade). */
  timeoutMs?: number;
  /** Delay between `getProgramScene` reads. Default 150 ms. */
  pollMs?: number;
}

/**
 * Resolve once `getProgramScene()` returns `target`; throw if it never does
 * within `timeoutMs`. Superseded by `waitForSceneSwitchApplied` (name-only —
 * satisfied mid-fade); kept until the driver migrates.
 */
export async function waitForProgramScene(
  getProgramScene: () => Promise<string>,
  target: string,
  opts: WaitForProgramSceneOptions = {},
): Promise<void> {
  const timeoutMs = opts.timeoutMs ?? 8000;
  const pollMs = opts.pollMs ?? 150;
  const deadline = Date.now() + timeoutMs;

  for (;;) {
    const current = await getProgramScene();
    if (current === target) return;
    if (Date.now() >= deadline) {
      throw new Error(
        `OBS program scene did not become "${target}" within ${timeoutMs}ms ` +
          `(last saw "${current}"). (#170)`,
      );
    }
    await new Promise((r) => setTimeout(r, pollMs));
  }
}
