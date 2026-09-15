/**
 * Unit tests for the deterministic OBS scene-switch wait (#170).
 *
 * Runs in the ubuntu mock suite (playwright.config.ts) — no browser and no
 * deployed box; these `test()` blocks never touch the `page` fixture. They
 * pin the contract the post-deploy `ObsDriver.switchScene` relies on: OBS on
 * win-resolume runs Studio Mode with a 2000ms Fade, so `GetCurrentProgramScene`
 * reports the target only after the fade completes — the wait must keep
 * polling until it applies, and fail loudly if it never does.
 */

import { test, expect } from "@playwright/test";
import { waitForProgramScene } from "./obs-scene-wait";

test.describe("waitForProgramScene — deterministic OBS scene-switch wait (#170)", () => {
  test("waits until the program scene actually applies (studio-mode fade)", async () => {
    // OBS reports the OLD scene until the fade completes, then the target.
    const reads = ["sp-fast", "sp-fast", "sp-slow"];
    let calls = 0;
    const getScene = async () => reads[Math.min(calls++, reads.length - 1)];

    await waitForProgramScene(getScene, "sp-slow", { timeoutMs: 2000, pollMs: 1 });

    // Must have polled past the stale reads, not returned after the first.
    expect(calls).toBeGreaterThanOrEqual(3);
  });

  test("throws loudly when the switch never applies within the timeout", async () => {
    const getScene = async () => "sp-fast"; // never becomes the target

    await expect(
      waitForProgramScene(getScene, "sp-slow", { timeoutMs: 40, pollMs: 5 }),
    ).rejects.toThrow(/sp-slow/);
  });

  test("returns immediately when already on the target scene (no-op switch)", async () => {
    let calls = 0;
    const getScene = async () => {
      calls++;
      return "sp-slow";
    };

    await waitForProgramScene(getScene, "sp-slow", { timeoutMs: 2000, pollMs: 50 });

    expect(calls).toBe(1);
  });
});
