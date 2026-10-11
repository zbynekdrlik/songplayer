/**
 * #233 (the spawn finding, PP 11.10.2026): a child process SongPlayer
 * spawns must never disturb an ASIO output. Every SongPlayer-spawned yt-dlp
 * made PP's Waves SoundGrid driver ask for a resync (its output popped,
 * LV1 counted errors) while the driver was loaded in SongPlayer's process;
 * since then every ASIO output runs in the ASIO host process
 * `sp-audio-host`, which spawns nothing.
 *
 * The live gate (`post-deploy-audio-asio.spec.ts`) reads every gated ASIO
 * output's `resyncs` and `hard_recentres` (`outputs[i].asio`), makes
 * SongPlayer spawn a child (`POST /api/v1/youtube/probe`: one yt-dlp), lets
 * the host's ~2 Hz status catch up, and reads them again. These pure
 * helpers make the decision (unit-tested in the mock suite,
 * `asio-spawn-gate.spec.ts`).
 */

import { gatedAsioOutputs, type OutputStatus } from "./audio-outputs-gate";

/** How long the gate waits after the spawn before it reads again, ms: the
 * host reports every 500 ms, and a resync the spawn caused came within
 * ~2 s of it on PP. Absence cannot be polled for, so this is one bounded
 * settle, not a poll. */
export const SETTLE_AFTER_SPAWN_MS = 5000;

/** One gated ASIO output's disturbance counters, and the blocks it sent
 * (they only grow while the same host runs it: a host restart starts them
 * at 0 again, even when the other two read 0 before and after). */
export interface SpawnCounters {
  id: string;
  resyncs: number;
  hard_recentres: number;
  blocks_sent: number;
}

/** The probe's error when SongPlayer spawned nothing: yt-dlp is not
 * installed yet (`api/youtube_probe.rs`). */
export const PROBE_NOT_SPAWNED = "yt-dlp is not ready yet on this server";

/** Why the probe did not make SongPlayer spawn a child (empty = it did). */
export function probeSpawnFailures(answer: { error?: string | null }): string[] {
  return answer.error === PROBE_NOT_SPAWNED ? [`the probe spawned nothing: ${PROBE_NOT_SPAWNED}`] : [];
}

/** The counters of every gated ASIO output that runs (`gatedAsioOutputs`:
 * enabled, not waiting for its driver's clock). */
export function spawnCounters(list: OutputStatus[]): SpawnCounters[] {
  return gatedAsioOutputs(list)
    .filter((o) => o.state === "running" && o.asio !== undefined)
    .map((o) => ({
      id: o.id,
      resyncs: o.asio?.resyncs ?? 0,
      hard_recentres: o.asio?.hard_recentres ?? 0,
      blocks_sent: o.blocks_sent,
    }));
}

/** Why the spawn disturbed an output: its resyncs or hard re-centres grew,
 * its counters went back (the host restarted: the counters count since its
 * output was built), or it no longer runs. */
export function spawnDisturbances(before: SpawnCounters[], after: SpawnCounters[]): string[] {
  const f: string[] = [];
  for (const b of before) {
    const a = after.find((x) => x.id === b.id);
    if (a === undefined) {
      f.push(`${b.id} does not run after the spawn`);
      continue;
    }
    if (a.resyncs > b.resyncs) f.push(`${b.id}: ${a.resyncs - b.resyncs} driver resyncs after the spawn`);
    if (a.hard_recentres > b.hard_recentres) {
      f.push(`${b.id}: ${a.hard_recentres - b.hard_recentres} hard re-centres after the spawn`);
    }
    if (a.resyncs < b.resyncs || a.hard_recentres < b.hard_recentres || a.blocks_sent < b.blocks_sent) {
      f.push(`${b.id}: its counters went back (the ASIO host restarted?)`);
    }
  }
  return f;
}
