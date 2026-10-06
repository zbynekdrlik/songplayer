/**
 * Unit tests for the A/V gate's time budget (#147, #221). Runs in the ubuntu
 * mock suite (playwright.config.ts); no browser, no box.
 *
 * Review round 1 of the dev.19 lock wait: nothing failed when the budget
 * drifted (`TEST_TIMEOUT_MS` back at 320 s would quietly cut the retake room
 * from 110 s to 85 s). The arithmetic now lives in `av-sync-budget.ts`, and
 * these tests pin it.
 */

import { test, expect } from "@playwright/test";
import {
  ANALYSIS_TIMEOUT_MS,
  CLEANUP_MS,
  MAX_TAKES,
  PLAY_WAIT_MS,
  RECORD_MS,
  RETAKE_BEFORE_MS,
  SKIP_WAIT_MS,
  STOP_RECORD_MS,
  TEST_TIMEOUT_MS,
  UNCOUNTED_CALLS_MS,
  WORST_TAKE_MS,
} from "./av-sync-budget";
import { AUDIO_WAIT_TIMEOUT_MS } from "./obs-audio-wait";
import { LOCK_WAIT_WORST_MS } from "./probe-lock-wait";
import { EVIDENCE_COPY_MS } from "./av-sync-evidence";

test.describe("A/V gate time budget (#147, #221)", () => {
  test("one take's worst case is the sum of its own bounds: 225 s", () => {
    expect(WORST_TAKE_MS).toBe(
      SKIP_WAIT_MS +
        PLAY_WAIT_MS +
        LOCK_WAIT_WORST_MS +
        AUDIO_WAIT_TIMEOUT_MS +
        RECORD_MS +
        STOP_RECORD_MS +
        ANALYSIS_TIMEOUT_MS +
        CLEANUP_MS +
        2 * EVIDENCE_COPY_MS,
    );
    expect(WORST_TAKE_MS).toBe(225_000);
    // The parts the take's own code waits on.
    expect([SKIP_WAIT_MS, PLAY_WAIT_MS, RECORD_MS, STOP_RECORD_MS]).toEqual([
      15_000, 30_000, 20_000, 10_000,
    ]);
    expect([ANALYSIS_TIMEOUT_MS, CLEANUP_MS]).toEqual([60_000, 35_000]);
    expect(LOCK_WAIT_WORST_MS).toBe(25_000);
    expect(AUDIO_WAIT_TIMEOUT_MS).toBe(20_000);
  });

  test("a retake keeps its 110 s of room: 345 s − 225 s − 10 s", () => {
    expect(TEST_TIMEOUT_MS).toBe(345_000);
    expect(UNCOUNTED_CALLS_MS).toBe(10_000);
    expect(RETAKE_BEFORE_MS).toBe(TEST_TIMEOUT_MS - WORST_TAKE_MS - UNCOUNTED_CALLS_MS);
    expect(RETAKE_BEFORE_MS).toBe(110_000);
    // A retake started at the limit still fits a whole worst-case take.
    expect(RETAKE_BEFORE_MS + WORST_TAKE_MS).toBeLessThan(TEST_TIMEOUT_MS);
    expect(MAX_TAKES).toBe(3);
  });
});
