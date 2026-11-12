import { test, expect } from "@playwright/test";
import {
  WINDOW_BLOCKS,
  asioGateFailures,
  danteGateFailures,
  danteGateLog,
  DANTE_LATE_TOP_BINS,
  lv1PathFailures,
  gatedAsioOutputs,
  waitingForClock,
  asioCount,
  asioCountReading,
  parseVbanHeader,
  receiverFailures,
  type AsioTelemetry,
  type DanteTelemetry,
  type OutputStatus,
} from "./audio-outputs-gate";

// #233: the post-deploy gate's pure functions (`audio-outputs-gate.ts`), run in
// the mock suite: lv1's path through the output list, a VBAN receiver's
// verdict on a destination's packets, and (lane 3) an ASIO output's minute.

const lv1 = (blocks: number, over: Partial<OutputStatus> = {}): OutputStatus => ({
  id: "out-1",
  type: "vban",
  name: "lv1.lan:6980",
  enabled: true,
  state: "running",
  reason: null,
  rate: 48000,
  format: "int24",
  channels: 2,
  delay_ms: 0,
  latency_ms: 66.67,
  blocks_sent: blocks,
  blocks_dropped: 0,
  vban: {
    stream_name: "sp-program",
    packets_sent: blocks * 8,
    send_errors: 0,
    targets: [{ target: "lv1.lan:6980", addr: "10.77.7.31:6980", error: null }],
  },
  ...over,
});

function packet(sr: number, frames: number, bit: number, name: string, counter: number): Uint8Array {
  const b = new Uint8Array(28 + frames * 6);
  b.set([0x56, 0x42, 0x41, 0x4e, sr, frames - 1, 1, bit]);
  b.set(Array.from(name, (c) => c.charCodeAt(0)), 8);
  new DataView(b.buffer).setUint32(24, counter, true);
  return b;
}

test.describe("audio outputs gate (#233)", () => {
  test("lv1 sending 48 kHz INT24 sp-program passes", () => {
    expect(lv1PathFailures([lv1(100)], [lv1(130)])).toEqual([]);
    expect(lv1PathFailures([lv1(100)], [lv1(125)]), "exactly 25 blocks").toEqual([]);
  });

  test("a missing lv1 output fails", () => {
    expect(lv1PathFailures([], [])).toEqual(["no VBAN output to lv1.lan:6980"]);
    const elsewhere = lv1(130, {
      vban: { stream_name: "sp-program", packets_sent: 0, send_errors: 0, targets: [{ target: "fohabl.lan:6980", addr: null, error: null }] },
    });
    expect(lv1PathFailures([lv1(100)], [elsewhere])).toEqual(["no VBAN output to lv1.lan:6980"]);
  });

  test("an lv1 output moved off 48 kHz INT24 fails with each reason", () => {
    const moved = lv1(130, { rate: 96000, format: "int16", state: "waiting", reason: "no IPv4 address" });
    expect(lv1PathFailures([lv1(100)], [moved])).toEqual([
      "the lv1 output is waiting (no IPv4 address)",
      "the lv1 output runs at 96000 Hz, not 48000",
      "the lv1 output sends int16, not int24",
    ]);
    const renamed = lv1(130, { delay_ms: 20, state: "opening" });
    renamed.vban!.stream_name = "cg";
    expect(lv1PathFailures([lv1(100)], [renamed])).toEqual([
      "the lv1 output is opening",
      "the lv1 stream is cg, not sp-program",
      "the lv1 output is delayed 20 ms",
    ]);
  });

  test("an lv1 output that stalled or errors fails", () => {
    const stalled = lv1(110);
    stalled.vban!.send_errors = 3;
    expect(lv1PathFailures([lv1(100)], [stalled])).toEqual([
      "only 10 lv1 blocks went out between the reads",
      "3 lv1 send errors between the reads",
    ]);
    expect(lv1PathFailures([lv1(100)], [lv1(124)])).toEqual(["only 24 lv1 blocks went out between the reads"]);
  });

  test("a VBAN header parses", () => {
    expect(parseVbanHeader(packet(4, 200, 2, "sp-e2e-96k", 7))).toEqual({
      srIndex: 4,
      subProtocol: 0,
      frames: 200,
      channels: 2,
      formatBit: 2,
      stream: "sp-e2e-96k",
      counter: 7,
    });
    expect(parseVbanHeader(new Uint8Array(10))).toBeNull();
    const notVban = packet(4, 200, 2, "x", 0);
    notVban[0] = 0x57;
    expect(parseVbanHeader(notVban)).toBeNull();
  });

  test("a contiguous 96 kHz stream passes; a wrong index or a gap fails", () => {
    const want = { srIndex: 4, frames: 200, formatBit: 2, stream: "sp-e2e-96k", minPackets: 3 };
    const good = [0, 1, 2].map((k) => packet(4, 200, 2, "sp-e2e-96k", 0xffffffff + k));
    expect(receiverFailures(good, want)).toEqual([]);
    expect(receiverFailures([packet(3, 200, 2, "sp-e2e-96k", 0), ...good.slice(1)], want)[0]).toContain(
      "a packet carries",
    );
    const gap = [
      packet(4, 200, 2, "sp-e2e-96k", 1),
      packet(4, 200, 2, "sp-e2e-96k", 3),
      packet(4, 200, 2, "sp-e2e-96k", 4),
    ];
    expect(receiverFailures(gap, want)).toEqual(["the frame counter jumps 1 -> 3"]);
    expect(receiverFailures(good.slice(0, 2), want)).toEqual(["2 packets, want at least 3"]);
    expect(receiverFailures([new Uint8Array(4), ...good], want)).toEqual(["a datagram is not VBAN"]);
  });
});

