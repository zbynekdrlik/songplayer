import { test, expect } from "@playwright/test";
import type { OutputStatus } from "./audio-outputs-gate";
import { SETTLE_AFTER_SPAWN_MS, spawnCounters, spawnDisturbances } from "./asio-spawn-gate";

// #233 (the spawn finding): the live gate's pure decision — a child
// SongPlayer spawns must leave every ASIO output's resyncs and hard
// re-centres where they were (`asio-spawn-gate.ts`), run in the mock suite.

const asio = (id: string, resyncs: number, hard: number, over: Partial<OutputStatus> = {}): OutputStatus => ({
  id,
  type: "asio",
  name: id,
  enabled: true,
  state: "running",
  reason: null,
  rate: 96000,
  format: "Int32LSB",
  channels: 2,
  delay_ms: 0,
  latency_ms: 70.7,
  blocks_sent: 100,
  blocks_dropped: 0,
  asio: {
    driver: "Waves SoundGrid",
    channels: [0, 1],
    driver_rate: 96000,
    sample_type: "Int32LSB",
    ppm: 1.5,
    underruns: 0,
    resets: 0,
    hard_recentres: hard,
    resyncs,
    latency_ms: 70.7,
  },
  ...over,
});

const vban: OutputStatus = {
  ...asio("out-1", 0, 0),
  type: "vban",
  asio: undefined,
};

test.describe("ASIO spawn gate (#233)", () => {
  test("the counters are read from every running gated ASIO output", () => {
    const noClock = asio("out-5", 1, 1, { state: "waiting" });
    noClock.asio!.reason_code = "no_clock";
    const off = asio("out-4", 1, 1, { enabled: false, state: "disabled" });
    const reopening = asio("out-6", 9, 9, { state: "waiting" });
    expect(spawnCounters([vban, asio("out-2", 35, 4), asio("out-3", 0, 0), noClock, off, reopening])).toEqual([
      { id: "out-2", resyncs: 35, hard_recentres: 4 },
      { id: "out-3", resyncs: 0, hard_recentres: 0 },
    ]);
    expect(spawnCounters([])).toEqual([]);
  });

  test("an undisturbed output passes", () => {
    const before = spawnCounters([asio("out-2", 35, 4), asio("out-3", 0, 0)]);
    expect(spawnDisturbances(before, before)).toEqual([]);
    expect(SETTLE_AFTER_SPAWN_MS).toBe(5000);
  });

  test("a resync or a hard re-centre after the spawn fails, naming the output", () => {
    const before = spawnCounters([asio("out-2", 35, 4), asio("out-3", 0, 0)]);
    const after = spawnCounters([asio("out-2", 40, 4), asio("out-3", 0, 2)]);
    expect(spawnDisturbances(before, after)).toEqual([
      "out-2: 5 driver resyncs after the spawn",
      "out-3: 2 hard re-centres after the spawn",
    ]);
    const both = spawnCounters([asio("out-2", 36, 5)]);
    expect(spawnDisturbances(spawnCounters([asio("out-2", 35, 4)]), both)).toEqual([
      "out-2: 1 driver resyncs after the spawn",
      "out-2: 1 hard re-centres after the spawn",
    ]);
  });

  test("an output that stopped or whose host restarted fails", () => {
    const before = spawnCounters([asio("out-2", 35, 4), asio("out-3", 2, 0)]);
    const after = spawnCounters([asio("out-2", 0, 0), asio("out-3", 2, 0, { state: "waiting" })]);
    expect(spawnDisturbances(before, after)).toEqual([
      "out-2: its counters went back (the ASIO host restarted?)",
      "out-3 does not run after the spawn",
    ]);
    const fewerRecentres = spawnCounters([asio("out-2", 35, 3)]);
    expect(spawnDisturbances(spawnCounters([asio("out-2", 35, 4)]), fewerRecentres)).toEqual([
      "out-2: its counters went back (the ASIO host restarted?)",
    ]);
  });
});
