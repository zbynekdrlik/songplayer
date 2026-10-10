/**
 * The `SP-program-MAX` post-deploy gate's decision (#223 S2), pure so the
 * mock suite tests it without a box (`max-gate.spec.ts`); the box read is
 * `post-deploy-max.spec.ts`. #239: and the decision for the `SP-program`
 * Spout sender (1920×1080) the same thread runs next to MAX
 * (`fhdGateFailures`).
 */

/** #239: `GET /api/v1/program` → `max.fhd` (sp-server `FhdStatus`). */
export interface FhdStatus {
  enabled: boolean;
  state: string;
  reason: string | null;
  spout_name: string;
  listed_width: number;
  listed_height: number;
  submitted: number;
  failed: number;
  sender_backoffs: number;
  upload_us_p99: number;
  draw_us_p99: number;
  send_us_p99: number;
}

/** `GET /api/v1/program` → `max` (sp-server `MaxStatus`). */
export interface MaxStatus {
  enabled: boolean;
  state: string;
  width: number;
  height: number;
  submitted: number;
  coalesced: number;
  failed: number;
  upload_us_p99: number;
  draw_us_p99: number;
  send_us_p99: number;
  send_at_us_p50: number;
  send_at_us_p99: number;
  send_at_us_max: number;
  send_late: number;
  vblank_output: string | null;
  vblank_tracking: boolean;
  /** #243: `measuring` | `ticking` | `not_ticking`, `null` with no tracker. */
  vblank_state: string | null;
  vblank_period_ns: number;
  vblank_phase_us: number;
  send_off_grid: number;
  send_phase_us_p50: number;
  send_phase_us_p99: number;
  slot_repicks: number;
  device_resets: number;
  sender_backoffs: number;
  spout_name: string;
  adapter: string | null;
  fhd: FhdStatus;
}

/** Boundaries that must go out between the two reads: one grid second (the
 *  program sends one per 30 fps slot, standby pairs included). */
export const MIN_BOUNDARIES = 30;

/** #223 follow-up: each Spout send starts in a slot of the display refresh
 *  Arena renders in (the primary display's: DWM's clock),
 *  `vblank_phase_us` after its vblank (`program_max_vblank.rs`), so the
 *  median start lies in [phase, phase + `SEND_PHASE_SLACK_US`]: outside it
 *  the sends are not paced on Arena's rhythm and the wall stutters. */
export const SEND_PHASE_SLACK_US = 1_500;

/** A slot is picked anew only when the drift between the display's clock and
 *  SongPlayer's carries the lead out of its window (hours apart): more than
 *  one between two reads seconds apart means the pick flaps. */
export const MAX_SLOT_REPICKS = 1;

/**
 * Why the box fails the gate between two reads of `max` (`first` before
 * `second`); empty when it passes: the setting on, the 3840×2160 canvas under
 * `SP-program-MAX`, a hardware adapter (never the Basic Render Driver), the
 * thread `running`, at least `MIN_BOUNDARIES` more boundaries out, and in
 * between none coalesced (the thread kept up with the program's 30
 * boundaries a second: a 2-deep queue drops the oldest only when it falls
 * behind), none failed and no device lost; and every boundary in between
 * sent on the display's refresh grid, the median at the phase setting,
 * the slot picked anew at most `MAX_SLOT_REPICKS` times.
 */
export function maxGateFailures(first: MaxStatus, second: MaxStatus): string[] {
  const failures: string[] = [];
  if (!second.enabled) failures.push("program_max_enabled is off");
  if (second.width !== 3840 || second.height !== 2160) {
    failures.push(`the canvas is ${second.width}x${second.height}, not 3840x2160`);
  }
  if (second.spout_name !== "SP-program-MAX") {
    failures.push(`the Spout name is ${second.spout_name}`);
  }
  if (second.adapter === null) {
    failures.push("no compositor was built (no adapter)");
  } else if (/basic render/i.test(second.adapter)) {
    failures.push(`it composes on ${second.adapter}, not a hardware GPU`);
  }
  if (second.state !== "running") failures.push(`the state is ${second.state}`);
  const sent = second.submitted - first.submitted;
  if (sent < MIN_BOUNDARIES) {
    failures.push(`only ${sent} boundaries went out (at least ${MIN_BOUNDARIES})`);
  }
  const coalesced = second.coalesced - first.coalesced;
  if (coalesced > 0) {
    failures.push(`${coalesced} boundaries coalesced (MAX fell behind the program)`);
  }
  const failed = second.failed - first.failed;
  if (failed > 0) failures.push(`${failed} boundaries failed`);
  const lost = second.device_resets - first.device_resets;
  if (lost > 0) failures.push(`the device was lost ${lost} times`);
  if (second.vblank_output === null) {
    failures.push("no display output paces the sends (no vblank tracker)");
  }
  const offGrid = second.send_off_grid - first.send_off_grid;
  if (offGrid > 0 || !second.vblank_tracking) {
    failures.push(`${offGrid} boundaries were sent off the display's refresh grid`);
  }
  if (second.vblank_state === "not_ticking") {
    failures.push(
      `the display output ${second.vblank_output} does not tick (vblank_state not_ticking): WaitForVBlank does not wait`,
    );
  }
  const phase = second.send_phase_us_p50;
  const want = second.vblank_phase_us;
  if (phase < want || phase > want + SEND_PHASE_SLACK_US) {
    failures.push(
      `the Spout sends start ${phase} us after the vblank (median), not at the ${want} us phase`,
    );
  }
  const repicks = second.slot_repicks - first.slot_repicks;
  if (repicks > MAX_SLOT_REPICKS) {
    failures.push(`the send slot was picked anew ${repicks} times between the reads`);
  }
  return failures;
}

/**
 * #239: why the box fails the `SP-program` (1920×1080) Spout gate between
 * two reads of `max.fhd` (`first` before `second`); empty when it passes:
 * its setting on and no switch keeping it off, the sender `SP-program`
 * `running`, Spout's registry listing it at 1920×1080 (the thread's own
 * read, as a receiver reads it), at least `MIN_BOUNDARIES` more boundaries
 * out, and in between none failed and no sender refused.
 */
export function fhdGateFailures(first: FhdStatus, second: FhdStatus): string[] {
  const failures: string[] = [];
  if (!second.enabled) failures.push("program_spout_fhd_enabled is off");
  if (second.reason !== null) failures.push(`the SP-program sender is off: ${second.reason}`);
  if (second.spout_name !== "SP-program") {
    failures.push(`the FHD Spout name is ${second.spout_name}`);
  }
  if (second.state !== "running") failures.push(`the SP-program sender is ${second.state}`);
  if (second.listed_width !== 1920 || second.listed_height !== 1080) {
    failures.push(
      `Spout lists SP-program at ${second.listed_width}x${second.listed_height}, not 1920x1080`,
    );
  }
  const sent = second.submitted - first.submitted;
  if (sent < MIN_BOUNDARIES) {
    failures.push(`only ${sent} SP-program boundaries went out (at least ${MIN_BOUNDARIES})`);
  }
  const failed = second.failed - first.failed;
  if (failed > 0) failures.push(`${failed} SP-program boundaries failed`);
  const refused = second.sender_backoffs - first.sender_backoffs;
  if (refused > 0) failures.push(`the SP-program sender was refused ${refused} times`);
  return failures;
}
