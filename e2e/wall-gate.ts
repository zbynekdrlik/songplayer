/**
 * #259: the LED wall report's post-deploy gate (pure, unit-tested in the
 * mock suite by `wall-gate.spec.ts`).
 *
 * The owner's rule (ROZHODNUTÉ 13.10.2026, #259 comment 6142937258):
 * SongPlayer never enforces a display or Resolume configuration — the
 * operator's own setup is the truth, and the screens may be off, Resolume
 * closed or re-arranged at any time. So the wall's state never fails a
 * deploy: it is logged. The gate fails only when SongPlayer's own readers
 * are broken: the report did not answer, never ran, or reads `unknown` for
 * its whole window (a read can fail once; the gate polls on).
 *
 * #275: the report also says whether SongPlayer's clip runs in Resolume
 * (`songplayer_clip`). The operator may show anything on the wall (slides
 * during a sermon), and after an Arena relaunch no clip runs until he
 * presses a Stream Deck scene: the clip's state is LOGGED, never a failure.
 * The gate fails only when SongPlayer's own reader is broken
 * (`clipFailures`): the deployed server does not carry the fact at all, or
 * its reader cannot read Arena's answer (`reader_fault`: a param with no
 * id, a value of another shape, an answer that does not decode — the box is
 * the reader's first real contact with Arena's REST). Arena not answering,
 * or its composition still loading, is no reader fault.
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

/** One clip Resolume shows (`sp_core::wall_clip::ClipView`). */
export interface ClipView {
  layer: string;
  clip: string;
  source: string;
}

/** #275: SongPlayer's clip in Resolume (`sp_core::wall_clip::SongplayerClip`). */
export interface SongplayerClip {
  /** `running`, `none` or `unknown`. */
  state: string;
  clips?: ClipView[];
  ndi_clips?: ClipView[];
  since_ms?: number | null;
  now_ms?: number | null;
  /** Why a host's fact is unknown (English). */
  read_error?: string | null;
  /** SongPlayer's own reader cannot read Arena's answer. */
  reader_fault?: boolean;
}

/** `GET /api/v1/wall` (`sp_core::wall::WallStatus`). */
export interface WallStatus {
  checked: boolean;
  state: string;
  problems: WallProblemView[];
  read?: {
    arena_running?: boolean | null;
    arena_windows?: { title: string; visible: boolean }[] | null;
    arena_output?: {
      advanced: boolean;
      preset?: string | null;
      preset_found?: boolean;
      displays: WantedDisplay[];
    } | null;
    displays?: WindowsDisplay[] | null;
    /** How far NVAPI was read: only when a display Resolume wants is gone
     * or has another size (`sp_core::wall::NvidiaRead`). */
    nvidia?: "not_read" | "no_driver" | "read" | null;
    connected?: { id: string; connector: string }[] | null;
    grids?: { rows: number; columns: number; displays: string[] }[] | null;
  };
  read_errors?: string[];
  checked_at_ms?: number | null;
  /** #254: the guard that ends a crashed or hung Resolume
   * (`sp_core::wall::WallGuard`); absent from an older server. */
  guard?: {
    ends: number;
    last?: { at_ms: number; why: string; pid: number; error?: string | null } | null;
    phase: string;
  } | null;
  /** #275: whether SongPlayer's clip runs in Resolume; absent from a server
   * without #275. */
  songplayer_clip?: SongplayerClip | null;
}

/** How long the gate waits for SongPlayer's first wall check after a
 * restart, ms (a check runs at once, every 5 s, a read is bounded at 10 s). */
export const FIRST_CHECK_WAIT_MS = 30_000;

/** How often the gate reads the report while it waits, ms. */
export const POLL_INTERVAL_MS = 2_000;

/** The states the health bar shows no segment for (Resolume drives no
 * physical display: nothing to report). */
export const NO_SEGMENT_STATES = ["simple_output", "no_display_output"];

/** The first check has run. */
export function wallChecked(status: WallStatus | null | undefined): boolean {
  return status !== null && status !== undefined && status.checked === true;
}

/** A check ran and read the box (any state but `unknown`): what the gate
 * polls for. */
export function wallReadable(status: WallStatus | null | undefined): boolean {
  return wallChecked(status) && status?.state !== "unknown";
}

/** Reads the report every `intervalMs` until a read is readable or
 * `timeoutMs` passed; answers that read, else the last one (judged by
 * `wallFailures`: `unknown` fails only when it held the whole window). */
export async function pollWall(
  read: () => Promise<WallStatus | null>,
  timeoutMs: number,
  wait: (ms: number) => Promise<void>,
  now: () => number = Date.now,
  intervalMs: number = POLL_INTERVAL_MS,
): Promise<WallStatus | null> {
  const deadline = now() + timeoutMs;
  let status = await read();
  while (!wallReadable(status) && now() < deadline) {
    await wait(intervalMs);
    status = await read();
  }
  return status;
}

