/**
 * #257: the post-deploy audio specs' reads of the audio fault ledger on the
 * box (`post-deploy-audio-asio.spec.ts`, `post-deploy-audio-outputs.spec.ts`):
 * open a window, then fail on any `loss` row in it. The decision itself is
 * the pure `audio-faults-gate.ts`.
 */

import { expect, type APIRequestContext } from "@playwright/test";
import {
  FAULTS_URL,
  LEDGER_SETTLE_MS,
  audioFaultFailures,
  audioFaultRisks,
  audioFaultSummary,
  type FaultsAnswer,
} from "./audio-faults-gate";

/** The ledger's answer over the last hour, read strictly. */
export async function readFaults(request: APIRequestContext): Promise<FaultsAnswer> {
  const resp = await request.get(FAULTS_URL, { timeout: 10_000 });
  expect(resp.status(), `GET ${FAULTS_URL}`).toBe(200);
  return (await resp.json()) as FaultsAnswer;
}

/** One bounded settle (`LEDGER_SETTLE_MS`): a fault reaches the table. */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, LEDGER_SETTLE_MS));
}

/** Open a window once the spec's outputs run: one settle (a fault from
 * before now is written by then, stamped before the window), then the
 * server's clock. */
export async function openFaultWindow(request: APIRequestContext): Promise<number> {
  await settle();
  return (await readFaults(request)).now_ms;
}

/** The server's clock now, or `null` when the ledger does not answer: the
 * fallback start a file takes in `beforeAll` (never a failure itself). */
export async function fileStartOrNull(request: APIRequestContext): Promise<number | null> {
  try {
    const resp = await request.get(FAULTS_URL, { timeout: 10_000 });
    return resp.status() === 200 ? ((await resp.json()) as FaultsAnswer).now_ms : null;
  } catch {
    return null;
  }
}

/** The file's last test: one settle, the answer logged (every path, the
 * risks), then no `loss` row from `windowStart` on. A window that never
 * opened (an earlier test failed, and the worker that restarted lost it)
 * still reads and logs the answer from `fallbackStart` (the `beforeAll`
 * start, else the answer's own start) before it fails: never a failure
 * without the ledger's evidence. */
export async function expectNoLossSince(
  request: APIRequestContext,
  windowStart: number | null,
  fallbackStart: number | null,
): Promise<void> {
  await settle();
  const answer = await readFaults(request);
  const start = windowStart ?? fallbackStart ?? answer.from_ms;
  console.log(`[#257 faults] window from ${new Date(start).toISOString()} (opened: ${windowStart !== null})`);
  for (const line of audioFaultSummary(answer)) console.log(`[#257 faults] ${line}`);
  for (const line of audioFaultRisks(answer, start)) console.log(`[#257 faults] risk: ${line}`);
  const failures = audioFaultFailures(answer, start);
  const opened = `the window opened (its outputs ran); losses from the fallback start: ${JSON.stringify(failures)}`;
  expect(windowStart, opened).not.toBeNull();
  expect(failures, "a loss row of any audio path in the window").toEqual([]);
}