const dvs = (blocks: number, over: Partial<OutputStatus> = {}, asio: Partial<AsioTelemetry> = {}): OutputStatus => ({
  id: "out-3",
  type: "asio",
  name: "DVS",
  enabled: true,
  state: "running",
  reason: null,
  rate: 96000,
  format: "Int32LSB",
  channels: 2,
  delay_ms: 0,
  latency_ms: 70.7,
  blocks_sent: blocks,
  blocks_dropped: 0,
  asio: {
    driver: "Dante Virtual Soundcard (x64)",
    channels: [0, 1],
    driver_rate: 96000,
    sample_type: "Int32LSB",
    ppm: 3.2,
    underruns: 0,
    resets: 0,
    hard_recentres: 0,
    resyncs: 0,
    latency_ms: 70.7,
    ...asio,
  },
  ...over,
});

test.describe("ASIO gate (#233)", () => {
  test("a minute at the driver's rate with no underrun passes", () => {
    expect(WINDOW_BLOCKS).toBe(1800);
    expect(asioGateFailures(dvs(100), dvs(1900))).toEqual([]);
    expect(asioGateFailures(dvs(100), dvs(100 + WINDOW_BLOCKS)), "exactly a minute").toEqual([]);
    expect(asioGateFailures(dvs(100), dvs(1900, {}, { ppm: -300 })), "the bound itself").toEqual([]);
    expect(asioGateFailures(dvs(100), dvs(1900, { latency_ms: 999.9 }))).toEqual([]);
  });

  test("each failure is named", () => {
    const bad = dvs(
      1000,
      { state: "waiting", reason: "the driver asked for a reset", rate: 48000, latency_ms: 0 },
      { underruns: 4, resets: 1, hard_recentres: 2, ppm: 301 },
    );
    expect(asioGateFailures(dvs(100), bad)).toEqual([
      "the ASIO output is waiting (the driver asked for a reset)",
      "it runs at 48000 Hz, the driver at 96000 Hz",
      "4 underruns in the window",
      "the driver was reopened 1 times in the window",
      "2 hard re-centres in the window",
      "its correction is 301 ppm (bound 300)",
      "its latency is 0 ms",
      "only 900 blocks in the window, want 1800",
    ]);
    expect(asioGateFailures(dvs(100), dvs(1899))).toEqual(["only 1799 blocks in the window, want 1800"]);
    expect(asioGateFailures(dvs(100), dvs(1900, { latency_ms: 1000 }))).toEqual(["its latency is 1000 ms"]);
    expect(asioGateFailures(dvs(100), dvs(1900, { state: "opening" }))).toEqual(["the ASIO output is opening"]);
  });

  // #233 review round 2: a hard re-centre is the owner's "fault" (a faded
  // skip or insert); the gate fails on one inside the minute, never on the
  // ones before it.
  test("a hard re-centre inside the minute fails, an earlier one does not", () => {
    const hard = (n: number) => ({ hard_recentres: n });
    expect(asioGateFailures(dvs(100, {}, hard(3)), dvs(1900, {}, hard(3)))).toEqual([]);
    expect(asioGateFailures(dvs(100, {}, hard(3)), dvs(1900, {}, hard(4)))).toEqual([
      "1 hard re-centres in the window",
    ]);
  });

  test("an output with no ASIO telemetry, or a driver at no rate, fails", () => {
    const none = dvs(1900);
    delete none.asio;
    expect(asioGateFailures(dvs(100), none)).toEqual(["no ASIO telemetry"]);
    expect(asioGateFailures(dvs(100), dvs(1900, { rate: 0 }, { driver_rate: 0 }))).toEqual([
      "it runs at 0 Hz, the driver at 0 Hz",
    ]);
  });

  // The owner's ruling (#233, 8.10.2026): at PP, DVS opens but gives no
  // clock while PP's network has no Dante PTP clock; its enabled output
  // waits, calmly, and a box that expects no ASIO output
  // (SP_ASIO_OUTPUTS_EXPECTED "0") must not fail on it. Any other waiting
  // reason, a disabled entry and a VBAN output keep their old treatment.
  test("an output waiting for its driver's clock is not counted or measured", () => {
    const vban = lv1(10);
    const off = dvs(10, { id: "out-4", enabled: false });
    const noClock = dvs(10, { id: "out-5", state: "waiting", reason: "the driver gives no clock" }, {
      reason_code: "no_clock",
    });
    const reset = dvs(10, { id: "out-6", state: "waiting" }, { reason_code: "reset" });
    const ids = (list: OutputStatus[]) => gatedAsioOutputs(list).map((o) => o.id);
    expect(ids([vban, off, noClock]), "PP: nothing to gate").toEqual([]);
    expect(ids([vban, dvs(10), noClock, reset, off])).toEqual(["out-3", "out-6"]);
    expect(ids([dvs(10, {}, { reason_code: "no_clock" })]), "a running one is gated").toEqual(["out-3"]);
  });

  // Review round 10: the outputs the gate leaves out for waiting on their
  // clock are named, so a box expecting one (SNV) that fails on a count of 0
  // says that its DVS waits for a clock, not that it has no output.
  test("the outputs waiting for their driver's clock are named", () => {
    const noClock = dvs(10, { id: "out-5", state: "waiting" }, { reason_code: "no_clock" });
    const off = dvs(10, { id: "out-4", enabled: false, state: "waiting" }, { reason_code: "no_clock" });
    const reset = dvs(10, { id: "out-6", state: "waiting" }, { reason_code: "reset" });
    const ids = (list: OutputStatus[]) => waitingForClock(list).map((o) => o.id);
    expect(ids([lv1(10), dvs(10), noClock, off, reset])).toEqual(["out-5"]);
    expect(ids([lv1(10), dvs(10)])).toEqual([]);
  });

  // #229 (the owner's ruling, 9.11.2026): PP's SoundGrid with its network
  // off takes audio ~2.9 % off its rate; the output waits calmly, reason
  // `clock_off_rate`, like a driver with no clock: named, never measured.
  test("an output whose card runs off its rate waits like one with no clock", () => {
    const offRate = dvs(10, { id: "out-2", state: "waiting" }, { reason_code: "clock_off_rate" });
    const running = dvs(10, {}, { reason_code: "clock_off_rate" });
    expect(waitingForClock([lv1(10), offRate, running]).map((o) => o.id)).toEqual(["out-2"]);
    expect(gatedAsioOutputs([lv1(10), offRate, running]).map((o) => o.id)).toEqual(["out-3"]);
  });

  // #229 (ROZHODNUTÉ 11.11.2026, PP deploy 43001397048): an output in a calm
  // clock wait is an enabled output behaving correctly while its device or
  // network is off: it counts toward SP_ASIO_OUTPUTS_EXPECTED, is named and is
  // not measured; the running ones are measured as before.
  test("an output in a calm clock wait counts toward the expected outputs, unmeasured", () => {
    const offRate = dvs(10, { id: "out-2", state: "waiting" }, { reason_code: "clock_off_rate" });
    const noClock = dvs(10, { id: "out-5", state: "waiting" }, { reason_code: "no_clock" });
    const pp = [lv1(10), offRate, dvs(10)];
    expect(asioCount(pp, true), "PP: SoundGrid off, Dante running").toEqual({
      counted: 2,
      measured: ["out-3"],
      waiting: ["out-2 (clock_off_rate)"],
    });
    expect(asioCountReading(pp, 2, true)).toBe(2);
    expect(asioCount([offRate, noClock], true)).toEqual({
      counted: 2,
      measured: [],
      waiting: ["out-2 (clock_off_rate)", "out-5 (no_clock)"],
    });
  });

  test("any other output state, a disabled entry or a missing output still fails the count", () => {
    const offRate = dvs(10, { id: "out-2", state: "waiting" }, { reason_code: "clock_off_rate" });
    const reset = dvs(10, { id: "out-6", state: "waiting" }, { reason_code: "reset" });
    const off = dvs(10, { id: "out-4", enabled: false, state: "disabled" });
    // A waiting output with another reason is measured: the running poll fails on it.
    expect(asioCount([reset, offRate], true)).toEqual({
      counted: 2,
      measured: ["out-6"],
      waiting: ["out-2 (clock_off_rate)"],
    });
    expect(asioCount([lv1(10), off, offRate], true).counted, "a disabled entry counts nothing").toBe(1);
    expect(asioCountReading([lv1(10), offRate], 2, true), "out-3 missing").toBe(
      "1 (measured: none; waiting for a clock: out-2 (clock_off_rate))",
    );
    expect(asioCountReading([dvs(10)], 2, true)).toBe("1 (measured: out-3; waiting for a clock: none)");
  });

  // #229 review F1 (11.11.2026): the calm wait counts only on a box that
  // allows it (SP_ASIO_CLOCK_WAIT_OK, set by deploy-pp.yml alone: PP's
  // SoundGrid network is off outside services). On resolume-snv a DVS or
  // native Dante output with no clock is a fault: it counts nothing, is still
  // named, and the deploy fails.
  test("a calm clock wait counts nothing on a box that does not allow it", () => {
    const noClock = dvs(10, { id: "out-4", state: "waiting" }, { reason_code: "no_clock" });
    expect(asioCount([noClock], false)).toEqual({
      counted: 0,
      measured: [],
      waiting: ["out-4 (no_clock)"],
    });
    expect(asioCountReading([noClock], 1, false)).toBe(
      "0 (measured: none; waiting for a clock: out-4 (no_clock))",
    );
    expect(asioCountReading([dvs(10)], 1, false)).toBe(1);
  });

  // #233 review round 1: an entry's delay (up to 2 s) is part of its
  // latency, so the bound is a second ABOVE the delay, not a second flat.
  test("the latency bound is a second above the entry's delay", () => {
    const delayed = (latency_ms: number) => dvs(1900, { delay_ms: 1500, latency_ms }, { latency_ms });
    expect(asioGateFailures(dvs(100), delayed(1566.7)), "1.5 s of delay").toEqual([]);
    expect(asioGateFailures(dvs(100), delayed(2499.9))).toEqual([]);
    expect(asioGateFailures(dvs(100), delayed(2500))).toEqual(["its latency is 2500 ms"]);
    expect(asioGateFailures(dvs(100), delayed(0))).toEqual(["its latency is 0 ms"]);
  });
});