/** Why the dashboard disagrees with itself, or null when it agrees: the
 * bar's `data-wall` marker and the segment's `data-state` (null: no
 * segment) are read in one poll, so they are one moment's. */
export function wallDashboardMismatch(marker: string, segmentState: string | null): string | null {
  if (marker === "" || marker === "unchecked") {
    return `data-wall=${JSON.stringify(marker)}: the bar's first wall poll has not landed`;
  }
  if (NO_SEGMENT_STATES.includes(marker)) {
    return segmentState === null ? null : `data-wall=${marker} but a segment shows ${segmentState}`;
  }
  if (segmentState === null) return `data-wall=${marker} but no segment`;
  return segmentState === marker ? null : `data-wall=${marker} but the segment shows ${segmentState}`;
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

/** #275: how long the gate waits for SongPlayer's clip to read (running or
 * none) or show a reader fault, ms: the driver reads the composition at its
 * start, and a failed fetch is retried after 60 s (#157). */
export const CLIP_WAIT_MS = 90_000;

/** #275: the clip fact has settled: it reads (running / none), or the
 * reader failed on Arena's answer. */
export function clipSettled(status: WallStatus | null | undefined): boolean {
  const clip = status?.songplayer_clip;
  return !!clip && (clip.state !== "unknown" || clip.reader_fault === true);
}

/** #275: reads the report every `intervalMs` until the clip settles or
 * `timeoutMs` passed; answers the last read. */
export async function pollClip(
  read: () => Promise<WallStatus | null>,
  timeoutMs: number,
  wait: (ms: number) => Promise<void>,
  now: () => number = Date.now,
  intervalMs: number = POLL_INTERVAL_MS,
): Promise<WallStatus | null> {
  const deadline = now() + timeoutMs;
  let status = await read();
  while (!clipSettled(status) && now() < deadline) {
    await wait(intervalMs);
    status = await read();
  }
  return status;
}

/** #275: why the gate fails on the clip fact; [] = SongPlayer's reader works
 * (whatever the state: `none` and an unanswered Arena are the operator's
 * and Arena's, never a deploy's fault). */
export function clipFailures(status: WallStatus | null | undefined): string[] {
  if (status === null || status === undefined || !status.checked) return [];
  const clip = status.songplayer_clip;
  if (!clip) {
    return ["the wall report carries no songplayer_clip (a server without #275?)"];
  }
  if (clip.reader_fault) {
    return [
      `SongPlayer cannot read Resolume's clip params: ${clip.read_error ?? "(no reason given)"}`,
    ];
  }
  return [];
}

/** #275: the clip fact for the gate's log: its state, how long (on the
 * server's clock), the SongPlayer clips that run and the visible NDI
 * clips; "" from a server without it. */
export function clipSummary(clip: SongplayerClip | null | undefined): string {
  if (!clip) return "";
  const names = (clips: ClipView[] | undefined) =>
    (clips ?? []).map((c) => `${c.layer} / ${c.clip}`).join(", ");
  const held =
    typeof clip.since_ms === "number" && typeof clip.now_ms === "number"
      ? ` for ${Math.round((clip.now_ms - clip.since_ms) / 1000)} s`
      : "";
  const running = (clip.clips ?? []).length > 0 ? ` [${names(clip.clips)}]` : "";
  const ndi = (clip.ndi_clips ?? []).length > 0 ? ` ndi=[${names(clip.ndi_clips)}]` : "";
  const fault = clip.reader_fault ? " reader_fault" : "";
  const error = clip.read_error ? ` error=${JSON.stringify(clip.read_error)}` : "";
  return `clip=${clip.state}${held}${running}${ndi}${fault}${error}`;
}

/** One line for the gate's log: the state, what Resolume wants, what
 * Windows shows, NVAPI, every problem, (#254) the guard's ends of Resolume
 * when it ended one since SongPlayer started, and (#275) SongPlayer's clip. */
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
    read.nvidia === "not_read"
      ? "nvidia=not read"
      : read.nvidia === "no_driver"
        ? "nvidia=none"
        : `nvidia=${read.connected?.length ?? "?"} connected, grids=${
            read.grids ? `[${read.grids.map((g) => `${g.rows}x${g.columns}`).join(", ")}]` : "?"
          }`;
  const problems = (status.problems ?? []).map((p) => p.code).join(", ");
  const guard = status.guard;
  const ended =
    guard && guard.ends > 0
      ? ` guard=ended ${guard.ends}x last=${guard.last?.why ?? "?"} pid ${guard.last?.pid ?? "?"}` +
        `${guard.last?.error ? ` (failed: ${guard.last.error})` : ""} phase=${guard.phase}`
      : "";
  const clip = clipSummary(status.songplayer_clip);
  return (
    `state=${status.state} arena=${read.arena_running ?? "?"} ${wanted} ${windows} ${nvidia}` +
    (problems ? ` problems=[${problems}]` : "") +
    ended +
    (clip ? ` ${clip}` : "")
  );
}
