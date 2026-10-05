/**
 * Unit tests for the pure dark gate logic (#127), on `SP-program` (#221 B4
 * step 6: a playlist's own NDI output has no consumer any more, so the
 * receiver that must exist is `SP-program`'s).
 *
 * Runs in the ubuntu mock suite (playwright.config.ts) — no browser and no
 * deployed box needed; these `test()` blocks never touch the `page` fixture.
 * They pin the decision the post-deploy suite relies on: a program with a
 * source and zero receivers fails, one with a live receiver passes.
 */

import { test, expect } from "@playwright/test";
import { classifyConnections, programReceiverVerdict } from "./ndi-health-gate";

const program = (
  source: number | null,
  connections: number,
  degraded_reason: string | null = null,
) => ({ source, health: { connections }, degraded_reason });

test.describe("SP-program dark gate logic (#127, #221)", () => {
  test("classifyConnections distinguishes live / dark / never-polled", () => {
    expect(classifyConnections(2)).toBe("live");
    expect(classifyConnections(1)).toBe("live");
    expect(classifyConnections(0)).toBe("dark");
    expect(classifyConnections(-1)).toBe("never_polled");
  });

  test("passes when a source is on program and SP-program has a live receiver", () => {
    const verdict = programReceiverVerdict(program(7, 2));
    expect(verdict).toEqual({
      ok: true,
      health: "live",
      source: 7,
      connections: 2,
      degraded_reason: null,
    });
    expect(programReceiverVerdict(program(-1, 1)).ok, "OBS manuál on program").toBe(true);
  });

  test("fails a program with zero receivers (the #127 dark output)", () => {
    const verdict = programReceiverVerdict(
      program(7, 0, "no NDI receiver on SP-program"),
    );
    expect(verdict.ok).toBe(false);
    expect(verdict.health).toBe("dark");
    expect(verdict.degraded_reason).toBe("no NDI receiver on SP-program");
    // Before the sender's first poll the server names no reason, and the
    // count's 0 still fails: the post-deploy gate keeps polling.
    const unpolled = programReceiverVerdict(program(7, 0));
    expect(unpolled.degraded_reason).toBeNull();
    expect(unpolled.ok).toBe(false);
  });

  test("fails a program not polled yet, or with nothing on it", () => {
    expect(programReceiverVerdict(program(7, -1)).health).toBe("never_polled");
    expect(programReceiverVerdict(program(7, -1)).ok).toBe(false);
    const empty = programReceiverVerdict(program(null, 3));
    expect(empty.health).toBe("nothing_on_program");
    expect(empty.ok).toBe(false);
  });

  test("fails when the server names a degraded reason, whatever the count", () => {
    expect(programReceiverVerdict(program(7, 2, "no NDI receiver on SP-program")).ok).toBe(
      false,
    );
  });

  test("an absent degraded_reason reads as none", () => {
    const verdict = programReceiverVerdict({ source: 4, health: { connections: 1 } });
    expect(verdict.ok).toBe(true);
    expect(verdict.degraded_reason).toBeNull();
  });
});
