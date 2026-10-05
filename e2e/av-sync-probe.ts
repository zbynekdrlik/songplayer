/**
 * The A/V gate's probe scene in cg OBS (#221 lane 3) — the pure decisions.
 *
 * A playlist has no NDI output of its own any more: every consumer takes
 * SongPlayer's PROGRAM (`SP-program`). The only recorder on win-resolume is cg
 * OBS (`StartRecord` records ITS program), so the post-deploy A/V gate records
 * `SP-program` through a dedicated cg OBS scene, [`AV_PROBE_SCENE`], holding
 * ONE DistroAV receiver of `SP-program` ([`AV_PROBE_INPUT`]). The gate
 * provisions it itself (design-question 6004634711, option 1):
 *
 * - created when missing, re-pointed when its source name differs, never
 *   deleted (a receiving DistroAV input does not delete reliably,
 *   `obs-ndi-health.md`);
 * - its settings are copied from an existing cg OBS NDI input, so it
 *   disconnects while it is not shown like the `sp-*` inputs (it holds no
 *   receiver on `SP-program` outside the take);
 * - not an `sp-*` name: it is never a playlist scene in SongPlayer's catalog,
 *   and `pickBaselineScene` never picks it.
 *
 * The take runs only while `SP-program` carries the baseline playlist: with
 * "OBS manuál" (-1) on program, cg OBS would record itself through
 * `SP-program` (a video loop). The glue is in `post-deploy-av-sync.spec.ts`
 * and `obs-driver.ts`; these helpers are unit-tested in the ubuntu mock suite
 * (`av-sync-probe.spec.ts`).
 */

/** The cg OBS scene the A/V gate records `SP-program` through. */
export const AV_PROBE_SCENE = "A/V gate (SP-program)";

/** Its one input: a DistroAV NDI receiver of `SP-program`. */
export const AV_PROBE_INPUT = "A/V gate SP-program";

/** SongPlayer's program NDI stream name (`program_output::PROGRAM_NDI_NAME`). */
export const PROGRAM_NDI_NAME = "SP-program";

/** DistroAV's NDI input kind. */
export const NDI_INPUT_KIND = "ndi_source";

/** The host of an advertised NDI source name `"HOST (stream)"`, or null. */
export function ndiHost(sourceName: string): string | null {
  const m = sourceName.match(/^(.+?) \((.+)\)$/);
  return m ? m[1] : null;
}

/**
 * `SP-program`'s advertised NDI source name on this box,
 * `"<HOST> (SP-program)"`. HOST is the computer name as the NDI runtime
 * announces it: `COMPUTERNAME` (the Windows uppercase form; DistroAV's re-match
 * is case-sensitive, `obs-ndi-health.md` #173), else the host an existing cg
 * OBS NDI input names. Null when neither is known.
 */
export function programSourceName(
  computerName: string | undefined,
  templateSource: string | null,
): string | null {
  const fromEnv = computerName?.trim();
  const host = fromEnv ? fromEnv : templateSource ? ndiHost(templateSource) : null;
  return host ? `${host} (${PROGRAM_NDI_NAME})` : null;
}

/**
 * The settings the probe input is created with: an existing DistroAV input's
 * (its bandwidth, sync and "disconnect when not shown" behaviour), with the
 * source name replaced. With no template, only the source name (DistroAV's
 * defaults).
 */
export function probeInputSettings(
  template: Record<string, unknown> | null,
  sourceName: string,
): Record<string, unknown> {
  return { ...(template ?? {}), ndi_source_name: sourceName };
}

/** What cg OBS has of the probe now. */
export interface ProbeState {
  sceneExists: boolean;
  /** The probe input's `ndi_source_name`; `undefined` when the input is
   *  missing, `null` when it has no source name set. */
  inputSource: string | null | undefined;
  /** The input has a scene item in the probe scene. */
  inputInScene: boolean;
}

/** One provisioning step, in the order the gate runs them. */
export type ProbeStep = "create_scene" | "create_input" | "add_to_scene" | "repoint";

/**
 * The steps that make the probe scene show `wanted` (`SP-program`'s source
 * name): create the scene when it is missing; create the input in it when the
 * input is missing; else put the existing input into the scene when it is not
 * there, and re-point it when its source name differs. Nothing for a ready
 * probe, and never a removal.
 */
export function probeSteps(state: ProbeState, wanted: string): ProbeStep[] {
  const steps: ProbeStep[] = [];
  if (!state.sceneExists) steps.push("create_scene");
  if (state.inputSource === undefined) {
    steps.push("create_input");
    return steps;
  }
  if (!state.inputInScene) steps.push("add_to_scene");
  if (state.inputSource !== wanted) steps.push("repoint");
  return steps;
}

/**
 * Whether the take may record `SP-program`: it must carry the baseline
 * playlist the facade put on program. Never -1 ("OBS manuál"): cg OBS would
 * then record itself through `SP-program`, a video loop.
 */
export function programCarriesBaseline(source: number | null, baselinePid: number): boolean {
  return source !== null && source === baselinePid;
}

/**
 * Whether cg OBS's probe receiver is on `SP-program`: when the gate switched
 * cg OBS to the probe scene just now, `SP-program`'s polled receivers rose
 * above the count read before the switch (`before`; a negative "no reading"
 * counts as 0). When cg OBS already showed the probe scene, its receiver is
 * already in the count: any receiver will do.
 */
export function probeReceiverAttached(switchedNow: boolean, before: number, now: number): boolean {
  return switchedNow ? now > Math.max(before, 0) : now >= 1;
}
