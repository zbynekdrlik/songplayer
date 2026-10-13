import { test, expect } from "@playwright/test";
import {
  FAULTS_URL,
  audioFaultFailures,
  audioFaultRisks,
  audioFaultSummary,
  type FaultsAnswer,
  type PathFaults,
} from "./audio-faults-gate";

// #257: the post-deploy audio gate's pure decision — any `loss` row inside
// the window fails, `risk` rows are logged (`audio-faults-gate.ts`), run in
// the mock suite; and the mock's `GET /api/v1/audio/faults` answers the
// server's shape and its refusal.

const START = 1_760_000_000_000;

function path(over: Partial<PathFaults>): PathFaults {
  return {
    path: "out:out-1",
    detail: "FOH",
    live: true,
    clean_since_ms: START - 3_600_000,
    last_loss_ms: null,
    last_risk_ms: null,
    counts: {},
    events: [],
    ...over,
  };
}

function answer(paths: PathFaults[], from_ms = START - 3_600_000): FaultsAnswer {
  return { now_ms: START + 60_000, from_ms, hours: 1, worst: null, truncated: false, paths };
}

test("a window with no loss passes", () => {
  const clean = path({
    last_risk_ms: START + 1_000,
    events: [{ ts_ms: START + 1_000, kind: "late_callback", severity: "risk", amount: 1 }],
  });
  expect(audioFaultFailures(answer([clean]), START)).toEqual([]);
});

test("a loss before the window passes, one at its start fails", () => {
  const before = path({
    last_loss_ms: START - 1,
    events: [{ ts_ms: START - 1, kind: "underrun", severity: "loss", amount: 2 }],
  });
  expect(audioFaultFailures(answer([before]), START)).toEqual([]);
  const atStart = path({
    path: "loop:out-2",
    detail: "SoundGrid",
    last_loss_ms: START,
    events: [
      { ts_ms: START - 1, kind: "dropout", severity: "loss", amount: 512 },
      { ts_ms: START, kind: "mismatch", severity: "loss", amount: 256 },
      { ts_ms: START + 5, kind: "late_callback", severity: "risk", amount: 1 },
    ],
  });
  expect(audioFaultFailures(answer([before, atStart]), START)).toEqual([
    "loop:out-2 (SoundGrid): a loss at 2025-10-09T08:53:20.000Z: mismatch ×256 at 2025-10-09T08:53:20.000Z",
  ]);
});

test("a loss whose rows the answer no longer lists still fails", () => {
  const truncated = path({ last_loss_ms: START + 10, events: [] });
  expect(audioFaultFailures(answer([truncated]), START)).toEqual([
    "out:out-1 (FOH): a loss at 2025-10-09T08:53:20.010Z: (its rows are past the answer's 500 events)",
  ]);
});

test("an answer that starts after the window fails", () => {
  expect(audioFaultFailures(answer([], START + 1), START)).toEqual([
    "the answer starts at 2025-10-09T08:53:20.001Z, after the window (2025-10-09T08:53:20.000Z)",
  ]);
});

test("the risks inside the window are listed for the log", () => {
  const risky = path({
    events: [
      { ts_ms: START - 1, kind: "late_packet", severity: "risk", amount: 12_000 },
      { ts_ms: START + 2, kind: "late_packet", severity: "risk", amount: 15_000 },
      { ts_ms: START + 3, kind: "start", severity: "info", amount: 1 },
    ],
  });
  expect(audioFaultRisks(answer([risky]), START)).toEqual([
    "out:out-1 (FOH): late_packet ×15000 at 2025-10-09T08:53:20.002Z",
  ]);
});

test("the evidence names every path's last loss, last risk and counts", () => {
  const risky = path({ last_risk_ms: START + 5, counts: { late_packet: 1 } });
  const lossy = path({
    path: "out:out-2",
    detail: "SoundGrid",
    clean_since_ms: START + 10,
    last_loss_ms: START + 10,
    counts: { underrun: 2 },
  });
  expect(audioFaultSummary(answer([risky, lossy]))).toEqual([
    "out:out-1 (FOH): clean since 2025-10-09T07:53:20.000Z, last loss none, " +
      'last risk 2025-10-09T08:53:20.005Z, counts {"late_packet":1}',
    "out:out-2 (SoundGrid): clean since 2025-10-09T08:53:20.010Z, last loss 2025-10-09T08:53:20.010Z, " +
      'last risk none, counts {"underrun":2}',
  ]);
});

test("the mock answers the ledger's shape and refuses an hours out of range", async ({ request }) => {
  const resp = await request.get(FAULTS_URL);
  expect(resp.status()).toBe(200);
  const body = (await resp.json()) as FaultsAnswer;
  expect(body.hours).toBe(1);
  expect(body.from_ms).toBe(body.now_ms - 3_600_000);
  expect(body.paths.map((p) => p.path)).toEqual(["loop:out-2", "out:out-1", "out:out-2", "pipeline:1", "program"]);
  expect(body.worst).toBe("out:out-2");
  expect(body.paths.find((p) => p.path === "loop:out-2")?.loop).toEqual({ checked_s: 3540, blind_s: 60 });
  // The fixture's losses are older than a window opened now.
  expect(audioFaultFailures(body, body.now_ms)).toEqual([]);
  expect(audioFaultFailures(body, body.from_ms).length).toBe(1);
  const day = (await (await request.get("/api/v1/audio/faults")).json()) as FaultsAnswer;
  expect(day.hours).toBe(24);
  for (const bad of ["0", "2161", "abc", "%2B5"]) {
    const refused = await request.get(`/api/v1/audio/faults?hours=${bad}`);
    expect(refused.status(), bad).toBe(400);
    expect(await refused.text()).toBe("hours must be a whole number from 1 to 2160");
  }
});
