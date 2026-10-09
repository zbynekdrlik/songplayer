/**
 * Unit tests for the `SP-program-MAX` post-deploy gate's decision (#223 S2,
 * `max-gate.ts`). Runs in the ubuntu mock suite (playwright.config.ts): no
 * browser and no box; these `test()` blocks never touch the `page` fixture.
 */

import { test, expect } from "@playwright/test";
import {
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
    vblank_period_ns: 16_666_700,
    vblank_phase_us: 8_000,
    send_off_grid: 40,
    send_phase_us_p50: 8_003,
    send_phase_us_p99: 8_090,
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
});
