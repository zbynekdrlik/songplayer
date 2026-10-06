/**
 * Unit tests for the A/V gate's wait for the probe's AUDIO (#221, dev.18).
 * Runs in the ubuntu mock suite (playwright.config.ts); these `test()` blocks
 * never touch `page`, so they need no browser and no box.
 *
 * The gate's first take on dev.17 (run 37423917199) opened with two audio
 * dropouts: a freshly attached DistroAV receiver delivers its picture first
 * and its audio only once camera-box's genlock audio pairing locks (~5 s), and
 * StartRecord came ~4 s after the attach. The gate now waits until the probe
 * input's obs-websocket `InputVolumeMeters` reading is non-silent for 1 s.
 */

import { test, expect } from "@playwright/test";
import {
  AUDIO_HOLD_MS,
  AUDIO_WAIT_TIMEOUT_MS,
  MAX_METER_GAP_MS,
  NO_STREAK,
  SILENCE_FLOOR_DBFS,
  audioFlowing,
  explainAudioWait,
  inputPeakMul,
  meterInputs,
  nextAudioStreak,
  waitForInputAudio,
  type AudioStreak,
  type AudioWaitReport,
  type MeterInput,
} from "./obs-audio-wait";

const PROBE = "A/V gate SP-program";

/** One meter reading of the probe: every channel at `peak` (linear), before
 *  and after its fader alike. */
function probeAt(peak: number, channels = 2): MeterInput {
  return {
    inputName: PROBE,
    inputLevelsMul: Array.from({ length: channels }, () => [peak * 0.7, peak, peak]),
  };
}

/** A meter source the test drives by hand: `push` delivers one event,
 *  `close` reports the connection closed. */
function fakeMeters() {
  let listener: ((inputs: MeterInput[]) => void) | null = null;
  let onClosed: ((reason: string) => void) | null = null;
  let unsubscribes = 0;
  return {
    subscribe(l: (inputs: MeterInput[]) => void, c: (reason: string) => void): () => void {
      listener = l;
      onClosed = c;
      return () => {
        listener = null;
        onClosed = null;
        unsubscribes++;
      };
    },
    push(inputs: MeterInput[]): void {
      listener?.(inputs);
    },
    close(reason: string): void {
      onClosed?.(reason);
    },
    get listening(): boolean {
      return listener !== null;
    },
    get unsubscribes(): number {
      return unsubscribes;
    },
  };
}

/** Feed `streak` one reading per 50 ms from `from` to `to` (inclusive). */
function feed(
  streak: AudioStreak,
  from: number,
  to: number,
  peak: number | null,
): AudioStreak {
  for (let t = from; t <= to; t += 50) streak = nextAudioStreak(streak, peak, t);
  return streak;
}

