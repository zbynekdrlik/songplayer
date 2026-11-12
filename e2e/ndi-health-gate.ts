/**
 * Pure decision logic for the post-deploy dark gate (#127), on `SP-program`
 * (#221 B4 step 6).
 *
 * Every consumer takes SongPlayer's PROGRAM now: the Presenter, strih and the
 * stream receive `SP-program` over NDI, the LED wall `SP-program-MAX` over
 * Spout, FOH its VBAN. cg OBS is only the NDI input "OBS manuál", and a
 * playlist has no NDI output of its own (#221 lane 3). The receiver that
 * must exist is
 * `SP-program`'s: `GET /api/v1/program` → `health.connections`, and the
 * server's own verdict `degraded_reason` ("no NDI receiver on SP-program").
 * #305 item 3: `SP-program` must also be DISCOVERABLE — listed by the
 * server's long-lived NDI source finder (`ndi.discoverable.state`
 * `discoverable`; PP 11.11.2026: the sender lived and counted frames while no
 * receiver could find it, so a receiver count alone is no proof).
 *
 * Separated from Playwright I/O so the decision is unit-testable without the
 * deployed win-resolume box (`ndi-health-gate.spec.ts`, the ubuntu mock
 * suite). The post-deploy suite polls `/api/v1/program` until the verdict is
 * `ok`.
 */

/** Subset of `GET /api/v1/program` this gate reads. */
export interface ProgramReceiverView {
  /** The source on program: a playlist id, `-1` for "OBS manuál", `null`
   *  before anything was selected. */
  source: number | null;
  health: { connections: number };
  /** The server's verdict (#221): `null` while SP-program has a receiver or
   *  nothing is on program. */
  degraded_reason?: string | null;
  /** #305 item 3: the server's discoverability check of `SP-program`. */
  ndi?: { discoverable?: { state: string } | null } | null;
}

/**
 * Receiver liveness for one NDI output:
 *  - `live`         — `connections > 0`, a receiver is subscribed.
 *  - `dark`         — `connections === 0`, nothing receives it.
 *  - `never_polled` — `connections < 0` (`-1`): no valid reading (the SDK's
 *    error value; SP-program reads 0, not -1, before its first poll, and the
 *    server names no `degraded_reason` then), so keep polling.
 */
export type OutputHealth = "live" | "dark" | "never_polled";

/** Classify an NDI sender's receiver connection count. */
export function classifyConnections(connections: number): OutputHealth {
  if (connections > 0) return "live";
  if (connections === 0) return "dark";
  return "never_polled"; // -1: no valid reading (the SDK's error value)
}

/**
 * #305 item 3: `ndi.discoverable.state` of a `GET /api/v1/program` answer —
 * `starting` / `discoverable` / `not_discoverable` / `unavailable` / `stale`,
 * or `missing` when there is no answer or it carries none. Only
 * `discoverable` passes a gate.
 */
export function discoverableState(program: ProgramReceiverView | null): string {
  return program?.ndi?.discoverable?.state ?? "missing";
}

/** The gate's verdict on `SP-program`. */
export interface ProgramReceiverVerdict {
  /** A source is on program and `SP-program` has a live receiver, the
   *  server names no degraded reason, and the network lists `SP-program`
   *  (#305 item 3). */
  ok: boolean;
  health: OutputHealth | "nothing_on_program";
  source: number | null;
  connections: number;
  degraded_reason: string | null;
  /** `ndi.discoverable.state` ([`discoverableState`]). */
  discoverable: string;
}

/**
 * `SP-program`'s receiver verdict. Nothing on program fails too: the box
 * always has a program source, and a program that carries nothing is not a
 * live output. A discoverability still `starting` (the server's 30 s grace)
 * fails as well, so the post-deploy poll keeps waiting for it.
 */
export function programReceiverVerdict(program: ProgramReceiverView): ProgramReceiverVerdict {
  const connections = program.health.connections;
  const degraded_reason = program.degraded_reason ?? null;
  const discoverable = discoverableState(program);
  const health =
    program.source === null ? "nothing_on_program" : classifyConnections(connections);
  return {
    ok: health === "live" && degraded_reason === null && discoverable === "discoverable",
    health,
    source: program.source,
    connections,
    degraded_reason,
    discoverable,
  };
}
