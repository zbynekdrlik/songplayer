import { test, expect } from "@playwright/test";
import { fohPathFailures, parseVbanHeader, receiverFailures, type OutputStatus } from "./audio-outputs-gate";

// #233: the post-deploy gate's pure functions (`audio-outputs-gate.ts`), run in
// the mock suite: FOH's path through the output list, and a VBAN receiver's
// verdict on a destination's packets.

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