test.describe("A/V gate: wait for the probe's audio (#221 dev.18)", () => {
  test("the defaults: -60 dBFS floor, 1 s of audio, 20 s bound, 500 ms event gap", () => {
    expect(SILENCE_FLOOR_DBFS).toBe(-60);
    expect(AUDIO_HOLD_MS).toBe(1_000);
    expect(AUDIO_WAIT_TIMEOUT_MS).toBe(20_000);
    expect(MAX_METER_GAP_MS).toBe(500);
  });

  test("meterInputs keeps the well-formed inputs of an InputVolumeMeters event", () => {
    expect(
      meterInputs([
        { inputName: PROBE, inputUuid: "u1", inputLevelsMul: [[0.1, 0.2, 0.3]] },
        { inputName: "mic", inputLevelsMul: [] },
        { inputName: 7, inputLevelsMul: [[1, 1, 1]] }, // no name: dropped
        { inputName: "cam", inputLevelsMul: "loud" }, // no level list: no channels
        null,
      ]),
    ).toEqual([
      { inputName: PROBE, inputLevelsMul: [[0.1, 0.2, 0.3]] },
      { inputName: "mic", inputLevelsMul: [] },
      { inputName: "cam", inputLevelsMul: [] },
    ]);
    expect(meterInputs(undefined)).toEqual([]);
    expect(meterInputs({ inputs: [] })).toEqual([]);
  });

  test("the probe's reading is its loudest channel's INPUT peak, before its fader", () => {
    const inputs: MeterInput[] = [
      { inputName: "other", inputLevelsMul: [[0.9, 0.9, 0.9]] },
      { inputName: PROBE, inputLevelsMul: [[0.01, 0.02, 0.02], [0.2, 0.3, 0.5]] },
    ];
    expect(inputPeakMul(inputs, PROBE)).toBe(0.5);
    // The loudest wherever it is: first, middle or last channel.
    expect(
      inputPeakMul([{ inputName: PROBE, inputLevelsMul: [[0.2, 0.3, 0.5], [0.01, 0.02, 0.02]] }], PROBE),
    ).toBe(0.5);
    expect(
      inputPeakMul(
        [{ inputName: PROBE, inputLevelsMul: [[0, 0, 0.1], [0, 0, 0.4], [0, 0, 0.2]] }],
        PROBE,
      ),
    ).toBe(0.4);
    // Muted or faded down (the first two values are × the fader): the receiver
    // still delivers audio, so it is flowing; the take then fails on its own.
    expect(inputPeakMul([{ inputName: PROBE, inputLevelsMul: [[0, 0, 0.25]] }], PROBE)).toBe(0.25);
    // A loud fader over a silent input is not audio.
    expect(inputPeakMul([{ inputName: PROBE, inputLevelsMul: [[0.5, 0.5, 0]] }], PROBE)).toBe(0);
    // No channels, or a malformed level: silence.
    expect(inputPeakMul([{ inputName: PROBE, inputLevelsMul: [] }], PROBE)).toBe(0);
    expect(inputPeakMul([{ inputName: PROBE, inputLevelsMul: [[0.5, 0.5]] }], PROBE)).toBe(0);
    // Not in the event: OBS meters only ACTIVE inputs.
    expect(inputPeakMul([{ inputName: "other", inputLevelsMul: [[1, 1, 1]] }], PROBE)).toBeNull();
    expect(inputPeakMul([], PROBE)).toBeNull();
  });

  test("silent → flowing: the audio counts once it is above the floor for 1 s", () => {
    let s = feed(NO_STREAK, 0, 4_000, 0); // the receiver's warm-up: no audio
    expect(s.since).toBeNull();
    expect(audioFlowing(s, 4_000)).toBe(false);
    s = nextAudioStreak(s, 0.1, 4_050); // -20 dBFS: the audio starts
    expect(s.since).toBe(4_050);
    s = feed(s, 4_100, 5_000, 0.1);
    expect(audioFlowing(s, 5_000), "950 ms is not 1 s").toBe(false);
    s = nextAudioStreak(s, 0.1, 5_050);
    expect(audioFlowing(s, 5_050)).toBe(true);
  });

  test("the floor itself is silence; just above it is audio", () => {
    const floor = 10 ** (SILENCE_FLOOR_DBFS / 20); // 0.001
    expect(nextAudioStreak(NO_STREAK, floor, 0).since).toBeNull();
    expect(nextAudioStreak(NO_STREAK, floor * 1.01, 0).since).toBe(0);
    // A custom floor.
    expect(nextAudioStreak(NO_STREAK, 0.05, 0, { floorDbfs: -20 }).since).toBeNull();
    expect(nextAudioStreak(NO_STREAK, 0.2, 0, { floorDbfs: -20 }).since).toBe(0);
  });

  test("a silent reading resets the 1 s streak", () => {
    let s = feed(NO_STREAK, 0, 900, 0.1);
    s = nextAudioStreak(s, 0, 950); // a gap in the audio
    expect(s.since).toBeNull();
    s = feed(s, 1_000, 1_950, 0.1);
    expect(audioFlowing(s, 1_950), "the streak restarted at 1 000").toBe(false);
    s = nextAudioStreak(s, 0.1, 2_000);
    expect(audioFlowing(s, 2_000)).toBe(true);
  });

  test("an event without the probe resets the streak (it is not metered)", () => {
    let s = feed(NO_STREAK, 0, 900, 0.1);
    s = nextAudioStreak(s, null, 950);
    expect(s.since).toBeNull();
    expect(audioFlowing(feed(s, 1_000, 1_900, 0.1), 1_900)).toBe(false);
  });

  test("a gap between meter events restarts the streak: only observed audio counts", () => {
    let s = nextAudioStreak(NO_STREAK, 0.1, 0);
    s = nextAudioStreak(s, 0.1, 500); // exactly the allowed gap: continuous
    expect(s.since).toBe(0);
    s = nextAudioStreak(s, 0.1, 1_001); // 501 ms without an event
    expect(s.since).toBe(1_001);
    expect(audioFlowing(s, 1_001)).toBe(false);
    // A custom gap.
    expect(nextAudioStreak({ since: 0, lastAt: 0 }, 0.1, 200, { maxGapMs: 100 }).since).toBe(200);
  });

  test("audioFlowing honours a custom hold", () => {
    const s = feed(NO_STREAK, 0, 300, 0.1);
    expect(audioFlowing(s, 300, 300)).toBe(true);
    expect(audioFlowing(s, 300, 301)).toBe(false);
    expect(audioFlowing(NO_STREAK, 10_000, 0)).toBe(false);
  });

  test("waitForInputAudio resolves 1 s after the audio starts, and unsubscribes", async () => {
    const meters = fakeMeters();
    let t = 0;
    const wait = waitForInputAudio(meters.subscribe, PROBE, { now: () => t });
    // 4 s of picture without audio, then -12 dBFS.
    for (t = 0; t <= 8_000 && meters.listening; t += 50) {
      meters.push([{ inputName: "other", inputLevelsMul: [[1, 1, 1]] }, probeAt(t < 4_000 ? 0 : 0.25)]);
    }
    const report = await wait;
    expect(report.waitedMs).toBe(5_000);
    expect(report.events).toBe(101);
    expect(report.withInput).toBe(101);
    expect(report.longestStreakMs).toBe(1_000);
    expect(report.loudestDbfs).toBeCloseTo(-12.04, 1);
    expect(meters.listening, "unsubscribed once flowing").toBe(false);
    expect(meters.unsubscribes).toBe(1);
  });

  test("waitForInputAudio: a gap in the audio resets the 1 s streak", async () => {
    const meters = fakeMeters();
    let t = 0;
    const wait = waitForInputAudio(meters.subscribe, PROBE, { now: () => t });
    // Audio from 0, a gap at 600 ms (the dev.17 warm-up shape), audio again.
    for (t = 0; t <= 8_000 && meters.listening; t += 50) {
      meters.push([probeAt(t === 600 ? 0 : 0.25)]);
    }
    const report = await wait;
    expect(report.waitedMs, "1 s after the gap, not 1 s after the first audio").toBe(1_650);
  });

  test("waitForInputAudio fails loudly at its bound with the meter state it saw", async () => {
    const meters = fakeMeters();
    let t = 0;
    const wait = waitForInputAudio(meters.subscribe, PROBE, {
      now: () => t,
      timeoutMs: 50,
    });
    meters.push([probeAt(0.25)]);
    t = 300;
    meters.push([probeAt(0.25)]); // a 300 ms run
    t = 350;
    meters.push([probeAt(0)]);
    t = 400;
    meters.push([{ inputName: "other", inputLevelsMul: [[1, 1, 1]] }]);
    t = 450;
    meters.push([probeAt(0.1)]);
    t = 500;
    meters.push([probeAt(0.1)]); // a later, shorter run: 50 ms
    t = 550;
    meters.push([probeAt(0.001 / 2)]);
    const err = await wait.then(
      () => null,
      (e: Error) => e,
    );
    expect(err, "the bound rejects").not.toBeNull();
    const msg = err!.message;
    expect(msg).toContain(`"${PROBE}"`);
    expect(msg).toContain("input peak above -60 dBFS for 1000 ms in a row");
    expect(msg).toContain("within 50 ms");
    expect(msg).toContain("7 InputVolumeMeters events, 6 with the input");
    expect(msg).toContain("last input peaks [-66.0, -66.0] dBFS");
    expect(msg).toContain("loudest -12.0 dBFS");
    expect(msg).toContain("longest run above the floor 300 ms");
    expect(msg, "an active, silent probe: the warm-up explanation").toContain("audio pairing");
    expect(meters.listening, "unsubscribed at the bound").toBe(false);
    expect(meters.unsubscribes).toBe(1);
  });

  test("waitForInputAudio says so when no meter event arrived at all", async () => {
    const meters = fakeMeters();
    const err = await waitForInputAudio(meters.subscribe, PROBE, { timeoutMs: 20 }).then(
      () => null,
      (e: Error) => e,
    );
    expect(err?.message).toContain("no InputVolumeMeters event arrived");
    expect(err?.message).toContain("subscription did not apply");
    expect(meters.listening).toBe(false);
  });

  test("the bound's explanation follows what the meter saw", () => {
    const seen = (events: number, withInput: number): AudioWaitReport => ({
      waitedMs: 20_000,
      events,
      withInput,
      loudestDbfs: -Infinity,
      lastPeaksDbfs: null,
      longestStreakMs: 0,
    });
    expect(explainAudioWait(seen(0, 0))).toContain("subscription did not apply");
    // Events, but never the probe: it is not on the program feed.
    expect(explainAudioWait(seen(400, 0))).toContain("never active");
    expect(explainAudioWait(seen(400, 0))).toContain("probe scene");
    // The probe metered but silent: the receiver's audio never held.
    expect(explainAudioWait(seen(400, 400))).toContain("audio pairing");
    expect(explainAudioWait(seen(400, 1))).not.toContain("never active");
  });

  test("a closed connection ends the wait at once, naming the close", async () => {
    const meters = fakeMeters();
    let t = 0;
    // A bound well inside the test's own timeout: a broken close path fails
    // on the message below, not on a hung test.
    const wait = waitForInputAudio(meters.subscribe, PROBE, { now: () => t, timeoutMs: 5_000 });
    meters.push([probeAt(0.25)]);
    t = 120;
    meters.close("code 1006");
    const err = await wait.then(
      () => null,
      (e: Error) => e,
    );
    expect(err?.message).toContain("the OBS connection closed (code 1006)");
    expect(err?.message).toContain(`"${PROBE}"`);
    expect(err?.message).toContain("after 120 ms");
    expect(err?.message).toContain("1 InputVolumeMeters events, 1 with the input");
    expect(meters.listening).toBe(false);
    expect(meters.unsubscribes).toBe(1);
  });

  test("a source that delivers synchronously still unsubscribes exactly once", async () => {
    let unsubscribes = 0;
    const report = await waitForInputAudio(
      (onMeters) => {
        onMeters([probeAt(0.25)]); // flowing at once with no hold
        return () => {
          unsubscribes++;
        };
      },
      PROBE,
      { holdMs: 0 },
    );
    expect(report.withInput).toBe(1);
    expect(unsubscribes).toBe(1);
  });
});
