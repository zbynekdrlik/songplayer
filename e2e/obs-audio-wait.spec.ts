/**
 * Unit tests for the A/V gate's wait for the probe's AUDIO (#221, dev.18).
 * Runs in the ubuntu mock suite (playwright.config.ts); these `test()` blocks
 * never touch `page`, so they need no browser and no box.
 *
 * The gate's first take on dev.17 (run 37423917199) opened with two audio
 * dropouts. A freshly attached DistroAV receiver delivers its picture at
 * once, and its audio reaches cg OBS's MIX with gaps until camera-box's
 * genlock audio pairing locks (LOCKED 4.1 s after the bind). The take
 * started 2.6 s after the bind. The gate now waits until the probe input's
 * obs-websocket `InputVolumeMeters` reading is non-silent for 1 s.
 *
 * That meter is tapped BEFORE the pairing's withhold: it shows that DistroAV
 * delivers audio, never that the mix gets it (`obs-audio-wait.ts`). So a
 * silent meter is never the pairing's fault.
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
    let s = feed(NO_STREAK, 0, 4_000, 0); // DistroAV delivers no audio yet
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
    // 4 s in which DistroAV delivers no audio yet, then -12 dBFS.
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
    // Audio from 0, one silent reading at 600 ms, audio again.
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
    // The probe WAS loud (readings up to -12 dBFS) but never held 1 s, and
    // its last run had ended before the bound: the explanation must say so,
    // never "no audio" next to readings that show audio (review round 3).
    expect(msg).not.toContain("started late");
    expect(msg).toContain("2 runs above it");
    expect(msg).toContain("rose above the floor (loudest -12.0 dBFS) but never held 1000 ms");
    expect(msg).not.toContain("delivers no audio");
    expect(msg, "the pairing never silences the meter").not.toMatch(/pairing/i);
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
    const seen = (
      events: number,
      withInput: number,
      loudestDbfs = -Infinity,
      runsAboveFloor = loudestDbfs > -60 ? 2 : 0,
    ): AudioWaitReport => ({
      waitedMs: 20_000,
      events,
      withInput,
      loudestDbfs,
      lastPeaksDbfs: null,
      longestStreakMs: 0,
      runsAboveFloor,
      readingsAtOrBelowFloor: 0,
      openRun: null,
    });
    expect(explainAudioWait(seen(0, 0))).toContain("subscription did not apply");
    // Events, but never the probe: it is not on the program feed.
    expect(explainAudioWait(seen(400, 0))).toContain("never active");
    expect(explainAudioWait(seen(400, 0))).toContain("probe scene");
    expect(explainAudioWait(seen(400, 1))).not.toContain("never active");
    // The probe metered, never above the floor: DistroAV delivers it no audio.
    // camera-box's pairing withholds packets from the MIX, after the meter's
    // tap, so it is never the cause of a silent meter (review round 2).
    expect(explainAudioWait(seen(400, 400))).toContain("DistroAV delivers no audio");
    expect(explainAudioWait(seen(400, 400))).not.toMatch(/pairing/i);
    // The floor itself is silence too.
    expect(explainAudioWait(seen(400, 400, -60))).toContain("DistroAV delivers no audio");
    // Above the floor but never held: audio with gaps, or the events stopped —
    // never "no audio" (review round 3). The hold and the floor are the wait's.
    const loud = explainAudioWait(seen(400, 400, -12.04));
    expect(loud).toContain("rose above the floor (loudest -12.0 dBFS) but never held 1000 ms");
    expect(loud).not.toContain("delivers no audio");
    expect(loud).not.toMatch(/pairing/i);
    expect(explainAudioWait(seen(400, 400, -30), { floorDbfs: -20, holdMs: 2_000 })).toContain(
      "DistroAV delivers no audio",
    );
    expect(explainAudioWait(seen(400, 400, -12), { floorDbfs: -20, holdMs: 2_000 })).toContain(
      "never held 2000 ms",
    );
  });

  /** Feed one probe reading per 50 ms from `from` to `to` (inclusive) at
   *  the peak `peakAt(t)`, then let the wait's 50 ms bound hit at `end`. */
  async function boundMessage(
    peakAt: (t: number) => number,
    from: number,
    to: number,
    end: number,
    opts: { floorDbfs?: number; holdMs?: number; maxGapMs?: number } = {},
  ): Promise<string> {
    const meters = fakeMeters();
    let t = from;
    const wait = waitForInputAudio(meters.subscribe, PROBE, { now: () => t, timeoutMs: 50, ...opts });
    for (t = from; t <= to; t += 50) meters.push([probeAt(peakAt(t))]);
    t = end;
    const err = await wait.then(
      () => null,
      (e: Error) => e,
    );
    expect(err, "the bound rejects").not.toBeNull();
    return err!.message;
  }

  test("a single run after silence, still open at the bound, is a late start", async () => {
    // Review rounds 4-5: silence, then steady audio still flowing when the
    // bound hits. Only what was observed: when it began, its last reading.
    const msg = await boundMessage((t) => (t < 650 ? 0 : 0.25), 0, 1_100, 1_100);
    expect(msg).toContain("rose above the floor only after silence, 450 ms before the bound");
    expect(msg).toContain("last reading 0 ms before it");
    expect(msg).toContain("started late");
    expect(msg).not.toContain("with gaps");
    expect(msg).not.toContain("delivers no audio");
    expect(msg).not.toMatch(/pairing/i);
  });

  test("one silent reading before the run is enough for a late start", async () => {
    // Review round 7: the smallest metered silence, a single reading.
    const msg = await boundMessage((t) => (t < 50 ? 0 : 0.25), 0, 500, 500);
    expect(msg).toContain("rose above the floor only after silence, 450 ms before the bound");
    expect(msg).toContain("started late");
  });

  test("a late start whose readings stalled names when it began, not a hold-sized guess", async () => {
    // Review round 5: the open run's last reading may be up to the gap bound
    // before the end, so it may have begun more than the hold before it.
    const msg = await boundMessage((t) => (t < 200 ? 0 : 0.25), 0, 1_100, 1_550);
    expect(msg).toContain("1350 ms before the bound");
    expect(msg).toContain("last reading 450 ms before it");
  });

  test("an open run exactly at the gap bound still counts as open", async () => {
    // The same <= as nextAudioStreak: 500 ms after the last reading is open,
    // 501 ms is not.
    const open = await boundMessage((t) => (t < 600 ? 0 : 0.25), 0, 1_000, 1_500);
    expect(open).toContain("started late");
    const stale = await boundMessage((t) => (t < 600 ? 0 : 0.25), 0, 1_000, 1_501);
    expect(stale).not.toContain("started late");
    expect(stale).toContain("rose above the floor (loudest -12.0 dBFS) but never held 1000 ms");
  });

  test("audio with gaps whose bound lands mid-burst is gaps, never a late start", async () => {
    // Review round 5: 300 ms loud / 200 ms silent, the bound in a loud phase.
    // Four runs above the floor: the open one is not the first.
    const msg = await boundMessage((t) => (t % 500 < 300 ? 0.25 : 0), 0, 1_750, 1_750);
    expect(msg).toContain("rose above the floor (loudest -12.0 dBFS) but never held 1000 ms");
    expect(msg).toContain("4 runs above it");
    expect(msg).toContain("with gaps");
    expect(msg).not.toContain("started late");
  });

  test("a run after an event gap counts again: two runs are gaps, never a late start", async () => {
    // Review round 6: one silent reading, loud, no event for 600 ms, loud
    // again and still open at the bound. The restart after the gap is a
    // second run. The silent reading first (review round 7) leaves the RUN
    // COUNT as the only thing between this state and a "late start".
    const meters = fakeMeters();
    let t = 0;
    const wait = waitForInputAudio(meters.subscribe, PROBE, { now: () => t, timeoutMs: 50 });
    meters.push([probeAt(0)]);
    for (t = 50; t <= 300; t += 50) meters.push([probeAt(0.25)]);
    for (t = 900; t <= 1_100; t += 50) meters.push([probeAt(0.25)]);
    t = 1_100;
    const msg = await wait.then(
      () => "",
      (e: Error) => e.message,
    );
    expect(msg).toContain("2 runs above it");
    expect(msg).toContain("more than 500 ms apart");
    expect(msg).not.toContain("started late");
  });

  test("a late start needs a silent reading of the probe before its run", async () => {
    // Review round 6: the probe absent from the events (inactive) for a long
    // stretch, then one loud run open at the bound. Nothing at or below the
    // floor was ever metered, so "after silence" would be false: the probe
    // became active late, it is not DistroAV's late start.
    const meters = fakeMeters();
    let t = 0;
    const wait = waitForInputAudio(meters.subscribe, PROBE, { now: () => t, timeoutMs: 50 });
    for (t = 0; t <= 600; t += 50) meters.push([{ inputName: "cam", inputLevelsMul: [[1, 1, 1]] }]);
    for (t = 650; t <= 1_050; t += 50) meters.push([probeAt(0.25)]);
    t = 1_050;
    const msg = await wait.then(
      () => "",
      (e: Error) => e.message,
    );
    expect(msg).not.toContain("started late");
    expect(msg).not.toContain("after silence");
    expect(msg).toContain("1 run above it");
  });

  test("a run that went stale before the bound is not a late start", async () => {
    // The last loud event is more than the gap bound before the bound: the
    // events stopped, so the run is not "still" open.
    const msg = await boundMessage(() => 0.25, 0, 300, 900);
    expect(msg).not.toContain("started late");
    expect(msg).toContain("rose above the floor (loudest -12.0 dBFS) but never held 1000 ms");
    expect(msg).toContain("1 run above it");
  });

  test("the bound explains with the wait's own floor", async () => {
    // Review round 4: -30 dBFS readings under a -20 dBFS floor are silence
    // for THIS wait, so the explanation says "no audio", never "rose above".
    const msg = await boundMessage(() => 10 ** (-30 / 20), 0, 2_500, 2_500, {
      floorDbfs: -20,
      holdMs: 2_000,
    });
    expect(msg).toContain("input peak above -20 dBFS for 2000 ms in a row");
    expect(msg).toContain("DistroAV delivers no audio");
    expect(msg).not.toContain("rose above the floor");
  });

  test("the bound explains with the wait's own hold and gap bound", async () => {
    // Review round 5: -12 dBFS for 1.5 s, then one silent reading, under a
    // 2 s hold and a 200 ms gap bound: the explanation names THOSE.
    const msg = await boundMessage((t) => (t <= 1_500 ? 0.25 : 0), 0, 1_550, 1_550, {
      floorDbfs: -20,
      holdMs: 2_000,
      maxGapMs: 200,
    });
    expect(msg).toContain("never held 2000 ms");
    expect(msg).toContain("more than 200 ms apart");
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
