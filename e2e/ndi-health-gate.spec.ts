/**
 * Unit tests for the pure NDI dark-wall gate logic (#127).
 *
 * Runs in the ubuntu mock suite (playwright.config.ts) — no browser and no
 * deployed box needed; these `test()` blocks never touch the `page` fixture.
 * They pin the decision the post-deploy suite relies on: a Playing-on-program
 * output with zero receivers must be flagged, while an off-program dark output
 * must not.
 */

import { test, expect } from "@playwright/test";
import {
  classifyConnections,
  unhealthyOnProgramOutputs,
} from "./ndi-health-gate";

test.describe("NDI dark-wall gate logic (#127)", () => {
  test("classifyConnections distinguishes live / dark / never-polled", () => {
    expect(classifyConnections(2)).toBe("live");
    expect(classifyConnections(1)).toBe("live");
    expect(classifyConnections(0)).toBe("dark");
    expect(classifyConnections(-1)).toBe("never_polled");
  });

  test("flags an on-program output with zero receivers (the #127 dark wall)", () => {
    const health = [
      { playlist_id: 4, ndi_name: "SP-slow", connections: 0 },
      { playlist_id: 7, ndi_name: "SP-fast", connections: 2 },
    ];
    const bad = unhealthyOnProgramOutputs([4], health);
    expect(bad).toHaveLength(1);
    expect(bad[0].ndi_name).toBe("SP-slow");
    expect(bad[0].health).toBe("dark");
  });

  test("passes when the on-program output has a live receiver", () => {
    const health = [
      { playlist_id: 4, ndi_name: "SP-slow", connections: 0 },
      { playlist_id: 7, ndi_name: "SP-fast", connections: 2 },
    ];
    expect(unhealthyOnProgramOutputs([7], health)).toHaveLength(0);
  });

  test("does not flag an off-program output that is dark (normal idle)", () => {
    const health = [
      { playlist_id: 4, ndi_name: "SP-slow", connections: 0 }, // off program, dark
      { playlist_id: 7, ndi_name: "SP-fast", connections: 2 }, // on program, live
    ];
    // Only playlist 7 is on program (and live). Playlist 4 is dark but NOT on
    // program → connections=0 is normal there; the gate must ignore it.
    expect(unhealthyOnProgramOutputs([7], health)).toHaveLength(0);
  });

  test("flags an on-program output that has never been polled (connections -1)", () => {
    const health = [{ playlist_id: 4, ndi_name: "SP-slow", connections: -1 }];
    const bad = unhealthyOnProgramOutputs([4], health);
    expect(bad).toHaveLength(1);
    expect(bad[0].health).toBe("never_polled");
  });

  test("flags an on-program playlist that has no health snapshot at all", () => {
    const bad = unhealthyOnProgramOutputs([9], []);
    expect(bad).toHaveLength(1);
    expect(bad[0].health).toBe("never_polled");
  });
});
