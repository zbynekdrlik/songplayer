/**
 * #144 post-deploy gate: Gemini 3.5 Transcribe must transcribe a real clip
 * from the box under the lyrics worker's own request.
 *
 * Every lyric the wall shows starts as a Gemini 3.5 Transcribe (g35t)
 * transcript: the base tier groups it into lines, the ★ tier verifies its
 * forced alignment against it. No post-deploy check ever sent a g35t request,
 * so a dead or refused Gemini key, a renamed model or a request field the API
 * refuses (the `language_codes` hint #144 added, verified only against the
 * docs) stayed invisible with CI green while every new transcript failed. The
 * owner's rule (29.9.2026): every external provider and model gets a live
 * post-deploy check.
 *
 * `POST /api/v1/lyrics/g35t/probe` cuts 20 s of a cached song from its first
 * served line (its isolated vocal stem when on disk) and sends it through the
 * worker's call: the same upload, request body, language hint and key
 * rotation. One short paid call per deploy, like the metadata probe.
 *
 * API-level on purpose: the probe has no dashboard surface. The decision is
 * the pure `g35t-gate.ts`, unit-tested in the mock suite.
 */

import { test, expect } from "@playwright/test";
import { G35tProbe, g35tGateFailures } from "./g35t-gate";

test.describe("Gemini 3.5 Transcribe live gate (#144)", () => {
  test("the box transcribes a real clip under the worker's request", async ({ request }) => {
    // The probe bounds the transcription at 180 s (`g35t_probe::PROBE_TIMEOUT`);
    // a 20 s clip answers in seconds.
    test.setTimeout(240_000);

    const resp = await request.post("/api/v1/lyrics/g35t/probe", { timeout: 220_000 });
    expect(resp.status(), "POST /api/v1/lyrics/g35t/probe").toBe(200);
    const probe = (await resp.json()) as G35tProbe;
    console.log(`[#144 g35t probe] ${JSON.stringify(probe)}`);

    expect(g35tGateFailures(probe), "the g35t probe must hear words").toEqual([]);
  });
});
