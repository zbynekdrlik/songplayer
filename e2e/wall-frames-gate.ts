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
 * `clean` passes. An incomplete or failed capture fails too, named as what
 * it is, never as judder.
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
  refreshes: { judged: number; uneven: number; unresolved: number } | null;
  unverified: { steps: number };
}

/** What the reader's exit codes mean (`wall_frames.py`'s module doc). */
export const EXIT_CLEAN = 0;
export const EXIT_DEFECTS = 1;
export const EXIT_USAGE = 2;
export const EXIT_INCOMPLETE = 3;
export const EXIT_NOTHING_DECODED = 4;
export const EXIT_CAPTURE_FAILED = 5;

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
      `${where} no picture of the clip was read on the wall output: the wall does not show SongPlayer's program (Arena's clip on another source or disconnected, MAX off, or the clip not on program)`,
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
