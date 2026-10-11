/**
 * #233 (the spawn finding, PP 11.10.2026): a child process SongPlayer
 * spawns must never disturb an ASIO output. Every SongPlayer-spawned yt-dlp
 * made PP's Waves SoundGrid driver ask for a resync (its output popped,
 * LV1 counted errors). The cause, proven A/B/A on PP: Windows Defender's
 * real-time scan of the processes SongPlayer launches (yt-dlp, deno) bursts
 * the CPUs the driver's DPCs run on. Moving the driver into its own process
 * did not help (tried and falsified on PP). The protection is the
 * production environment itself (LTSC, Defender real-time off, firewall
 * off), checked by SongPlayer and gated by `post-deploy-environment.spec.ts`;
 * this gate is the regression check that the box really stays quiet.
 *
 * The live gate (`post-deploy-audio-asio.spec.ts`) reads every gated ASIO
 * output's `resyncs` and `hard_recentres` (`outputs[i].asio`), makes
 * SongPlayer spawn a child (`POST /api/v1/youtube/probe`: one yt-dlp), lets
 * a resync the spawn caused reach the counters, and reads them again. These
 * pure helpers make the decision (unit-tested in the mock suite,
 * `asio-spawn-gate.spec.ts`).
 */

import { gatedAsioOutputs, type OutputStatus } from "./audio-outputs-gate";

/** How long the gate waits after the spawn before it reads again, ms: a
 * resync the spawn caused came within ~2 s of it on PP (the worker counts it
 * at its next 10 ms poll). Absence cannot be polled for, so this is one
 * bounded settle, not a poll. */
export const SETTLE_AFTER_SPAWN_MS = 5000;

/** One gated ASIO output's disturbance counters. */
export interface SpawnCounters {
  id: string;
  resyncs: number;
  hard_recentres: number;
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
    }));
}

/** Why the spawn disturbed an output: its resyncs or hard re-centres grew,
 * its counters went back (the output was rebuilt or SongPlayer restarted:
 * the counters count since the output was built), or it no longer runs. */
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
    if (a.resyncs < b.resyncs || a.hard_recentres < b.hard_recentres) {
      f.push(`${b.id}: its counters went back (the output was rebuilt?)`);
    }
  }
  return f;
}
