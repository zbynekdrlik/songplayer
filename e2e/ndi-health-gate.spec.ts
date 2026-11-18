/**
 * Unit tests for the pure dark gate logic (#127), on `SP-program` (#221 B4
 * step 6 / lane 3: a playlist has no NDI output of its own any more, so the
 * receiver that must exist is `SP-program`'s).
 *
 * Runs in the ubuntu mock suite (playwright.config.ts) — no browser and no
 * deployed box needed; these `test()` blocks never touch the `page` fixture.
 * They pin the decision the post-deploy suite relies on: a program with a
 * source and zero receivers fails, one with a live receiver passes.
 */

import { test, expect } from "@playwright/test";
import {
  EMIT_LATE_EDGES_US,
  classifyConnections,
  emitLateHistFailures,
  discoverableState,
  programReceiverVerdict,
} from "./ndi-health-gate";

const program = (
  source: number | null,
  connections: number,
  degraded_reason: string | null = null,
  discoverable = "discoverable",
) => ({
  source,
  health: { connections },
  degraded_reason,
  ndi: { discoverable: { state: discoverable } },
});

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
      discoverable: "discoverable",
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

  test("fails a program with no valid reading (-1), or with nothing on it", () => {
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
    const verdict = programReceiverVerdict({
      source: 4,
      health: { connections: 1 },
      ndi: { discoverable: { state: "discoverable" } },
    });
    expect(verdict.ok).toBe(true);
    expect(verdict.degraded_reason).toBeNull();
  });

  test("fails while SP-program is not discoverable on the network (#305)", () => {
    const lost = programReceiverVerdict(
      program(7, 2, "SP-program not discoverable on the network", "not_discoverable"),
    );
    expect(lost.ok).toBe(false);
    expect(lost.discoverable).toBe("not_discoverable");
    expect(lost.degraded_reason).toBe("SP-program not discoverable on the network");
    // The receiver count alone never passes it: PP's sender kept its
    // receivers' count while nobody could find it.
    expect(programReceiverVerdict(program(7, 2, null, "not_discoverable")).ok).toBe(false);
    expect(programReceiverVerdict(program(7, 2, null, "starting")).ok, "still in its grace").toBe(
      false,
    );
    expect(programReceiverVerdict(program(7, 2, null, "unavailable")).ok, "no finder").toBe(false);
  });

  test("a server that sends no discoverability fails the gate (#305)", () => {
    const none = programReceiverVerdict({ source: 4, health: { connections: 1 } });
    expect(none.ok).toBe(false);
    expect(none.discoverable).toBe("missing");
    const nulls = programReceiverVerdict({
      source: 4,
      health: { connections: 1 },
      ndi: { discoverable: null },
    });
    expect(nulls.discoverable).toBe("missing");
    expect(programReceiverVerdict({ ...nulls, ndi: null }).discoverable).toBe("missing");
  });

  test("discoverableState reads the state, or missing (#305, PP's gate)", () => {
    expect(discoverableState(program(7, 0, null, "discoverable"))).toBe("discoverable");
    expect(discoverableState(program(null, 0, null, "stale"))).toBe("stale");
    expect(discoverableState(null), "no answer").toBe("missing");
    expect(discoverableState({ source: 1, health: { connections: 1 } })).toBe("missing");
  });
});

test.describe("emit-lateness histogram (#289 step 0)", () => {
  const counts = (n: number) => EMIT_LATE_EDGES_US.map((_, i) => (i === 0 ? n : 0));
  const row = (ndi_name: string, enabled: boolean, hist?: { edges_us: number[]; counts: number[] }) => ({
    ndi_name,
    pacing: { enabled, emit_late_hist: hist },
  });

  test("a paced pipeline with counted emits passes; one not paced is not read", () => {
    expect(EMIT_LATE_EDGES_US.length).toBe(14);
    expect(EMIT_LATE_EDGES_US[13]).toBe(33_333);
    expect(
      emitLateHistFailures([
        row("SP-slow", true, { edges_us: EMIT_LATE_EDGES_US, counts: counts(1800) }),
        row("SP-off", false),
        { ndi_name: "SP-none" },
      ]),
    ).toEqual([]);
  });

  test("each failure is named", () => {
    expect(
      emitLateHistFailures([
        row("SP-a", true),
        row("SP-b", true, { edges_us: [0, 120], counts: [0, 0] }),
        row("SP-c", true, { edges_us: EMIT_LATE_EDGES_US, counts: counts(0) }),
      ]),
    ).toEqual([
      "SP-a: no pacing.emit_late_hist",
      "SP-b: edges_us [0,120]",
      "SP-b: 2 counts, want 14",
      "SP-b: no emit counted",
      "SP-c: no emit counted",
    ]);
  });
});
