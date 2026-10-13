/**
 * #259: the LED wall report's post-deploy gate (pure, unit-tested in the
 * mock suite by `wall-gate.spec.ts`).
 *
 * The owner's rule (ROZHODNUTÉ 13.10.2026, #259 comment 6142937258):
 * SongPlayer never enforces a display or Resolume configuration — the
 * operator's own setup is the truth, and the screens may be off, Resolume
 * closed or re-arranged at any time. So the wall's state never fails a
 * deploy: it is logged. The gate fails only when SongPlayer's own readers
 * are broken: the report did not answer, never ran, or reads `unknown`.
 */

/** One problem as the server names it (`sp_core::wall::WallProblemView`). */
export interface WallProblemView {
  code: string;
  short_sk: string;
  text_sk: string;
}

/** A display Resolume's saved output wants (`sp_core::wall::WantedDisplay`). */
export interface WantedDisplay {
  name: string;
  device_id: string;
  width: number;
  height: number;
  fullscreen: boolean;
}

/** A Windows display (`sp_core::wall::Display`). */
export interface WindowsDisplay {
  name: string;
  device_key: string;
  primary: boolean;
  x: number;
  y: number;
  width: number;
  height: number;
  hz: number;
}

/** `GET /api/v1/wall` (`sp_core::wall::WallStatus`). */
export interface WallStatus {
  checked: boolean;
  state: string;
  problems: WallProblemView[];
  read?: {
    arena_running?: boolean | null;
    arena_windows?: { title: string; visible: boolean }[] | null;
    arena_output?: { advanced: boolean; preset?: string | null; displays: WantedDisplay[] } | null;
    displays?: WindowsDisplay[] | null;
    nvidia?: boolean | null;
    connected?: { id: string; connector: string }[] | null;
    grids?: { rows: number; columns: number; displays: string[] }[] | null;
  };
  read_errors?: string[];
  checked_at_ms?: number | null;
}

/** How long the gate waits for SongPlayer's first wall check after a
 * restart, ms (a check runs at once, every 5 s, a read is bounded at 10 s). */
export const FIRST_CHECK_WAIT_MS = 30_000;

/** The first check has run (the gate polls for it). */
export function wallChecked(status: WallStatus | null | undefined): boolean {
  return status !== null && status !== undefined && status.checked === true;
}

/** Why the gate fails; [] = SongPlayer read the box (whatever the wall's
 * state). */
export function wallFailures(status: WallStatus | null | undefined): string[] {
  if (status === null || status === undefined) {
    return ["/api/v1/wall did not answer (a server without the #259 report?)"];
  }
  if (!status.checked) return ["the wall report has not run yet"];
  if (status.state !== "unknown") return [];
  const failures = ["the wall report could not read the box (state unknown)"];
  for (const p of status.problems ?? []) failures.push(`${p.code}: ${p.text_sk}`);
  for (const e of status.read_errors ?? []) failures.push(`read error: ${e}`);
  return failures;
}

/** One line for the gate's log: the state, what Resolume wants, what
 * Windows shows, NVAPI, and every problem. */
export function wallSummary(status: WallStatus | null | undefined): string {
  if (status === null || status === undefined) return "no answer";
  const read = status.read ?? {};
  const output = read.arena_output;
  const wanted = output
    ? `${output.advanced ? "advanced" : "simple"} preset=${JSON.stringify(output.preset ?? null)} wants=[${(
        output.displays ?? []
      )
        .map((d) => `${d.name} ${d.width}x${d.height}`)
        .join(", ")}]`
    : "output=?";
  const windows = read.displays
    ? `windows=[${read.displays.map((d) => `${d.name} ${d.width}x${d.height}@${d.x},${d.y}`).join(", ")}]`
    : "windows=?";
  const nvidia =
    read.nvidia === false
      ? "nvidia=none"
      : `nvidia=${read.connected?.length ?? "?"} connected, grids=[${(read.grids ?? [])
          .map((g) => `${g.rows}x${g.columns}`)
          .join(", ")}]`;
  const problems = (status.problems ?? []).map((p) => p.code).join(", ");
  return (
    `state=${status.state} arena=${read.arena_running ?? "?"} ${wanted} ${windows} ${nvidia}` +
    (problems ? ` problems=[${problems}]` : "")
  );
}
