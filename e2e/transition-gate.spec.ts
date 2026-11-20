// #347: the transition gate's pure verdict (no browser, no box).
import { test, expect } from "@playwright/test";
import {
  RATE,
  decode,
  frameAt,
  longestZeroRun,
  medianLevelDb,
  rmsDb,
  transitionFailures,
  type Arrival,
  type CutRead,
} from "./transition-gate";

const STREAM = "sp-e2e-xfade";
const FRAMES_PER_PACKET = 120;

/** VBAN datagrams of interleaved stereo INT16 `samples`, one per 120 frames, arriving 2.5 ms apart. */
function packets(samples: Int16Array, opts: { skip?: number; firstCounter?: number } = {}): Arrival[] {
  const out: Arrival[] = [];
  const frames = samples.length / 2;
  let counter = opts.firstCounter ?? 1000;
  for (let f = 0, i = 0; f < frames; f += FRAMES_PER_PACKET, i++) {
    const n = Math.min(FRAMES_PER_PACKET, frames - f);
    const b = new Uint8Array(28 + n * 4);
    b.set([0x56, 0x42, 0x41, 0x4e, 3, n - 1, 1, 0x01]);
    for (let k = 0; k < STREAM.length; k++) b[8 + k] = STREAM.charCodeAt(k);
    const v = new DataView(b.buffer);
    v.setUint32(24, counter >>> 0, true);
    for (let k = 0; k < n * 2; k++) v.setInt16(28 + k * 2, samples[f * 2 + k], true);
    if (opts.skip !== i) out.push({ at_ms: (f * 1000) / RATE, bytes: b });
    counter++;
  }
  return out;
}

/** Stereo sine at `amp` (0..1) and `hz`. */
function tone(seconds: number, hz: number, amp: number): Float64Array {
  const n = Math.round(seconds * RATE);
  const s = new Float64Array(n);
  for (let i = 0; i < n; i++) s[i] = amp * Math.sin((2 * Math.PI * hz * i) / RATE);
  return s;
}

/** A program: song A, then at `cut` s an equal-power fade of `fade` s into song B (or `mode`'s defect). */
function program(
  mode: "clean" | "hard_stop" | "black_step",
  cut = 4,
  fade = 2,
  total = 9,
): Int16Array {
  const a = tone(total, 440, 0.5);
  const b = tone(total, 660, 0.5);
  const n = a.length;
  const out = new Int16Array(n * 2);
  const c = Math.round(cut * RATE);
  const w = Math.round(fade * RATE);
  for (let i = 0; i < n; i++) {
    let x: number;
    if (i < c) x = a[i];
    else if (i < c + w) {
      const th = (Math.PI / 2) * ((i - c + 0.5) / w);
      const outgoing = mode === "clean" ? a[i] : 0; // hard_stop: the outgoing song paused at the cut
      const incoming = mode === "black_step" ? 0 : b[i]; // black_step: silence first (a Blank pre-step)
      x = outgoing * Math.cos(th) + incoming * Math.sin(th);
    } else x = mode === "black_step" ? 0 : b[i];
    const v = Math.round(x * 32767);
    out[2 * i] = v;
    out[2 * i + 1] = v;
  }
  return out;
}

function read(over: Partial<CutRead> = {}): CutRead {
  return {
    cutFrame: 4 * RATE,
    fadeFrames: 2 * RATE,
    cueFrames: 0,
    before: { side_fills: 10, cue_timeouts: 1, filled_owned: 0 },
    after: { side_fills: 10, cue_timeouts: 1, filled_owned: 0 },
    outgoingTransport: "Playing",
    ...over,
  };
}

test("a VBAN stream decodes whole, its frames mapped to arrival times", () => {
  const s = program("clean");
  const d = decode(packets(s), STREAM);
  expect(d.failures).toEqual([]);
  expect(d.samples).toEqual(s);
  expect(frameAt(d.marks, 1000)).toBe(48_000);
  expect(frameAt(d.marks, 99_999)).toBeNull();
});

test("a lost packet is named", () => {
  const d = decode(packets(program("clean"), { skip: 5 }), STREAM);
  expect(d.failures).toEqual(["a VBAN packet was lost: the counter jumps 1004 -> 1006"]);
});

test("another stream is ignored, a packet of another format is refused", () => {
  const p = packets(program("clean"));
  const other = p[0].bytes.slice();
  other[8] = 0x58; // another stream name
  const int24 = p[1].bytes.slice();
  int24[7] = 0x02;
  expect(decode([{ at_ms: 0, bytes: other }], STREAM).samples.length).toBe(0);
  expect(decode([{ at_ms: 0, bytes: int24 }], STREAM).failures[0]).toContain("not 48 kHz INT16 stereo");
});

test("the levels and runs are measured exactly", () => {
  const s = new Int16Array([0, 0, 0, 0, 16384, 16384, 0, 0]);
  expect(longestZeroRun(s, 0, 4)).toEqual({ frames: 2, at: 0 });
  expect(rmsDb(s, 0, 2)).toBe(-Infinity);
  expect(rmsDb(s, 2, 3)).toBeCloseTo(-6.02, 2);
  expect(medianLevelDb(new Int16Array(0), 0, 10)).toBe(-Infinity);
});

test("an equal-power crossfade passes", () => {
  expect(transitionFailures(program("clean"), read())).toEqual([]);
});

test("the outgoing song paused at the cut fails on the dip (the incoming side only fades in)", () => {
  const f = transitionFailures(program("hard_stop"), read());
  expect(f.some((x) => x.startsWith("the level fell to"))).toBe(true);
  expect(f.some((x) => x.startsWith("the program went silent for "))).toBe(false);
});

test("a fade into black + silence first (the old Blank pre-step) fails on the hole and the dip", () => {
  const f = transitionFailures(program("black_step"), read());
  expect(f.some((x) => x.startsWith("the program went silent for "))).toBe(true);
  expect(f.some((x) => x.startsWith("the level fell to"))).toBe(true);
});

test("grown counters and an outgoing playlist no longer playing fail by name", () => {
  const f = transitionFailures(
    program("clean"),
    read({
      after: { side_fills: 12, cue_timeouts: 2, filled_owned: 3 },
      outgoingTransport: "Paused",
    }),
  );
  expect(f).toEqual([
    "side_fills grew by 2 over the cut",
    "cue_timeouts grew by 1 over the cut",
    "filled_owned grew by 3 over the cut",
    "the outgoing playlist was Paused half way through the fade, not Playing: its hold ended early",
  ]);
});

test("a recording that does not cover the cut, or a silent outgoing song, is no pass", () => {
  expect(transitionFailures(program("clean"), read({ cutFrame: RATE }))[0]).toContain("does not cover the cut");
  const quiet = new Int16Array(9 * RATE * 2);
  expect(transitionFailures(quiet, read())[0]).toContain("was silent before the cut");
});

test("the cue's wait moves the judged span", () => {
  // A hole 2.3 s after the cut: outside the span with no wait (0.8 · 2 s = 1.6 s),
  // inside it with a 1 s wait.
  const s = program("clean");
  for (let i = 0; i < 480; i++) {
    const f = Math.round(6.3 * RATE) + i;
    s[2 * f] = 0;
    s[2 * f + 1] = 0;
  }
  expect(transitionFailures(s, read()).filter((x) => x.includes("went silent"))).toEqual([]);
  expect(transitionFailures(s, read({ cueFrames: RATE })).some((x) => x.includes("went silent"))).toBe(true);
});
