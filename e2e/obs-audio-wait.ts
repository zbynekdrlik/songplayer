/**
 * The A/V gate waits for its probe's AUDIO before it records (#221, dev.18)
 * — the pure decision, plus the bounded wait over any meter source.
 *
 * Why: the gate records `SP-program` through cg OBS's probe input
 * (`av-sync-probe.ts`), a DistroAV receiver attached just before the take. A
 * freshly attached receiver delivers its PICTURE at once. Its AUDIO reaches
 * cg OBS's MIX (what StartRecord records) only with gaps until camera-box's
 * genlock audio pairing (camera-box 1367) has fixed the delay. By their
 * design the pairing withholds the packets from the mix until its latch
 * locks.
 *
 * The cg OBS log of the dev.17 run (local time):
 * - the probe's scene reset at 08:49:31.566;
 * - DistroAV bound the source at 33.114;
 * - `genlock-shallow-lock` at 36.133;
 * - DEGRADED `audio_pairing` at 36.217;
 * - LOCKED at 37.214, 4.1 s after the bind.
 *
 * The gate logged its take start at 35.705: 4.1 s after the reset and 2.6 s
 * after the bind. The take opened with two dropouts (0.100 s / 22 ms and
 * 0.227 s / 234 ms; run 37423917199). Its audio came at 0.122–0.227 s, then
 * stopped until 0.461 s, and the rest of the 20 s was clean. Before lane 3
 * the gate recorded a long-attached input and never met this warm-up.
 *
 * The condition (the main session's decided design): the probe's
 * obs-websocket `InputVolumeMeters` reading must be above a silence floor
 * (−60 dBFS) for 1 s in a row. The wait is bounded (20 s), and the bound
 * fails loudly with the meter state it saw. The dropout check, its
 * thresholds and its edge guard are unchanged.
 *
 * **What the meter can NOT see (review round 2).** obs-websocket's meter is
 * an audio CAPTURE CALLBACK. camera-box's libobs calls it for every packet
 * the source outputs: `source_signal_audio_data`, at the end of
 * `source_output_audio_data` (camera-box
 * `vendor/obs-studio/libobs/obs-source.c`). That includes the packets the
 * pairing WITHHOLDS from the mix (the `GENLOCK_AUDIO_ACT_WITHHOLD` branch
 * just before it). camera-box's `genlock-audio-pairing.md` says the same:
 * "its packets never enter the mix (they still reach the audio
 * callbacks/monitoring)".
 *
 * So this wait proves that DistroAV delivers audio, and adds the 1 s hold.
 * It never observes the pairing's withhold: on its own, its cover for the
 * warm-up was only the time it takes (the dev.17 take would have started
 * ~1 s later, after the audio turned clean, 0.461 s into it, but still
 * before LOCKED). #221 dev.19 narrows it: the gate first waits for cg OBS's
 * genlock state (`probe-lock-wait.ts`, camera-box's `genlock_lock` facet:
 * the probe locked, the box LOCKED for none), then runs this wait, then
 * records. That facet reads the withhold's PENDING phase as paired too, so
 * the withhold is still not observed exactly (the open design question on
 * #221, comment 6014055098).
 *
 * What a reading is (obs-websocket 5,
 * `plugins/obs-websocket/src/utils/Obs_VolumeMeter.cpp`):
 * - `inputLevelsMul` is one `[magnitude × volume, peak × volume, peak]`
 *   triple per channel, linear (1.0 = 0 dBFS). The gate reads the THIRD
 *   value, the input peak BEFORE the input's fader and mute: the question is
 *   whether the receiver delivers audio. A muted or faded probe still
 *   records silence and then fails as unmeasurable (`obs-ndi-health.md`).
 * - One event every 50 ms, carrying only ACTIVE inputs: those on the
 *   program feed (`obs_source_active`). An input that is only on a preview is
 *   not metered. An event without the probe resets the streak.
 * - A level is HELD until no audio has arrived for more than 0.3 s, then
 *   reset to 0. So a gap in DistroAV's delivery shorter than ~300 ms is
 *   invisible to this meter too (`obs-ndi-health.md`, "Limit").
 *
 * Only continuity that was OBSERVED counts: a gap of more than
 * `MAX_METER_GAP_MS` between two meter events restarts the streak.
 *
 * The obs-websocket glue (the `Reidentify` around the wait) is
 * `ObsDriver.waitForInputAudio` (`obs-driver.ts`); these helpers are
 * unit-tested in the ubuntu mock suite (`obs-audio-wait.spec.ts`).
 */

/** At or below this input peak a reading is silence. */
export const SILENCE_FLOOR_DBFS = -60;

