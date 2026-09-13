/**
 * Pure decision logic for the post-deploy NDI dark-wall gate (#127).
 *
 * Separated from Playwright I/O so the "is any on-program output dark?"
 * decision is unit-testable without the deployed win-resolume box. The
 * post-deploy suite feeds this the on-program playlist ids
 * (`/api/v1/status.active_playlist_ids`) and the `/api/v1/ndi/health` array,
 * then polls until every on-program output has a live receiver.
 */

/** Subset of a `/api/v1/ndi/health` snapshot this gate reads. */
export interface HealthSnapshot {
  playlist_id: number;
  ndi_name: string;
  connections: number;
}

/**
 * Receiver liveness for one output:
 *  - `live`         — `connections > 0`, a receiver is subscribed.
 *  - `dark`         — `connections === 0`, Playing but nothing receives (the
 *                     #127 dark wall).
 *  - `never_polled` — `connections < 0` (`-1`), the heartbeat has not run yet.
 */
export type OutputHealth = "live" | "dark" | "never_polled";

/** Classify a pipeline's NDI receiver connection count. */
export function classifyConnections(connections: number): OutputHealth {
  // RED stub — every count reads as live; the real classifier lands in GREEN.
  void connections;
  return "live";
}

/** An on-program output that does not (yet) have a live receiver. */
export interface UnhealthyOutput {
  playlist_id: number;
  ndi_name: string;
  connections: number;
  health: OutputHealth;
}

/**
 * Given the on-program playlist ids and the `/api/v1/ndi/health` array, return
 * the on-program outputs that do NOT yet have a live receiver.
 *
 * An on-program playlist with no health snapshot at all is reported as
 * `never_polled`. An empty result means every on-program output has
 * `connections > 0` — the gate passes. Off-program pipelines are ignored on
 * purpose: a non-program pipeline at `connections === 0` is normal and must
 * not fail the gate.
 */
export function unhealthyOnProgramOutputs(
  activePlaylistIds: number[],
  health: HealthSnapshot[],
): UnhealthyOutput[] {
  // RED stub — never flags anything; the real gate lands in GREEN.
  void activePlaylistIds;
  void health;
  return [];
}
