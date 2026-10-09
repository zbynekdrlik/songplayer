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
  device_resets: number;
  sender_backoffs: number;
  spout_name: string;
  adapter: string | null;
}

/** Boundaries that must go out between the two reads: one grid second (the
 *  program sends one per 30 fps slot, standby pairs included). */
export const MIN_BOUNDARIES = 30;

/** #223 follow-up: each Spout send leaves `MAX_SEND_LEAD` (12 ms) after the
 *  program offered its boundary, so the median send lands in
 *  [`SEND_PHASE_US`, `SEND_PHASE_US + SEND_PHASE_SLACK_US`] (the wait, then
 *  `SendTexture`'s copy): a median outside it means the sends are no longer
 *  paced and Arena's 60 Hz render stutters. */
export const SEND_PHASE_US = 12_000;
export const SEND_PHASE_SLACK_US = 1_500;

/**
 * Why the box fails the gate between two reads of `max` (`first` before
 * `second`); empty when it passes: the setting on, the 3840×2160 canvas under
 * `SP-program-MAX`, a hardware adapter (never the Basic Render Driver), the
 * thread `running`, at least `MIN_BOUNDARIES` more boundaries out, and in
 * between none coalesced (the thread kept up with the program's 30
 * boundaries a second: a 2-deep queue drops the oldest only when it falls
 * behind), none failed and no device lost; and the median send at its
 * constant phase (`SEND_PHASE_US`).
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
  const phase = second.send_at_us_p50;
  if (phase < SEND_PHASE_US || phase > SEND_PHASE_US + SEND_PHASE_SLACK_US) {
    failures.push(
      `the Spout sends leave ${phase} us after the offer (median), not at their ${SEND_PHASE_US} us phase`,
    );
  }
  return failures;
}