test("a native Dante output is gated like an ASIO one (#285)", () => {
  const dante = { id: "out-4", type: "dante", enabled: true, state: "running" } as OutputStatus;
  const off = { ...dante, id: "out-5", enabled: false } as OutputStatus;
  expect(gatedAsioOutputs([dante, off]).map((o) => o.id)).toEqual(["out-4"]);
});

// #285 (design 6809328088): the native Dante output's stack figures over
// the ASIO gate's minute. `late_bins` at dstack's edges: < 0.25, 0.5, 1, 2,
// 5 ms, then 5 ms or more; the last two are 2 ms or more late.
const danteFigures = (over: Partial<DanteTelemetry> = {}): DanteTelemetry => ({
  clock_ready: true,
  live_flows: 1,
  late_bins: [1000, 20, 3, 1, 0, 0],
  late_max_us: 2400,
  send_max_us: 90,
  pump_late: 2,
  pump_missed: 0,
  pump_overruns: 1,
  ...over,
});
const out4 = (blocks: number, dante: DanteTelemetry | undefined): OutputStatus => ({
  ...dvs(blocks, { id: "out-4", type: "dante", name: "Dante" }, { driver: "SongPlayer", driver_rate: 48000 }),
  rate: 48000,
  ...(dante ? { dante } : {}),
});

