/**
 * #257: the post-deploy audio gate on the audio fault ledger. The owner
 * (12.10.2026): "akékoľvek puknutie, strata samplu je neakceptovateľná" —
 * any fault of any audio path inside a post-deploy audio window fails the
 * deploy, on SNV and PP.
 *
 * SongPlayer records every audio path's faults (`GET /api/v1/audio/faults`,
 * the persisted ledger; `.claude/rules/audio-faults.md`). A post-deploy
 * audio spec waits one settle (`LEDGER_SETTLE_MS`) once its outputs run,
 * then opens its window with the answer's `now_ms` (the server's clock), so
 * a fault from before it is never stamped inside; it does its work, waits
 * the settle again, reads again and fails on any `loss` row stamped at or
 * after the window's start; `risk` rows are logged (`audio-faults-read.ts`
 * reads the box). These pure helpers make the decision (unit-tested in the
 * mock suite, `audio-faults-gate.spec.ts`).
 */

/** One row of a path's answer. */
export interface FaultEvent {
  ts_ms: number;
  kind: string;
  severity: string;
  amount: number;
}

/** One audio path of the answer. */
export interface PathFaults {
  path: string;
  detail: string;
  live: boolean;
  clean_since_ms: number;
  last_loss_ms: number | null;
  last_risk_ms: number | null;
  /** #257 lane 2: the window's loss / risk rows (the dashboard's count). */
  losses?: number;
  risks?: number;
  counts: Record<string, number>;
  events: FaultEvent[];
  loop?: { checked_s: number; blind_s: number };
}

/** `GET /api/v1/audio/faults?hours=N`. */
export interface FaultsAnswer {
  now_ms: number;
  from_ms: number;
  hours: number;
  /** #257 lane 2: the oldest instant the ledger keeps (its 90 days). */
  kept_from_ms?: number;
  /** #257 lane 2: since when the whole chain lost no sample (beyond the window). */
  clean_since_ms?: number;
  worst: string | null;
  truncated: boolean;
  paths: PathFaults[];
}

/** The gate reads the last hour (a spec file runs minutes, never an hour). */
export const FAULTS_URL = "/api/v1/audio/faults?hours=1";

/** How long a fault takes to reach the ledger's table, at most, plus a
 * second: a playlist pipeline's counters reach the server's registry with
 * its 5 s health heartbeat (`pipeline.rs`), and the ledger writes a step at
 * its next 5 s tick. Waited once before a window opens (a fault from before
 * it then has its row stamped before it) and once after the window's work,
 * before the last read. An absence cannot be polled for, so this is one
 * bounded settle, not a poll. */
export const LEDGER_SETTLE_MS = 11_000;

/** A row's time, readable in the log. */
function at(ms: number): string {
  return new Date(ms).toISOString();
}

/** The rows of `severity` stamped at or after `startMs`, one line each. */
function rowsSince(path: PathFaults, severity: string, startMs: number): string[] {
  return path.events
    .filter((e) => e.severity === severity && e.ts_ms >= startMs)
    .map((e) => `${e.kind} ×${e.amount} at ${at(e.ts_ms)}`);
}

/** Why the window from `startMs` fails: a line per path with a `loss` row in
 * it (its last loss, and its loss rows when the answer still lists them), and
 * an answer that does not cover the window. Empty = no sample lost. */
export function audioFaultFailures(answer: FaultsAnswer, startMs: number): string[] {
  const failures: string[] = [];
  if (answer.from_ms > startMs) {
    failures.push(`the answer starts at ${at(answer.from_ms)}, after the window (${at(startMs)})`);
  }
  for (const p of answer.paths) {
    if (p.last_loss_ms === null || p.last_loss_ms < startMs) continue;
    const rows = rowsSince(p, "loss", startMs);
    const listed = rows.length > 0 ? rows.join(", ") : "(its rows are past the answer's 500 events)";
    failures.push(`${p.path} (${p.detail}): a loss at ${at(p.last_loss_ms)}: ${listed}`);
  }
  return failures;
}

/** The `risk` rows stamped at or after `startMs`, one line each (logged,
 * never a failure). */
export function audioFaultRisks(answer: FaultsAnswer, startMs: number): string[] {
  return answer.paths.flatMap((p) => rowsSince(p, "risk", startMs).map((r) => `${p.path} (${p.detail}): ${r}`));
}

/** The answer, one line per path (logged as evidence before a verdict):
 * its last loss and risk, since when it is clean, its counts. */
export function audioFaultSummary(answer: FaultsAnswer): string[] {
  const when = (ms: number | null) => (ms === null ? "none" : at(ms));
  return answer.paths.map(
    (p) =>
      `${p.path} (${p.detail}): clean since ${at(p.clean_since_ms)}, last loss ${when(p.last_loss_ms)}, ` +
      `last risk ${when(p.last_risk_ms)}, counts ${JSON.stringify(p.counts)}`,
  );
}
