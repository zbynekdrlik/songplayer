/**
 * Unit tests for the deterministic OBS scene-switch decisions (#170).
 *
 * Runs in the ubuntu mock suite (playwright.config.ts) — no browser and no
 * deployed box; these `test()` blocks never touch the `page` fixture. They
 * pin the contract `ObsDriver.switchScene` relies on: OBS on win-resolume runs
 * Studio Mode with a 2000ms Fade, so (a) a same-scene switch must be skipped
 * (it runs a real transition that then drops the NEXT switch's event) and
 * (b) `GetCurrentProgramScene` reports the target mid-fade, so the wait must
 * require the transition to have actually ENDED, not just the name to match.
 */

import { test, expect } from "@playwright/test";
import {
  shouldSkipSceneSwitch,
  sceneSwitchSettled,
  waitForSceneSwitchApplied,
  waitForPreviewApplied,
} from "./obs-scene-wait";

test.describe("OBS studio-mode scene-switch decisions (#170 round 3)", () => {
  test("shouldSkipSceneSwitch is true only when already on the target", () => {
    // Never issue a same-scene switch — it runs a pointless studio-mode
    // transition that then drops the following switch's event.
    expect(shouldSkipSceneSwitch("sp-slow", "sp-slow")).toBe(true);
    expect(shouldSkipSceneSwitch("sp-slow", "sp-fast")).toBe(false);
  });

  test("sceneSwitchSettled requires BOTH target reached AND transition ended", () => {
    // Program reached the target but the fade is still running → NOT settled
    // (the round-2 name-only bug). This is the discriminating case.
    expect(sceneSwitchSettled("sp-fast", "sp-fast", true)).toBe(false);
    // Program reached the target and no transition is running → settled.
    expect(sceneSwitchSettled("sp-fast", "sp-fast", false)).toBe(true);
    // Still on the old scene → NOT settled regardless of the transition flag.
    expect(sceneSwitchSettled("sp-slow", "sp-fast", false)).toBe(false);
  });

  test("waitForSceneSwitchApplied waits out the transition, not just the name", async () => {
    // Studio-mode fade: GetCurrentProgramScene reports the target from the 2nd
    // read on (mid-fade), but the transition-active flag stays set until the
    // fade ends a few polls later.
    let calls = 0;
    let transitionActive = true;
    const getScene = async () => {
      calls++;
      if (calls >= 4) transitionActive = false;
      return calls >= 2 ? "sp-fast" : "sp-slow";
    };

    await waitForSceneSwitchApplied(getScene, () => transitionActive, "sp-fast", {
      timeoutMs: 2000,
      pollMs: 1,
    });

    // Must have polled until the transition ended (>=4), not returned when the
    // program name first matched (call 2).
    expect(calls).toBeGreaterThanOrEqual(4);
  });

  test("waitForSceneSwitchApplied throws loudly when the switch never applies", async () => {
    // The dropped-switch hole: the program never becomes the target.
    await expect(
      waitForSceneSwitchApplied(
        async () => "sp-slow",
        () => false,
        "sp-fast",
        { timeoutMs: 40, pollMs: 5 },
      ),
    ).rejects.toThrow(/sp-fast/);
  });
});

test.describe("OBS studio-mode preview race (round 4, 19.9.2026)", () => {
  test("waitForPreviewApplied returns only once OBS reports the target preview", async () => {
    // SetCurrentPreviewScene is applied asynchronously: the first reads still
    // show the OLD preview. Triggering then would fade program -> itself.
    let calls = 0;
    const getPreview = async () => {
      calls++;
      return calls >= 3 ? "sp-fast" : "sp-alex";
    };
    await waitForPreviewApplied(getPreview, "sp-fast", { timeoutMs: 2000, pollMs: 1 });
    expect(calls).toBe(3);
  });

  test("waitForPreviewApplied refuses to trigger against a stale preview", async () => {
    await expect(
      waitForPreviewApplied(async () => "sp-alex", "sp-fast", { timeoutMs: 40, pollMs: 5 }),
    ).rejects.toThrow(/stale preview/);
  });
});
