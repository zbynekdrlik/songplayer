/**
 * Evidence files for the post-deploy A/V gate (#147).
 *
 * A take that was analysed and did not pass keeps its OBS recording (and the
 * auto-remux sibling) plus the analysis output in the Playwright output dir.
 * The CI upload step "Upload post-deploy Playwright report on failure"
 * uploads `e2e/test-results/`, so the recording can be analysed offline.
 *
 * Nothing here throws. It runs in the take's `finally`, and an exception
 * there would mask the analysis error that got us there. Every failure is
 * logged loudly with console.error instead.
 *
 * Unit-tested in the ubuntu mock suite (`av-sync-evidence.spec.ts`) on a
 * temp dir.
 */

import * as fs from "fs";
import * as path from "path";
import { evidenceName } from "./av-sync-gate";

/** Where one failing take's evidence goes. */
export interface Evidence {
  dir: string;
  take: number;
}

/** Busy-retry budget for copying ONE evidence file (OBS may still hold it). */
export const EVIDENCE_COPY_MS = 5_000;
/** How long a file's size must hold still before it counts as complete. */
export const SETTLE_MS = 1_000;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** True once `file` has a non-zero size that held for `SETTLE_MS` (a remux still writing it grows). */
export async function settled(file: string, deadline: number): Promise<boolean> {
  let last = -1;
  while (Date.now() < deadline) {
    let size = -1;
    try {
      size = fs.statSync(file).size;
    } catch {
      // not there (yet): keep polling until the deadline
    }
    if (size > 0 && size === last) return true;
    last = size;
    await sleep(SETTLE_MS);
  }
  return false;
}

/**
 * Copy the recording files into the evidence dir as `take<N>-<name>`.
 * `files[0]` is the stopped (closed) recording. Any further file is the
 * auto-remux sibling, which OBS writes after StopRecord: it is copied only
 * once its size settled before `siblingDeadline`. Retries while a file is
 * still held.
 */
export async function keepRecording(
  files: string[],
  ev: Evidence,
  siblingDeadline: number,
): Promise<void> {
  try {
    fs.mkdirSync(ev.dir, { recursive: true });
  } catch (e) {
    console.error(`A/V gate: could not create the evidence dir ${ev.dir}: ${e}`);
    return;
  }
  for (const [i, f] of files.entries()) {
    if (i > 0 && !(await settled(f, siblingDeadline))) {
      console.error(`A/V gate: ${f} did not settle, not kept as evidence`);
      continue;
    }
    const dest = path.join(ev.dir, evidenceName(ev.take, f));
    const deadline = Date.now() + EVIDENCE_COPY_MS;
    for (;;) {
      try {
        fs.copyFileSync(f, dest);
        console.log(`A/V gate: kept ${f} as CI evidence -> ${dest}`);
        break;
      } catch (e) {
        if (Date.now() >= deadline) {
          console.error(`A/V gate: could not keep ${f} as evidence: ${e}`);
          break;
        }
        await sleep(500);
      }
    }
  }
}

/** Write one analysis text (JSON / stderr / error) as `take<N>-<name>`. */
export function keepText(ev: Evidence, name: string, text: string): void {
  try {
    fs.mkdirSync(ev.dir, { recursive: true });
    fs.writeFileSync(path.join(ev.dir, evidenceName(ev.take, name)), text);
  } catch (e) {
    console.error(`A/V gate: could not write evidence ${name}: ${e}`);
  }
}