/** The audio must be above the floor for this long in a row. */
export const AUDIO_HOLD_MS = 1_000;

/** The wait's bound, well above camera-box's ~4 s from the bind to LOCKED
 *  (dev.17: 33.114 → 37.214). */
export const AUDIO_WAIT_TIMEOUT_MS = 20_000;

/** More than this between two meter events (obs-websocket sends one every
 *  50 ms) restarts the streak: it was not observed. */
export const MAX_METER_GAP_MS = 500;

/** One input of an obs-websocket `InputVolumeMeters` event. */
export interface MeterInput {
  inputName: string;
  /** Per channel: `[magnitude × volume, peak × volume, peak]`, linear. */
  inputLevelsMul: number[][];
}

/**
 * The well-formed inputs of an `InputVolumeMeters` event's `inputs`: an entry
 * without a string `inputName` is dropped; a missing or malformed level list
 * is no channels, and a level that is not an array is dropped.
 */
export function meterInputs(raw: unknown): MeterInput[] {
  if (!Array.isArray(raw)) return [];
  const out: MeterInput[] = [];
  for (const entry of raw) {
    const e = entry as { inputName?: unknown; inputLevelsMul?: unknown } | null;
    if (!e || typeof e.inputName !== "string") continue;
    const levels = Array.isArray(e.inputLevelsMul)
      ? (e.inputLevelsMul.filter((l) => Array.isArray(l)) as number[][])
      : [];
    out.push({ inputName: e.inputName, inputLevelsMul: levels });
  }
  return out;
}

/**
 * `inputName`'s loudest channel INPUT peak (linear, the third value of each
 * channel: before the input's fader and mute) — 0 for no channels or a
 * malformed level — or null when the event does not carry the input (it is
 * not active: obs-websocket meters only active inputs).
 */
export function inputPeakMul(inputs: MeterInput[], inputName: string): number | null {
  const input = inputs.find((i) => i.inputName === inputName);
  if (!input) return null;
  let peak = 0;
  for (const level of input.inputLevelsMul) {
    const p = level[2];
    if (typeof p === "number" && Number.isFinite(p) && p > peak) peak = p;
  }
  return peak;
}

/** The streak of readings above the floor. Times are the caller's
 *  monotonic milliseconds. */
export interface AudioStreak {
  /** When the current run above the floor began; null while silent. */
  since: number | null;
  /** The previous meter event's time; null before the first. */
  lastAt: number | null;
}

/** No reading yet. */
export const NO_STREAK: AudioStreak = { since: null, lastAt: null };

export interface StreakOptions {
  /** Default [`SILENCE_FLOOR_DBFS`]. */
  floorDbfs?: number;
  /** Default [`MAX_METER_GAP_MS`]. */
  maxGapMs?: number;
}

/**
 * The streak after one meter event at `atMs` whose reading of the probe is
 * `peakMul` (null: the event does not carry the probe). A reading at or
 * below the floor, or a missing one, ends the run; a reading above it
 * extends the run, or starts one — also when the previous event is more than
 * `maxGapMs` back (that stretch was not observed).
 */
export function nextAudioStreak(
  prev: AudioStreak,
  peakMul: number | null,
  atMs: number,
  opts: StreakOptions = {},
): AudioStreak {
  const floorMul = 10 ** ((opts.floorDbfs ?? SILENCE_FLOOR_DBFS) / 20);
  const maxGapMs = opts.maxGapMs ?? MAX_METER_GAP_MS;
  if (peakMul === null || !(peakMul > floorMul)) return { since: null, lastAt: atMs };
  const observed = prev.lastAt !== null && atMs - prev.lastAt <= maxGapMs;
  const since = prev.since !== null && observed ? prev.since : atMs;
  return { since, lastAt: atMs };
}

/** Whether the audio has been above the floor for `holdMs` at `atMs`. */
export function audioFlowing(streak: AudioStreak, atMs: number, holdMs = AUDIO_HOLD_MS): boolean {
  return streak.since !== null && atMs - streak.since >= holdMs;
}

/** What the wait saw; returned when the audio flows, and in the bound's error. */
export interface AudioWaitReport {
  /** From the call to the reading that made it flow (or to the bound). */
  waitedMs: number;
  /** `InputVolumeMeters` events received. */
  events: number;
  /** Of those, the events that carried the probe. */
  withInput: number;
  /** The loudest input peak read, dBFS (−Infinity: none above 0). */
  loudestDbfs: number;
  /** The probe's per-channel input peaks in its last reading, dBFS; null
   *  before any reading of it. */
  lastPeaksDbfs: number[] | null;
  /** The longest run above the floor seen. */
  longestStreakMs: number;
  /** How many runs above the floor began (a run restarted after a gap in
   *  the events counts again). */
  runsAboveFloor: number;
  /** How many readings of the probe were at or below the floor. */
  readingsAtOrBelowFloor: number;
  /** The run above the floor still open when the wait ended — its last
   *  reading within the gap bound of the end — as what was observed: how
   *  long before the end it began, and its last reading. Null when none was. */
  openRun: { beganMsBeforeEnd: number; lastReadingMsBeforeEnd: number } | null;
}

