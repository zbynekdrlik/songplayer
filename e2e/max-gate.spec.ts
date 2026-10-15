/**
 * Unit tests for the `SP-program-MAX` post-deploy gate's decision (#223 S2,
 * `max-gate.ts`). Runs in the ubuntu mock suite (playwright.config.ts): no
 * browser and no box; these `test()` blocks never touch the `page` fixture.
 */

import { test, expect } from "@playwright/test";
import {
  COMPOSE_PHASE_SLACK_US,
  FhdStatus,
  MAX_SLOT_REPICKS,
  MIN_BOUNDARIES,
  MaxStatus,
  SEND_PHASE_SLACK_US,
  fhdGateFailures,
  maxGateFailures,
} from "./max-gate";

/** #239: the SP-program Spout sender running next to MAX, listed at 1080p. */
function fhd(submitted: number): FhdStatus {
  return {
    enabled: true,
    state: "running",
    reason: null,
    spout_name: "SP-program",
    listed_width: 1920,
    listed_height: 1080,
    submitted,
    failed: 1,
    sender_backoffs: 1,
    draw_us_p99: 300,
    send_us_p99: 500,
  };
}

/** A box where MAX runs: the RTX, 3840×2160, `running`. */
function running(submitted: number): MaxStatus {
  return {
    enabled: true,
    state: "running",
    width: 3840,
    height: 2160,
    submitted,
    coalesced: 0,
    failed: 2,
    upload_us_p99: 900,
    draw_us_p99: 400,
    send_us_p99: 700,
    send_at_us_p50: 12_200,
    send_at_us_p99: 12_600,
    send_at_us_max: 19_000,
    send_late: 3,
    vblank_output: "\\\\.\\DISPLAY2 7680x1080",
    vblank_tracking: true,
    vblank_state: "ticking",
    vblank_period_ns: 16_666_700,
    vblank_phase_us: 8_000,
    compose_phase_us: 13_000,
    send_off_grid: 40,
    send_phase_us_p50: 8_003,
    send_phase_us_p99: 8_090,
    compose_phase_us_p50: 13_004,
    compose_phase_us_p99: 13_200,
    compose_late: 5,
    slot_repicks: 2,
    device_resets: 1,
    sender_backoffs: 0,
    spout_name: "SP-program-MAX",
    adapter: "NVIDIA GeForce RTX 3070 Ti",
    fhd: fhd(submitted),
  };
}

