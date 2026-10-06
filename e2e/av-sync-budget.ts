/**
 * The post-deploy A/V gate's time budget (#147, #221) — the arithmetic the
 * gate (`post-deploy-av-sync.spec.ts`) runs on, pinned by the mock-suite
 * `av-sync-budget.spec.ts` so it cannot drift unnoticed (review round 1 of
 * the dev.19 lock wait).
 *
 * A take is repeated only while `RETAKE_BEFORE_MS` of the test's budget has
 * not been used: a full worst-case take then still fits, with
 * `UNCOUNTED_CALLS_MS` for the calls the sum does not count (the audio
 * wait's two `Reidentify` round trips, the StartRecord pre-check, the
 * `/mix` and `/videos` reads, spawning the analysis). Each wait that joins
 * a take adds its worst case to `WORST_TAKE_MS` and to `TEST_TIMEOUT_MS`, so
 * the retake room stays 110 s (#221 dev.18: the audio wait, 300 → 320 s;
 * dev.19: the lock wait, 320 → 345 s).
 */

import { EVIDENCE_COPY_MS } from "./av-sync-evidence";
import { AUDIO_WAIT_TIMEOUT_MS } from "./obs-audio-wait";
import { LOCK_WAIT_WORST_MS } from "./probe-lock-wait";

/** The recording's length. */
export const RECORD_MS = 20_000;

/** The analysis (`av_sync_check.py`), ~5-10 s on the box. */
export const ANALYSIS_TIMEOUT_MS = 60_000;

/** Takes at most. */
export const MAX_TAKES = 3;

/** The wait for the playlist to move off a song after a `/skip`. */
export const SKIP_WAIT_MS = 15_000;

/** The wait for an on-program playlist to be Playing with frames. */
export const PLAY_WAIT_MS = 30_000;

/** `StopRecord` until OBS reports the output inactive. */
export const STOP_RECORD_MS = 10_000;

/** The wait for a recording's auto-remux sibling (`<base>.mp4`). */
export const REMUX_SIBLING_WAIT_MS = 15_000;

/** The retries of one file OBS still holds (Windows EBUSY/EPERM). */
export const BUSY_RETRY_MS = 10_000;

/** Deleting one take's recording: the wait for its remux sibling, then the
 *  busy-file retries of each of its two files. */
export const CLEANUP_MS = REMUX_SIBLING_WAIT_MS + 2 * BUSY_RETRY_MS;

/** The calls one take makes that the sum below does not count. */
export const UNCOUNTED_CALLS_MS = 10_000;

/** The test's own timeout. */
export const TEST_TIMEOUT_MS = 345_000;

/** One take's worst case: skip + play + the probe lock wait + the probe
 *  audio wait + record + stop + analysis + cleanup + two evidence copies. */
export const WORST_TAKE_MS =
  SKIP_WAIT_MS +
  PLAY_WAIT_MS +
  LOCK_WAIT_WORST_MS +
  AUDIO_WAIT_TIMEOUT_MS +
  RECORD_MS +
  STOP_RECORD_MS +
  ANALYSIS_TIMEOUT_MS +
  CLEANUP_MS +
  2 * EVIDENCE_COPY_MS;

/** A retake starts only while less than this much of the budget is used. */
export const RETAKE_BEFORE_MS = TEST_TIMEOUT_MS - WORST_TAKE_MS - UNCOUNTED_CALLS_MS;
