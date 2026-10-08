import { test, expect } from "@playwright/test";
import {
  WINDOW_BLOCKS,
  asioGateFailures,
  fohPathFailures,
  gatedAsioOutputs,
  parseVbanHeader,
  receiverFailures,
  type AsioTelemetry,
  type OutputStatus,
} from "./audio-outputs-gate";

// #233: the post-deploy gate's pure functions (`audio-outputs-gate.ts`), run in
// the mock suite: FOH's path through the output list, a VBAN receiver's
// verdict on a destination's packets, and (lane 3) an ASIO output's minute.

const foh = (blocks: number, over: Partial<OutputStatus> = {}): OutputStatus => ({
  id: "out-1",
  type: "vban",
  name: "fohabl.lan:6980",
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
    targets: [{ target: "fohabl.lan:6980", addr: "10.77.7.30:6980", error: null }],
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
  test("FOH sending 48 kHz INT24 sp-program passes", () => {
    expect(fohPathFailures([foh(100)], [foh(130)])).toEqual([]);
    expect(fohPathFailures([foh(100)], [foh(125)]), "exactly 25 blocks").toEqual([]);
  });

  test("a missing FOH output fails", () => {
    expect(fohPathFailures([], [])).toEqual(["no VBAN output to fohabl.lan:6980"]);
    const lv1 = foh(130, {
      vban: { stream_name: "sp-program", packets_sent: 0, send_errors: 0, targets: [{ target: "lv1.lan:6980", addr: null, error: null }] },
    });
    expect(fohPathFailures([foh(100)], [lv1])).toEqual(["no VBAN output to fohabl.lan:6980"]);
  });

  test("a FOH output moved off 48 kHz INT24 fails with each reason", () => {
    const moved = foh(130, { rate: 96000, format: "int16", state: "waiting", reason: "no IPv4 address" });
    expect(fohPathFailures([foh(100)], [moved])).toEqual([
      "the FOH output is waiting (no IPv4 address)",
      "the FOH output runs at 96000 Hz, not 48000",
      "the FOH output sends int16, not int24",
    ]);
    const renamed = foh(130, { delay_ms: 20, state: "opening" });
    renamed.vban!.stream_name = "cg";
    expect(fohPathFailures([foh(100)], [renamed])).toEqual([
      "the FOH output is opening",
      "the FOH stream is cg, not sp-program",
      "the FOH output is delayed 20 ms",
    ]);
  });

  test("a FOH output that stalled or errors fails", () => {
    const stalled = foh(110);
    stalled.vban!.send_errors = 3;
    expect(fohPathFailures([foh(100)], [stalled])).toEqual([
      "only 10 FOH blocks went out between the reads",
      "3 FOH send errors between the reads",
    ]);
    expect(fohPathFailures([foh(100)], [foh(124)])).toEqual(["only 24 FOH blocks went out between the reads"]);
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
    const vban = foh(10);
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
