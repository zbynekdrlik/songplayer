/**
 * #347: the program audio across a scene cut — the post-deploy transition
 * gate's pure verdict (unit-tested in the mock suite by
 * `transition-gate.spec.ts`, run on SNV by `post-deploy-transition.spec.ts`).
 *
 * The owner, 20.11.2026: every scene switch at the events stutters and the
 * outgoing song is "pulled down hard" instead of crossfading — and no test
 * ever judged what a cut sounds like, although CI cuts scenes all the time.
 *
 * The spec records the program through a temporary VBAN output on a loopback
 * socket (the real fan-out after the crossfade and the limiter) around one
 * playlist → playlist cut. Between the cut and 80 % of the fade the outgoing
 * song still weighs cos θ ≥ 0.31 (equal power), so neither a digital-zero run
 * nor a deep level dip can come from the crossfade itself: either one is the
 * outgoing song cut short (paused early, a missing side, a black + silence
 * step before the next scene).
 */

import { parseVbanHeader } from "./audio-outputs-gate";

/** One received datagram and when it arrived (ms, the spec's clock). */
export interface Arrival {
  at_ms: number;
  bytes: Uint8Array;
}

/** The probe's stream: 48 kHz, INT16, stereo. */
export const RATE = 48_000;
export const SR_INDEX_48K = 3;
export const FORMAT_INT16 = 0x01;

/** A zero run this long (5 ms) is a hole, never a quiet note. */
export const ZERO_RUN_FRAMES = 240;
/** The level window. */
export const LEVEL_WINDOW_FRAMES = 4_800; // 100 ms
/** How far below the outgoing song's level a window may fall in the span. */
export const MAX_DIP_DB = 20;
/** The judged span ends at this fraction of the fade. */
export const SPAN_FRACTION = 0.8;
/**
 * When the spec reads the outgoing playlist's transport: half way through
 * the fade. Its hold ends one slot after the window's end, so a read near
 * the end could find a hold that ended on time.
 */
export const TRANSPORT_FRACTION = 0.5;

/** The decoded stream: interleaved stereo samples and each packet's first frame's arrival. */
export interface Decoded {
  samples: Int16Array;
  /** frame index → arrival ms of the packet that carried it (first frame of each packet). */
  marks: { frame: number; at_ms: number }[];
  failures: string[];
}

/** Decode the probe's datagrams in arrival order (stream `stream`). */
export function decode(arrivals: Arrival[], stream: string): Decoded {
  const failures: string[] = [];
  const chunks: Int16Array[] = [];
  const marks: { frame: number; at_ms: number }[] = [];
  let frames = 0;
  let last: number | null = null;
  for (const a of arrivals) {
    const h = parseVbanHeader(a.bytes);
    if (!h || h.stream !== stream) continue;
    if (h.srIndex !== SR_INDEX_48K || h.formatBit !== FORMAT_INT16 || h.channels !== 2) {
      failures.push(`a packet is not 48 kHz INT16 stereo: ${JSON.stringify(h)}`);
      break;
    }
    if (last !== null && h.counter !== ((last + 1) >>> 0)) {
      failures.push(`a VBAN packet was lost: the counter jumps ${last} -> ${h.counter}`);
    }
    last = h.counter;
    const n = h.frames * 2;
    const body = a.bytes.subarray(28, 28 + n * 2);
    const view = new DataView(body.buffer, body.byteOffset, body.length);
    const s = new Int16Array(n);
    for (let i = 0; i < n; i++) s[i] = view.getInt16(i * 2, true);
    marks.push({ frame: frames, at_ms: a.at_ms });
    chunks.push(s);
    frames += h.frames;
  }
  const samples = new Int16Array(frames * 2);
  let o = 0;
  for (const c of chunks) {
    samples.set(c, o);
    o += c.length;
  }
  return { samples, marks, failures };
}

/** The frame that went out at `at_ms`: the first packet that arrived at or after it. */
export function frameAt(marks: { frame: number; at_ms: number }[], at_ms: number): number | null {
  const m = marks.find((x) => x.at_ms >= at_ms);
  return m ? m.frame : null;
}

/** RMS of frames [from, to) in dBFS (both channels), -Infinity for silence. */
export function rmsDb(samples: Int16Array, from: number, to: number): number {
  let sum = 0;
  let n = 0;
  for (let f = Math.max(0, from); f < Math.min(to, samples.length / 2); f++) {
    const l = samples[2 * f] / 32768;
    const r = samples[2 * f + 1] / 32768;
    sum += l * l + r * r;
    n += 2;
  }
  return n === 0 || sum === 0 ? -Infinity : 10 * Math.log10(sum / n);
}

