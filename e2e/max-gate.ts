/**
 * The `SP-program-MAX` post-deploy gate's decision (#223 S2), pure so the
 * mock suite tests it without a box (`max-gate.spec.ts`); the box read is
 * `post-deploy-max.spec.ts`.
 */

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
