/**
 * The post-deploy wall frame gate's decision (#272 lane 2). The owner
 * (16.10.2026): no CI run may pass while the LED wall judders.
 *
 * `scripts/wall_frames.py` reads SongPlayer's frame-ID wall clips back from
 * the wall output as Windows composes it (dxcam), and `wall_analysis.py`
 * judges every boundary: lost, out of order, held the wrong number of
 * refreshes (judder), blended at the wrong weight, frozen. This module
 * turns its exit code and JSON report into the gate's failures. A gate
 * never passes without positive evidence: only exit 0 with the verdict
 * `clean` passes, or a take whose ONE defect is slips (the owner, #272,
 * 31.10.2026: at most `MAX_SLIPS` per 40 s take; a slip is one picture held a
 * refresh long, the next a refresh short). An incomplete or failed capture
 * fails too, named as what it is, never as judder.
 */

/** The reader's report fields the gate reads (`wall_analysis.analyse`). */
export interface WallFramesReport {
  verdict: "clean" | "defects" | "incomplete";
  fps: number;
  samples: number;
  decoded: number;
  boundaries: number;
  lost_frames: number;
  lost_boundaries: number;
  jumps_back: number;
  cadence_errors: number;
  frozen: boolean;
  weights: { mismatches: number };
  refreshes: {
    judged: number;
    uneven: number;
    unresolved: number;
    /** Boundaries whose timing contradicts itself: the capture's fault. */
    inconsistent?: number;
    /** The capture samples the uneven boundaries start at. */
    uneven_samples?: number[];
    /** Each uneven boundary's number and the fewest refreshes it surely
     *  held; absent = none listed. */
    uneven_boundaries?: { boundary: number; refreshes: number }[];
  } | null;
  unverified: { steps: number };
  /** #272 v2: the copies of the clip's strip the reader found, their cell
   *  width (px) and the dxcam region it grabbed; null when it found none. */
  located?: { copies: number; cell_px: number; region: number[] | null } | null;
}

/** What the reader's exit codes mean (`wall_frames.py`'s module doc). */
export const EXIT_CLEAN = 0;
export const EXIT_DEFECTS = 1;
export const EXIT_USAGE = 2;
export const EXIT_INCOMPLETE = 3;
export const EXIT_NOTHING_DECODED = 4;
export const EXIT_CAPTURE_FAILED = 5;
/** The reader's own floors for a clean take (`MIN_COVERAGE`, `MIN_BOUNDARIES`). */
const MIN_COVERAGE = 0.95;
const MIN_JUDGED = 300;
/** The slips a take may hold (the owner, #272, 31.10.2026). */
const MAX_SLIPS = 3;
/** A slip holds a picture one refresh long on a 60 Hz wall: 3. More is a
 *  stall, which never passes as a slip. */
const MAX_SLIP_REFRESHES = 3;

/** The slips the uneven boundaries make: two neighbours are one, one picture
 *  long and the next short; a lone boundary is one too, so a run counts one
 *  per two boundaries, rounded up. */
function slipCount(at: number[]): number {
  let slips = 0;
  let run = 0;
  at.forEach((n, i) => {
    run = i > 0 && n === at[i - 1] + 1 ? run + 1 : 1;
    if (run % 2 === 1) {
      slips += 1;
    }
  });
  return slips;
}

/** Whether the take's only defect is at most `MAX_SLIPS` slips, with
 *  everything a clean verdict needs besides. */
function slipsOnly(report: WallFramesReport): boolean {
  const r = report.refreshes;
  const uneven = r?.uneven_boundaries ?? [];
  return (
    r !== null &&
    r.uneven > 0 &&
    uneven.length === r.uneven &&
    uneven.every((b) => b.refreshes <= MAX_SLIP_REFRESHES) &&
    slipCount(uneven.map((b) => b.boundary)) <= MAX_SLIPS &&
    r.unresolved === 0 &&
    (r.inconsistent ?? 0) === 0 &&
    r.judged >= MIN_JUDGED &&
    report.unverified.steps === 0 &&
    report.decoded >= MIN_COVERAGE * report.samples &&
    defects("", { ...report, refreshes: { ...r, uneven: 0 } }).length === 0
  );
}

/**
 * The gate's failures for one clip's take: `exitCode` and `report` (null
 * when stdout held no report). Empty = the take passed.
 */
export function wallFramesFailures(
  clip: string,
  exitCode: number | null,
  report: WallFramesReport | null,
): string[] {
  const where = `${clip}:`;
  if (exitCode === EXIT_NOTHING_DECODED) {
    return [
      `${where} no picture of the clip was read on the wall output: the wall does not show SongPlayer's program (Arena's clip on another source or disconnected, MAX off, or the clip not on program), or no whole copy of the clip's strip and its ruler is in view (the reader's stderr says which)`,
    ];
  }
  if (exitCode === EXIT_CAPTURE_FAILED) {
    return [
      `${where} the capture of the wall output failed (see the reader's stderr): no verdict, the gate cannot pass`,
    ];
  }
  if (exitCode === EXIT_USAGE) {
    return [`${where} the reader refused its arguments (exit 2)`];
  }
  if (report === null) {
    return [`${where} the reader exited ${exitCode} with no report`];
  }
  if (exitCode === EXIT_DEFECTS && report.verdict === "defects" && slipsOnly(report)) {
    return [];
  }
  const failures = defects(where, report);
  if (exitCode === EXIT_DEFECTS || report.verdict === "defects") {
    if (failures.length === 0) {
      failures.push(`${where} the reader reported defects (exit 1) that no counter names`);
    }
    return failures;
  }
  if (exitCode === EXIT_INCOMPLETE || report.verdict === "incomplete") {
    return [incomplete(where, report)];
  }
  if (exitCode !== EXIT_CLEAN || report.verdict !== "clean") {
    return [`${where} the reader exited ${exitCode} with the verdict ${report.verdict}`];
  }
  return [];
}