/** The longest run of frames where both channels are exactly 0, inside [from, to). */
export function longestZeroRun(samples: Int16Array, from: number, to: number): { frames: number; at: number } {
  let best = { frames: 0, at: from };
  let run = 0;
  for (let f = Math.max(0, from); f < Math.min(to, samples.length / 2); f++) {
    if (samples[2 * f] === 0 && samples[2 * f + 1] === 0) {
      run++;
      if (run > best.frames) best = { frames: run, at: f - run + 1 };
    } else {
      run = 0;
    }
  }
  return best;
}

/** The median of the 100 ms levels in [from, to). */
export function medianLevelDb(samples: Int16Array, from: number, to: number): number {
  const levels: number[] = [];
  for (let f = from; f + LEVEL_WINDOW_FRAMES <= to; f += LEVEL_WINDOW_FRAMES) {
    levels.push(rmsDb(samples, f, f + LEVEL_WINDOW_FRAMES));
  }
  if (levels.length === 0) return -Infinity;
  levels.sort((a, b) => a - b);
  return levels[Math.floor(levels.length / 2)];
}

/** What the spec measured around one cut. */
export interface CutRead {
  /** The frame the cut went out at (the press). */
  cutFrame: number;
  /** The fade's length and the cue's wait, in frames. */
  fadeFrames: number;
  cueFrames: number;
  /** Counters before and after: transition + program health. */
  before: { side_fills: number; cue_timeouts: number; filled_owned: number };
  after: { side_fills: number; cue_timeouts: number; filled_owned: number };
  /** The outgoing playlist's transport read at TRANSPORT_FRACTION of the fade. */
  outgoingTransport: string | null;
}

/** The verdict: one line per failure. */
export function transitionFailures(samples: Int16Array, read: CutRead): string[] {
  const f: string[] = [];
  const spanEnd = read.cutFrame + read.cueFrames + Math.floor(read.fadeFrames * SPAN_FRACTION);
  const preFrom = read.cutFrame - 3 * RATE;
  const preTo = read.cutFrame - RATE / 2;
  if (preFrom < 0 || spanEnd > samples.length / 2) {
    f.push(`the recording does not cover the cut (frames ${samples.length / 2}, need ${preFrom}..${spanEnd})`);
    return f;
  }
  const pre = medianLevelDb(samples, preFrom, preTo);
  if (!Number.isFinite(pre) || pre < -50) {
    f.push(`the outgoing song was silent before the cut (median ${pre.toFixed(1)} dBFS): no judgement possible`);
    return f;
  }
  const zero = longestZeroRun(samples, read.cutFrame, spanEnd);
  if (zero.frames >= ZERO_RUN_FRAMES) {
    f.push(
      `the program went silent for ${((zero.frames * 1000) / RATE).toFixed(1)} ms ` +
        `${(((zero.at - read.cutFrame) * 1000) / RATE).toFixed(0)} ms after the cut (the outgoing song still weighs ≥ 31 % there)`,
    );
  }
  let worst = { db: Infinity, at: read.cutFrame };
  for (let s = read.cutFrame; s + LEVEL_WINDOW_FRAMES <= spanEnd; s += LEVEL_WINDOW_FRAMES / 2) {
    const db = rmsDb(samples, s, s + LEVEL_WINDOW_FRAMES);
    if (db < worst.db) worst = { db, at: s };
  }
  if (worst.db < pre - MAX_DIP_DB) {
    f.push(
      `the level fell to ${worst.db.toFixed(1)} dBFS ${(((worst.at - read.cutFrame) * 1000) / RATE).toFixed(0)} ms after the cut, ` +
        `${(pre - worst.db).toFixed(1)} dB under the outgoing song's ${pre.toFixed(1)} dBFS (allowed ${MAX_DIP_DB})`,
    );
  }
  for (const k of ["side_fills", "cue_timeouts", "filled_owned"] as const) {
    const grew = read.after[k] - read.before[k];
    if (grew > 0) f.push(`${k} grew by ${grew} over the cut`);
  }
  if (read.outgoingTransport !== "Playing") {
    f.push(
      `the outgoing playlist was ${read.outgoingTransport ?? "unread"} half way through the fade, not Playing: its hold ended early`,
    );
  }
  return f;
}