test.describe("SP-program-MAX post-deploy gate (#223 S2)", () => {
  test("a running MAX with one grid second sent passes", () => {
    expect(MIN_BOUNDARIES).toBe(30);
    expect(maxGateFailures(running(100), running(130))).toEqual([]);
    expect(maxGateFailures(running(100), running(200))).toEqual([]);
  });

  test("fewer than one grid second of boundaries fails", () => {
    expect(maxGateFailures(running(100), running(129))).toEqual([
      "only 29 boundaries went out (at least 30)",
    ]);
  });

  test("a coalesced or failed boundary or a lost device between the reads fails", () => {
    const later = { ...running(160), coalesced: 4, failed: 3, device_resets: 3 };
    expect(maxGateFailures(running(100), later)).toEqual([
      "4 boundaries coalesced (MAX fell behind the program)",
      "1 boundaries failed",
      "the device was lost 2 times",
    ]);
  });

  test("counts from before the first read do not fail it", () => {
    const before = { ...running(100), coalesced: 9, failed: 5, device_resets: 2 };
    const after = { ...running(130), coalesced: 9, failed: 5, device_resets: 2 };
    expect(maxGateFailures(before, after)).toEqual([]);
  });

  test("off, another state, another canvas or name fails", () => {
    const broken = {
      ...running(160),
      enabled: false,
      state: "error: no GPU adapter",
      width: 1920,
      height: 1080,
      spout_name: "SP-program-MAX_1",
    };
    expect(maxGateFailures(running(100), broken)).toEqual([
      "program_max_enabled is off",
      "the canvas is 1920x1080, not 3840x2160",
      "the Spout name is SP-program-MAX_1",
      "the state is error: no GPU adapter",
    ]);
  });

  test("no adapter, or the Basic Render Driver, fails", () => {
    expect(maxGateFailures(running(100), { ...running(160), adapter: null })).toEqual([
      "no compositor was built (no adapter)",
    ]);
    const warp = { ...running(160), adapter: "Microsoft Basic Render Driver" };
    expect(maxGateFailures(running(100), warp)).toEqual([
      "it composes on Microsoft Basic Render Driver, not a hardware GPU",
    ]);
  });

  test("the median send must start at the phase setting after the vblank (#223 follow-up)", () => {
    expect(SEND_PHASE_SLACK_US).toBe(1_500);
    const at = (send_phase_us_p50: number, vblank_phase_us = 8_000) => ({
      ...running(160),
      send_phase_us_p50,
      vblank_phase_us,
    });
    expect(maxGateFailures(running(100), at(8_000))).toEqual([]);
    expect(maxGateFailures(running(100), at(9_500))).toEqual([]);
    expect(maxGateFailures(running(100), at(5_200, 5_000))).toEqual([]);
    expect(maxGateFailures(running(100), at(7_999))).toEqual([
      "the Spout sends start 7999 us after the vblank (median), not at the 8000 us phase",
    ]);
    expect(maxGateFailures(running(100), at(9_501))).toEqual([
      "the Spout sends start 9501 us after the vblank (median), not at the 8000 us phase",
    ]);
  });

  test("a phase of a refresh or more is read modulo the refresh (#243)", () => {
    // SNV 13.10.2026: the setting 19 ms on a 16 666.728 us refresh puts each
    // send 2333 us after a vblank (the pacer's slot is vblank + phase): the
    // gate read the median against 19 000 and failed a box on the grid.
    const at = (send_phase_us_p50: number) => ({
      ...running(160),
      vblank_period_ns: 16_666_728,
      vblank_phase_us: 19_000,
      send_phase_us_p50,
    });
    expect(maxGateFailures(running(100), at(2_333))).toEqual([]);
    expect(maxGateFailures(running(100), at(3_833))).toEqual([]);
    expect(maxGateFailures(running(100), at(2_332))).toEqual([
      "the Spout sends start 2332 us after the vblank (median), not at the 19000 us phase (2333 us into the refresh)",
    ]);
    expect(maxGateFailures(running(100), at(3_834))).toEqual([
      "the Spout sends start 3834 us after the vblank (median), not at the 19000 us phase (2333 us into the refresh)",
    ]);
  });

  test("the median compose must start at the compose phase while the grid ticks (#251)", () => {
    expect(COMPOSE_PHASE_SLACK_US).toBe(1_500);
    const at = (compose_phase_us_p50: number, compose_phase_us = 13_000) => ({
      ...running(160),
      compose_phase_us_p50,
      compose_phase_us,
    });
    expect(maxGateFailures(running(100), at(13_000))).toEqual([]);
    expect(maxGateFailures(running(100), at(14_500))).toEqual([]);
    expect(maxGateFailures(running(100), at(11_200, 11_000))).toEqual([]);
    expect(maxGateFailures(running(100), at(12_999))).toEqual([
      "the MAX composes start 12999 us after the vblank (median), not at the 13000 us compose phase",
    ]);
    expect(maxGateFailures(running(100), at(14_501))).toEqual([
      "the MAX composes start 14501 us after the vblank (median), not at the 13000 us compose phase",
    ]);
    // Composing at the offer again (SNV, 15.10.2026: the worst minutes had
    // the compose start 6.7-8.9 ms after the vblank) fails the gate.
    expect(maxGateFailures(running(100), at(7_800))).toEqual([
      "the MAX composes start 7800 us after the vblank (median), not at the 13000 us compose phase",
    ]);
  });

  test("a compose phase of a refresh or more is read modulo the refresh (#251)", () => {
    // 29 ms on a 16 666.728 us refresh is 12 333 us into it.
    const at = (compose_phase_us_p50: number) => ({
      ...running(160),
      vblank_period_ns: 16_666_728,
      compose_phase_us: 29_000,
      compose_phase_us_p50,
    });
    expect(maxGateFailures(running(100), at(12_333))).toEqual([]);
    expect(maxGateFailures(running(100), at(13_833))).toEqual([]);
    expect(maxGateFailures(running(100), at(12_332))).toEqual([
      "the MAX composes start 12332 us after the vblank (median), not at the 29000 us compose phase (12333 us into the refresh)",
    ]);
  });

  test("the compose phase is not judged while the grid does not tick (#251)", () => {
    for (const vblank_state of ["measuring", "not_ticking", null]) {
      const off = { ...running(160), vblank_state, compose_phase_us_p50: 2_000 };
      expect(maxGateFailures(running(100), off).join("\n")).not.toContain("compose");
    }
  });

  test("every boundary between the reads must go on the display's refresh grid", () => {
    const off = { ...running(160), send_off_grid: 43 };
    expect(maxGateFailures(running(100), off)).toEqual([
      "3 boundaries were sent off the display's refresh grid",
    ]);
    const lost = { ...running(160), vblank_tracking: false };
    expect(maxGateFailures(running(100), lost)).toEqual([
      "0 boundaries were sent off the display's refresh grid",
    ]);
    const none = { ...running(160), vblank_output: null };
    expect(maxGateFailures(running(100), none)).toEqual([
      "no display output paces the sends (no vblank tracker)",
    ]);
  });

  test("an output that does not tick is named (#243)", () => {
    const dark = {
      ...running(160),
      send_off_grid: 43,
      vblank_tracking: false,
      vblank_state: "not_ticking",
    };
    expect(maxGateFailures(running(100), dark)).toEqual([
      "3 boundaries were sent off the display's refresh grid",
      "the display output \\\\.\\DISPLAY2 7680x1080 does not tick (vblank_state not_ticking): WaitForVBlank does not wait",
    ]);
  });

  test("at most one new slot pick between the reads", () => {
    expect(MAX_SLOT_REPICKS).toBe(1);
    expect(maxGateFailures(running(100), { ...running(160), slot_repicks: 3 })).toEqual([]);
    expect(maxGateFailures(running(100), { ...running(160), slot_repicks: 4 })).toEqual([
      "the send slot was picked anew 2 times between the reads",
    ]);
  });
});