/** One line per defect counter that is not zero. */
function defects(where: string, report: WallFramesReport): string[] {
  const out: string[] = [];
  const uneven = report.refreshes?.uneven ?? 0;
  if (uneven > 0) {
    const judged = report.refreshes?.judged ?? 0;
    out.push(
      `${where} JUDDER: ${uneven} of ${judged} judged boundaries were held the wrong number of refreshes`,
    );
  }
  if (report.jumps_back > 0) {
    out.push(`${where} ${report.jumps_back} jump(s) back: an older frame shown after a newer one`);
  }
  if (report.lost_frames > 0) {
    out.push(`${where} ${report.lost_frames} frame(s) of the clip never shown`);
  }
  if (report.lost_boundaries > 0) {
    out.push(`${where} ${report.lost_boundaries} boundary(ies) lost`);
  }
  if (report.cadence_errors > 0) {
    out.push(`${where} ${report.cadence_errors} cadence error(s): a step off the 30 fps grid`);
  }
  if (report.weights.mismatches > 0) {
    out.push(`${where} ${report.weights.mismatches} blend weight(s) off the pacer's rule`);
  }
  if (report.frozen) {
    out.push(`${where} the wall froze: fewer new pictures than half the grid's boundaries`);
  }
  if (report.boundaries === 0) {
    out.push(`${where} no boundary of the clip was read`);
  }
  return out;
}

/** Why a take could not be judged clean. */
function incomplete(where: string, report: WallFramesReport): string {
  const parts: string[] = [];
  if (report.unverified.steps > 0) {
    parts.push(`${report.unverified.steps} step(s) the capture did not see`);
  }
  if (report.samples > 0) {
    parts.push(`${report.decoded} of ${report.samples} pictures decoded`);
  }
  if (report.refreshes === null) {
    parts.push("no refresh check");
  } else {
    if (report.refreshes.unresolved > 0) {
      parts.push(`${report.refreshes.unresolved} boundary(ies) unresolved`);
    }
    if ((report.refreshes.inconsistent ?? 0) > 0) {
      parts.push(`${report.refreshes.inconsistent} boundary(ies) with contradictory timing`);
    }
    parts.push(`${report.refreshes.judged} boundaries judged`);
  }
  return `${where} the measurement is INCOMPLETE (not judder, not clean): ${parts.join(", ")}`;
}

/** A layer of Arena's composition as `GET /api/v1/composition` lists it
 *  (only the fields the gate reads; Arena's own REST, read only). */
export interface ArenaLayer {
  bypassed?: { value?: boolean };
  clips?: Array<{
    name?: { value?: string };
    connected?: { value?: string };
    video?: { description?: string } | null;
  }>;
}

/** The Spout sender Arena's wall clip receives (`SPOUT_SENDER_NAME`). */
export const MAX_SENDER = "SP-program-MAX";

/** A clip whose source is Arena's Blank generator (its description's first
 *  line), which draws nothing, so it covers nothing (#272, SNV 20.10.2026:
 *  L17-19). */
function drawsNothing(description: string | undefined): boolean {
  return (description ?? "").split("\n")[0].trim() === "Blank";
}

/**
 * Why a take decoded nothing, read from Arena's composition (#272, 19.10.2026:
 * the operator's layers left on after a service covered the clip): the
 * connected clips of every layer drawn ABOVE the highest layer whose
 * connected clip receives `SP-program-MAX` (Arena numbers layers from 1 at
 * the bottom; a higher one draws on top), as `L<n> '<clip>'`, a bypassed
 * layer left out. With no layer receiving it, that alone is the reason. The
 * gate only names them: the composition is the operator's, never cleared.
 */
export function coveringLayers(layers: ArenaLayer[]): string[] {
  const connected = (layer: ArenaLayer) =>
    (layer.clips ?? []).filter((clip) => clip.connected?.value === "Connected");
  let wall = -1;
  layers.forEach((layer, i) => {
    if (connected(layer).some((clip) => (clip.video?.description ?? "").includes(MAX_SENDER))) {
      wall = i;
    }
  });
  if (wall < 0) {
    return [`no connected Arena clip receives ${MAX_SENDER}`];
  }
  const above: string[] = [];
  layers.forEach((layer, i) => {
    if (i <= wall || layer.bypassed?.value === true) {
      return;
    }
    for (const clip of connected(layer)) {
      if (!drawsNothing(clip.video?.description)) {
        above.push(`L${i + 1} '${clip.name?.value ?? ""}'`);
      }
    }
  });
  return above;
}
