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
 * served line (the isolated vocal the worker uploads when on disk, else its
 * vocal stem, else the mix) and sends it through the worker's call: the same
 * upload, request body, language hint and key rotation. One short paid call
 * per deploy, like the metadata probe. The keys refused before the answering
 * one are logged (`refused_keys`); one refused for any reason but a 429 (a
 * dead or invalid key) fails the gate even when a later key answered.
 *
 * API-level on purpose: the probe has no dashboard surface. The decision is
 * the pure `g35t-gate.ts`, unit-tested in the mock suite.
 */

import { test, expect } from "@playwright/test";
import { G35tProbe, g35tGateFailures } from "./g35t-gate";

test.describe("Gemini 3.5 Transcribe live gate (#144)", () => {
  test("the box transcribes a real clip under the worker's request", async ({ request }) => {
    // The probe bounds the transcription at 180 s (`g35t_probe::PROBE_TIMEOUT`);
    // a 20 s clip answers in seconds. The ffmpeg wait comes first.
    test.setTimeout(300_000);

    // The probe cuts the clip with the app's ffmpeg, which the tools manager
    // makes ready after a (re)start: wait for it rather than rely on the specs
    // that happen to run before this one for that. A refused or slow status
    // read (the app still starting) is "not yet", never a failure: expect.poll
    // does not retry a generator that throws.
    await expect
      .poll(
        async () => {
          try {
            const status = await request.get("/api/v1/status", { timeout: 10_000 });
            if (status.status() !== 200) return false;
            const body = (await status.json()) as { tools?: { ffmpeg_available?: boolean } };
            return body.tools?.ffmpeg_available === true;
          } catch {
            return false;
          }
        },
        { message: "the app's ffmpeg is ready (tools.ffmpeg_available)", timeout: 60_000 },
      )
      .toBe(true);

    const resp = await request.post("/api/v1/lyrics/g35t/probe", { timeout: 220_000 });
    expect(resp.status(), "POST /api/v1/lyrics/g35t/probe").toBe(200);
    const probe = (await resp.json()) as G35tProbe;
    console.log(`[#144 g35t probe] ${JSON.stringify(probe)}`);
    // Every refused key is logged. A 429 passes (quota, not a dead key); any
    // other refusal fails the gate below (#144 comment 5999711400): the key
    // is dead, invalid, or not allowed this model or API — the logged reason
    // says which, so read it before pruning the key.
    for (const refused of probe.refused_keys) {
      const kind = refused.rate_limited
        ? "rate-limited (429)"
        : "REFUSED (a 403/400 key refusal, not a 429: read the reason)";
      console.log(`[#144 g35t probe] key ${refused.key_index + 1} ${kind}: ${refused.error}`);
    }

    expect(
      g35tGateFailures(probe),
      "the g35t probe must hear words, with no dead or invalid key",
    ).toEqual([]);
  });
});