export interface AudioWaitOptions extends StreakOptions {
  /** Default [`AUDIO_HOLD_MS`]. */
  holdMs?: number;
  /** Default [`AUDIO_WAIT_TIMEOUT_MS`]. */
  timeoutMs?: number;
  /** The monotonic clock (ms); default `performance.now`. */
  now?: () => number;
}

/** Linear → dBFS (0 → −Infinity). */
export function mulToDbfs(mul: number): number {
  return mul > 0 ? 20 * Math.log10(mul) : -Infinity;
}

function fmtDbfs(db: number): string {
  return Number.isFinite(db) ? db.toFixed(1) : "-inf";
}

/** The meter state in words, for the bound's error. */
export function describeAudioWait(report: AudioWaitReport): string {
  if (report.events === 0) {
    return "no InputVolumeMeters event arrived";
  }
  const last = report.lastPeaksDbfs
    ? `[${report.lastPeaksDbfs.map(fmtDbfs).join(", ")}] dBFS`
    : "none";
  return (
    `${report.events} InputVolumeMeters events, ${report.withInput} with the input; ` +
    `last input peaks ${last}, loudest ${fmtDbfs(report.loudestDbfs)} dBFS, ` +
    `longest run above the floor ${Math.round(report.longestStreakMs)} ms`
  );
}

/**
 * What the meter state at the bound most likely means, under the wait's own
 * floor and hold:
 * - no event at all;
 * - an input that was never active;
 * - an active input whose ONLY run above the floor came after a metered
 *   silence (a reading of it at or below the floor) and was still open at
 *   the end (shorter than the hold): its audio started late;
 * - an active input that rose above the floor but never held, in any other
 *   way (several runs, or one that ended): audio with gaps, or the meter
 *   events stopped;
 * - an active input that never rose above the floor: DistroAV delivers it
 *   no audio.
 *
 * Every number it names was observed.
 */
export function explainAudioWait(
  report: AudioWaitReport,
  opts: { floorDbfs?: number; holdMs?: number; maxGapMs?: number } = {},
): string {
  const floorDbfs = opts.floorDbfs ?? SILENCE_FLOOR_DBFS;
  const holdMs = opts.holdMs ?? AUDIO_HOLD_MS;
  const maxGapMs = opts.maxGapMs ?? MAX_METER_GAP_MS;
  if (report.events === 0) {
    return "obs-websocket sent no meter event: the InputVolumeMeters subscription did not apply.";
  }
  if (report.withInput === 0) {
    return (
      "The input was in none of the events, so it was never active " +
      "(obs-websocket meters only the inputs on the program feed): is cg OBS on " +
      "the probe scene, and is the probe's scene item visible?"
    );
  }
  // camera-box's genlock audio pairing withholds packets from the MIX, after
  // the meter's tap, so it never silences or breaks this meter (review
  // round 2): no branch below names it.
  // A single open run had no silent reading inside it, so a silent reading
  // of the probe came before it: "after silence" is what was metered.
  const open = report.openRun;
  if (open !== null && report.runsAboveFloor === 1 && report.readingsAtOrBelowFloor > 0) {
    return (
      `The probe's audio rose above the floor only after silence, ` +
      `${Math.round(open.beganMsBeforeEnd)} ms before the bound, and was still above it ` +
      `there (last reading ${Math.round(open.lastReadingMsBeforeEnd)} ms before it): ` +
      `DistroAV's audio for the probe started late.`
    );
  }
  if (report.loudestDbfs > floorDbfs) {
    const runs = `${report.runsAboveFloor} run${report.runsAboveFloor === 1 ? "" : "s"}`;
    return (
      `The probe's audio rose above the floor (loudest ${fmtDbfs(report.loudestDbfs)} dBFS) ` +
      `but never held ${holdMs} ms (${runs} above it): DistroAV delivers it with gaps, or ` +
      `the meter events stopped (more than ${maxGapMs} ms apart, or the probe left the ` +
      `program feed).`
    );
  }
  return (
    "The probe was metered but stayed at or below the floor, so DistroAV delivers no audio " +
    "for it: does SP-program carry the playlist's sound, and is the probe's NDI audio " +
    "(ndi_audio) on?"
  );
}

