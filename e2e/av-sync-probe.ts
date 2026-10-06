/**
 * The A/V gate's probe scene in cg OBS (#221 lane 3) — the pure decisions.
 *
 * A playlist has no NDI output of its own any more: every consumer takes
 * SongPlayer's PROGRAM (`SP-program`). The only recorder on win-resolume is cg
 * OBS (`StartRecord` records ITS program), so the post-deploy A/V gate records
 * `SP-program` through a dedicated cg OBS scene, [`AV_PROBE_SCENE`], holding
 * ONE DistroAV input, [`AV_PROBE_INPUT`]. The gate provisions it itself
 * (design-question 6004634711, option 1): created when missing, put into the
 * scene when it is not there, never deleted (a receiving DistroAV input does
 * not delete reliably, `obs-ndi-health.md`). It is not an `sp-*` name, so it
 * is never a playlist scene in SongPlayer's catalog, and `pickBaselineScene`
 * never picks it.
 *
 * **The probe is IDLE outside the take: its `ndi_source_name` is `""`.**
 * DistroAV's `ndi_behavior` 0 is KEEP_ACTIVE (and cg OBS's genlock build
 * forces it on every input with `genlock_fifo` on, the default:
 * `camera-box/vendor/distroav/src/ndi-source.cpp`), so an input keeps its
 * receiver whether it is shown or not; only an empty source name stops the
 * receiver thread. A probe left pointed at `SP-program` would hold a receiver
 * on it forever, and the post-deploy gate "SP-program has a live NDI
 * receiver" could no longer see the Presenter, strih and the stream go.
 * So: idle at provisioning, SP-program's receivers read once settled, the
 * probe pointed at `SP-program` only for the take (its receivers must rise),
 * idle again in `afterAll`.
 *
 * The take runs only while `SP-program` carries the baseline playlist: with
 * "OBS manuál" (-1) on program, cg OBS would record itself through
 * `SP-program` (a video loop). The glue is in `post-deploy-av-sync.spec.ts`
 * and `obs-driver.ts`; these helpers are unit-tested in the ubuntu mock suite
 * (`av-sync-probe.spec.ts`).
 */

/** The cg OBS scene the A/V gate records `SP-program` through. */
export const AV_PROBE_SCENE = "A/V gate (SP-program)";

/** Its one input: a DistroAV NDI receiver, of `SP-program` during the take. */
export const AV_PROBE_INPUT = "A/V gate SP-program";

/** SongPlayer's program NDI stream name (`program_output::PROGRAM_NDI_NAME`). */
export const PROGRAM_NDI_NAME = "SP-program";

/** DistroAV's NDI input kind. */
export const NDI_INPUT_KIND = "ndi_source";

/**
 * Settings the probe always carries over its template's (set at creation AND
 * on every run), so the recording is the program's picture AND audio through
 * the receive path camera-box certified, whatever input it copied: the
 * genlock FIFO on (`genlock_fifo`: DistroAV then forces source-timecode sync,
 * KEEP_ACTIVE, the highest bandwidth and normal latency), its audio on
 * (`ndi_audio`), not the low-bandwidth monitor (`genlock_monitor`), no
 * measurement burn (`genlock_burn`: a QR on the recording), the highest
 * bandwidth (`ndi_bw_mode` 0; 2 would be audio only).
 */
export const PROBE_FIXED_SETTINGS: Readonly<Record<string, unknown>> = {
  genlock_fifo: true,
  ndi_audio: true,
  genlock_monitor: false,
  genlock_burn: false,
  ndi_bw_mode: 0,
};

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
 * The cg OBS NDI input whose settings the probe copies: `sp-slow_video` (the
 * baseline's), else any `sp-*_video`, else the first one; never the probe
 * itself; null when cg OBS has none.
 */
export function pickTemplateInput(inputs: string[]): string | null {
  const others = inputs.filter((n) => n !== AV_PROBE_INPUT);
  if (others.includes("sp-slow_video")) return "sp-slow_video";
  return others.find((n) => /^sp-.+_video$/.test(n)) ?? others[0] ?? null;
}

/**
 * The settings the probe input is created with: its template's (the input
 * kind's genlock / sync knobs), [`PROBE_FIXED_SETTINGS`] over them, and the
 * source name (`""`: created idle). With no template, only the fixed ones.
 */
export function probeInputSettings(
  template: Record<string, unknown> | null,
  sourceName: string,
): Record<string, unknown> {
  return { ...(template ?? {}), ...PROBE_FIXED_SETTINGS, ndi_source_name: sourceName };
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
export type ProbeStep = "create_scene" | "create_input" | "add_to_scene" | "reset";

/**
 * The steps that leave the probe scene ready and the probe IDLE: create the
 * scene when it is missing; create the input in it (idle, with
 * [`PROBE_FIXED_SETTINGS`]) when the input is missing; else put the existing
 * input into the scene when it is not there, and RESET it on every run
 * ([`probeIdleSettings`]: its fixed settings again, and idle — a run that died
 * mid-take may have left it pointed, a hand edit may have changed one of
 * its settings; only its settings: a hidden scene item or a muted input is
 * not undone, the take then fails as unmeasurable). Never a removal.
 */
export function probeSteps(state: ProbeState): ProbeStep[] {
  const steps: ProbeStep[] = [];
  if (!state.sceneExists) steps.push("create_scene");
  if (state.inputSource === undefined) {
    steps.push("create_input");
    return steps;
  }
  if (!state.inputInScene) steps.push("add_to_scene");
  steps.push("reset");
  return steps;
}

/** What an existing probe is reset to on every run: its fixed settings, idle. */
export function probeIdleSettings(): Record<string, unknown> {
  return { ...PROBE_FIXED_SETTINGS, ndi_source_name: "" };
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
 * Whether `SP-program`'s receiver count has settled: two reads a poll apart
 * agree (`previous` null before the second read). The idle probe's receiver
 * leaves asynchronously, and the count is polled about once a second.
 */
export function receiversSettled(previous: number | null, current: number): boolean {
  return previous !== null && previous === current;
}

/**
 * Whether cg OBS's probe receiver is on `SP-program`: its polled receivers
 * rose above the settled count read while the probe was idle (`before`; a
 * negative "no reading" counts as 0).
 */
export function probeReceiverAttached(before: number, now: number): boolean {
  return now > Math.max(before, 0);
}