test.describe("Dante gate (#285)", () => {
  test("a clean minute passes, its late maximum is not gated", () => {
    expect(DANTE_LATE_TOP_BINS).toBe(2);
    const later = danteFigures({ late_bins: [90_000, 900, 40, 9, 0, 0], late_max_us: 9_000, send_max_us: 900 });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, later))).toEqual([]);
    // Only the bins under 2 ms grew, by one packet each.
    const one = danteFigures({ late_bins: [1001, 21, 4, 2, 0, 0] });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, one))).toEqual([]);
    // Packets already late before the minute stand as they were.
    const before = danteFigures({ late_bins: [1000, 20, 3, 1, 4, 2] });
    const after = danteFigures({ late_bins: [5000, 20, 3, 1, 4, 2] });
    expect(danteGateFailures(out4(100, before), out4(1900, after))).toEqual([]);
    // A gate run with the asio gate: the same output passes both.
    expect(asioGateFailures(out4(100, danteFigures()), out4(1900, later))).toEqual([]);
  });

  test("an output of another type is not judged", () => {
    expect(danteGateFailures(dvs(100), dvs(1900))).toEqual([]);
    const vban = lv1(130, { dante: danteFigures({ clock_ready: false }) });
    expect(danteGateFailures(lv1(100), vban)).toEqual([]);
  });

  test("a running Dante output with no figures fails, naming it and the read", () => {
    const bins = danteFigures({ late_bins: [2000, 20, 3, 1, 0, 0] });
    expect(danteGateFailures(out4(100, undefined), out4(1900, bins))).toEqual([
      "Dante output out-4: no Dante figures (outputs[i].dante) at the first read",
    ]);
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, undefined))).toEqual([
      "Dante output out-4: no Dante figures (outputs[i].dante) at the second read",
    ]);
    expect(danteGateFailures(out4(100, undefined), out4(1900, undefined))).toEqual([
      "Dante output out-4: no Dante figures (outputs[i].dante) at the first and second read",
    ]);
  });

  test("the clock must be ready at both reads", () => {
    const sent = danteFigures({ late_bins: [2000, 20, 3, 1, 0, 0] });
    expect(danteGateFailures(out4(100, danteFigures({ clock_ready: false })), out4(1900, sent))).toEqual([
      "Dante output out-4: clock_ready false -> true, want true at both reads",
    ]);
    expect(
      danteGateFailures(out4(100, danteFigures()), out4(1900, { ...sent, clock_ready: false })),
    ).toEqual(["Dante output out-4: clock_ready true -> false, want true at both reads"]);
  });

  test("a receiver must hold a flow", () => {
    const sent = danteFigures({ late_bins: [2000, 20, 3, 1, 0, 0] });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, { ...sent, live_flows: 0 }))).toEqual([
      "Dante output out-4: live_flows 1 -> 0, want at least 1",
    ]);
    expect(danteGateFailures(out4(100, danteFigures({ live_flows: 0 })), out4(1900, sent))).toEqual([]);
  });

  test("packets must go out in the minute", () => {
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, danteFigures()))).toEqual([
      "Dante output out-4: packets sent (late_bins sum) 1024 -> 1024, want growth",
    ]);
    // A device rebuilt in the minute counts from 0 again: no growth.
    const fresh = danteFigures({ late_bins: [500, 0, 0, 0, 0, 0] });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, fresh))).toEqual([
      "Dante output out-4: packets sent (late_bins sum) 1024 -> 500, want growth",
    ]);
  });

  test("packets 2 ms or more late are logged, never a failure (FOH at 10 ms reads them on time)", () => {
    // #285, dev CI 43124069179: under the E2E's load out-4 sent 71 packets
    // 2-5 ms late and 1 over 5 ms in 17 min while FOH read late 0; the
    // ledger counts only pump_missed as a loss (audio-faults.md).
    const base = danteFigures();
    const later = danteFigures({ late_bins: [2000, 20, 3, 1, 2, 3] });
    expect(danteGateFailures(out4(100, base), out4(1900, later))).toEqual([]);
    expect(danteGateLog(out4(100, base), out4(1900, later))).toBe(
      "Dante output out-4: packets 2 ms or more late 0 -> 5, pump_late 2 -> 2, pump_overruns 1 -> 1",
    );
  });

  test("bins that change shape between the reads fail", () => {
    const short = danteFigures({ late_bins: [5000, 20] });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, short))).toEqual([
      "Dante output out-4: late_bins [1000,20,3,1,0,0] -> [5000,20], want the same bins at both reads",
    ]);
    const one = (late_bins: number[]) => danteFigures({ late_bins });
    expect(danteGateFailures(out4(100, one([10])), out4(1900, one([20])))).toEqual([
      "Dante output out-4: late_bins [10] -> [20], want the same bins at both reads",
    ]);
  });

  test("only a missed pump block fails; late and overrun pump blocks are logged", () => {
    const sent = { late_bins: [2000, 20, 3, 1, 0, 0] };
    const after = danteFigures({ ...sent, pump_late: 3, pump_missed: 1, pump_overruns: 4 });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, after))).toEqual([
      "Dante output out-4: pump_missed 0 -> 1",
    ]);
    expect(danteGateLog(out4(100, danteFigures()), out4(1900, after))).toBe(
      "Dante output out-4: packets 2 ms or more late 0 -> 0, pump_late 2 -> 3, pump_overruns 1 -> 4",
    );
  });

  test("a non-Dante output or missing figures log nothing", () => {
    expect(danteGateLog(dvs(100), dvs(1900))).toBe("");
    expect(danteGateLog(out4(100, undefined), out4(1900, danteFigures()))).toBe("");
  });

  test("every failure at once is listed", () => {
    const bad = danteFigures({
      clock_ready: false,
      live_flows: 0,
      late_bins: [1000, 20, 3, 1, 0, 0],
      pump_missed: 7,
    });
    expect(danteGateFailures(out4(100, danteFigures()), out4(1900, bad))).toEqual([
      "Dante output out-4: clock_ready true -> false, want true at both reads",
      "Dante output out-4: live_flows 1 -> 0, want at least 1",
      "Dante output out-4: packets sent (late_bins sum) 1024 -> 1024, want growth",
      "Dante output out-4: pump_missed 0 -> 7",
    ]);
  });
});