/**
 * A meter source: `onMeters` gets every `InputVolumeMeters` event's inputs,
 * and `onClosed` is called if the connection carrying them closes. Returns
 * the removal of both.
 */
export type MeterSubscribe = (
  onMeters: (inputs: MeterInput[]) => void,
  onClosed: (reason: string) => void,
) => () => void;

/**
 * Wait until `inputName`'s audio is above the floor for `holdMs` in a row
 * ([`nextAudioStreak`] over every meter event). It rejects in two cases,
 * naming the input, the condition and the meter state it saw
 * ([`describeAudioWait`], [`explainAudioWait`]):
 * - once `timeoutMs` has passed;
 * - at once when the source reports its connection closed.
 *
 * The listeners are removed as soon as the wait ends, either way.
 */
export function waitForInputAudio(
  subscribe: MeterSubscribe,
  inputName: string,
  opts: AudioWaitOptions = {},
): Promise<AudioWaitReport> {
  const now = opts.now ?? (() => performance.now());
  const holdMs = opts.holdMs ?? AUDIO_HOLD_MS;
  const timeoutMs = opts.timeoutMs ?? AUDIO_WAIT_TIMEOUT_MS;
  const floorDbfs = opts.floorDbfs ?? SILENCE_FLOOR_DBFS;
  const condition = `input peak above ${floorDbfs} dBFS for ${holdMs} ms in a row`;
  return new Promise<AudioWaitReport>((resolve, reject) => {
    const start = now();
    const report: AudioWaitReport = {
      waitedMs: 0,
      events: 0,
      withInput: 0,
      loudestDbfs: -Infinity,
      lastPeaksDbfs: null,
      longestStreakMs: 0,
      runsAboveFloor: 0,
      readingsAtOrBelowFloor: 0,
      openRun: null,
    };
    const maxGapMs = opts.maxGapMs ?? MAX_METER_GAP_MS;
    let streak = NO_STREAK;
    let done = false;
    let unsubscribe: (() => void) | null = null;
    // The error is built AFTER `waitedMs` and `openRun` are set, so it can
    // name them.
    const finish = (error: (() => Error) | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      unsubscribe?.();
      const end = now();
      report.waitedMs = end - start;
      // A run is still open only if its last reading is within the gap bound
      // of the end (the same <= as `nextAudioStreak`); an older one stopped
      // (the events did).
      report.openRun =
        streak.since !== null && streak.lastAt !== null && end - streak.lastAt <= maxGapMs
          ? { beganMsBeforeEnd: end - streak.since, lastReadingMsBeforeEnd: end - streak.lastAt }
          : null;
      if (error) reject(error());
      else resolve(report);
    };
    const timer = setTimeout(() => {
      finish(() => {
        const why = explainAudioWait(report, { floorDbfs, holdMs, maxGapMs });
        return new Error(
          `the audio of OBS input "${inputName}" did not flow (${condition}) within ` +
            `${timeoutMs} ms: ${describeAudioWait(report)}. ${why} (#221)`,
        );
      });
    }, timeoutMs);
    unsubscribe = subscribe(
      (inputs) => {
        if (done) return;
        const at = now();
        report.events++;
        const peak = inputPeakMul(inputs, inputName);
        if (peak !== null) {
          report.withInput++;
          report.loudestDbfs = Math.max(report.loudestDbfs, mulToDbfs(peak));
          const input = inputs.find((i) => i.inputName === inputName)!;
          report.lastPeaksDbfs = input.inputLevelsMul.map((l) =>
            typeof l[2] === "number" && Number.isFinite(l[2]) ? mulToDbfs(l[2]) : -Infinity,
          );
        }
        const prevSince = streak.since;
        streak = nextAudioStreak(streak, peak, at, opts);
        if (streak.since !== null) {
          if (streak.since !== prevSince) report.runsAboveFloor++;
          report.longestStreakMs = Math.max(report.longestStreakMs, at - streak.since);
        } else if (peak !== null) {
          // A reading of the probe that ended (or never began) a run: at or
          // below the floor.
          report.readingsAtOrBelowFloor++;
        }
        if (audioFlowing(streak, at, holdMs)) finish(null);
      },
      (reason) => {
        finish(
          () =>
            new Error(
              `the OBS connection closed (${reason}) while waiting for the audio of input ` +
                `"${inputName}" (${condition}), after ${Math.round(report.waitedMs)} ms: ` +
                `${describeAudioWait(report)} (#221)`,
            ),
        );
      },
    );
    // A source that delivered events synchronously may have ended the wait
    // before `unsubscribe` was known.
    if (done) unsubscribe();
  });
}