test.describe("SP-program (1920x1080) Spout gate (#239)", () => {
  test("a running SP-program sender with one grid second sent passes", () => {
    expect(fhdGateFailures(fhd(100), fhd(130))).toEqual([]);
    expect(fhdGateFailures(fhd(100), fhd(300))).toEqual([]);
  });

  test("fewer than one grid second of SP-program boundaries fails", () => {
    expect(fhdGateFailures(fhd(100), fhd(129))).toEqual([
      "only 29 SP-program boundaries went out (at least 30)",
    ]);
  });

  test("a failed boundary or a refused sender between the reads fails", () => {
    const later = { ...fhd(160), failed: 4, sender_backoffs: 3 };
    expect(fhdGateFailures(fhd(100), later)).toEqual([
      "3 SP-program boundaries failed",
      "the SP-program sender was refused 2 times",
    ]);
  });

  test("counts from before the first read do not fail it", () => {
    const before = { ...fhd(100), failed: 9, sender_backoffs: 5 };
    const after = { ...fhd(130), failed: 9, sender_backoffs: 5 };
    expect(fhdGateFailures(before, after)).toEqual([]);
  });

  test("off, another state, another name or listing fails", () => {
    const broken = {
      ...fhd(160),
      enabled: false,
      reason: "setting_off",
      state: "off",
      spout_name: "SP-program_1",
      listed_width: 3840,
      listed_height: 2160,
    };
    expect(fhdGateFailures(fhd(100), broken)).toEqual([
      "program_spout_fhd_enabled is off",
      "the SP-program sender is off: setting_off",
      "the FHD Spout name is SP-program_1",
      "the SP-program sender is off",
      "Spout lists SP-program at 3840x2160, not 1920x1080",
    ]);
  });

  test("MAX off turns the SP-program sender off, and the gate names why", () => {
    const off = { ...fhd(160), reason: "max_off", state: "off" };
    expect(fhdGateFailures(fhd(100), off)).toEqual([
      "the SP-program sender is off: max_off",
      "the SP-program sender is off",
    ]);
  });

  test("a sender Spout has not listed yet fails", () => {
    const unlisted = { ...fhd(160), listed_width: 0, listed_height: 0 };
    expect(fhdGateFailures(fhd(100), unlisted)).toEqual([
      "Spout lists SP-program at 0x0, not 1920x1080",
    ]);
  });

  test("#251 step 2: the mock serves max.fhd with the server's fields, no upload", async ({
    request,
  }) => {
    // The server's `FhdStatus` names (program_max_fhd_tests.rs pins them).
    const program = await (await request.get("/api/v1/program")).json();
    expect(Object.keys(program.max.fhd).sort()).toEqual([
      "draw_us_p99",
      "enabled",
      "failed",
      "listed_height",
      "listed_width",
      "reason",
      "send_us_p99",
      "sender_backoffs",
      "spout_name",
      "spout_relists",
      "state",
      "submitted",
    ]);
  });
});
