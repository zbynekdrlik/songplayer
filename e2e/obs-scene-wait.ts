/**
 * Deterministic wait for an OBS program-scene switch to actually take effect
 * (#170).
 *
 * OBS on win-resolume runs **Studio Mode** with a **`Fade` transition of
 * 2000ms**. In studio mode `SetCurrentProgramScene` merely *starts* the fade;
 * OBS updates `GetCurrentProgramScene` — and emits the
 * `CurrentProgramSceneChanged` event SongPlayer reacts to — only when the
 * transition **completes** (~2 s later). A blind sleep after
 * `SetCurrentProgramScene` therefore races the fade, which is what made the
 * post-deploy suite flaky (tests 15/17). This helper polls the program scene
 * until it equals the target and fails loudly if it never does, so it is
 * robust to any transition duration (a 0 ms cut or a 2 s fade) and turns a
 * genuinely-stuck transition / missing scene into a clear error instead of a
 * mysterious downstream failure.
 *
 * Separated from Playwright I/O (like `ndi-health-gate.ts`) so it is
 * unit-testable in the ubuntu mock suite without a browser or the box.
 */

export interface WaitForProgramSceneOptions {
  /** Give up (throw) after this many ms. Default 8000 (covers the 2 s fade). */
  timeoutMs?: number;
  /** Delay between `getProgramScene` reads. Default 150 ms. */
  pollMs?: number;
}

/**
 * Resolve once `getProgramScene()` returns `target`; throw if it never does
 * within `timeoutMs`.
 */
export async function waitForProgramScene(
  getProgramScene: () => Promise<string>,
  target: string,
  _opts: WaitForProgramSceneOptions = {},
): Promise<void> {
  // RED stub (#170): reads the program scene once and returns without waiting
  // for the transition to apply. GREEN replaces this with a bounded poll.
  await getProgramScene();
}
