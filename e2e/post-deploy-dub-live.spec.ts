/**
 * #232 unit 2 post-deploy gate: Gemini Live Translate must dub a real clip
 * from the box under the dub worker's own child call.
 *
 * The dabing feature streams a whole video through ONE continuous Gemini Live
 * Translate session. No post-deploy check ever opened such a session, so a
 * refused key, a retired model or a session protocol the API stopped accepting
 * stayed invisible with CI green while every new dub failed. The owner's rule
 * (29.9.2026): every external provider and model gets a live post-deploy check.
 *
 * `POST /api/v1/dabing/probe` cuts 20 s of a downloaded song (from its first
 * served lyrics line, else from 30 s; its vocal stem when ready, else its
 * audio) and runs the dub worker's child on it with the `dub_model` /
 * `dub_voice` settings and the first Gemini key, in a temp dir, NOT under the
 * heavy slot. One short paid session per deploy (~20 s of Live Translate).
 *
 * SNV only: PP's subset (`post-deploy-pp.config.ts`) lists its specs and
 * leaves this one out; PP has paid AI off (the probe would answer 409).
 *
 * API-level on purpose: the probe has no dashboard surface. The decision is
 * the pure `dub-live-gate.ts`, unit-tested in the mock suite.
 */

import { test, expect } from "@playwright/test";
import { DubProbe, dubLiveGateFailures } from "./dub-live-gate";

test.describe("Gemini Live Translate live gate (#232 unit 2)", () => {
  test("the box dubs a real clip under the dub worker's own session", async ({ request }) => {
    // The probe bounds the cut at 15 s, the dub at 240 s and its read-back at
    // 15 s (`dabing::probe`); the request timeout sits above all three, the
    // test's above the ffmpeg wait plus the request.
    test.setTimeout(420_000);

    // The probe cuts the clip with the app's ffmpeg, ready only after the
    // tools manager started: wait for it. A refused or slow status read (the
    // app still starting) is "not yet", never a failure.
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

    const resp = await request.post("/api/v1/dabing/probe", { timeout: 300_000 });
    const probe = (await resp.json()) as DubProbe;
    console.log(`[#232 dub probe] HTTP ${resp.status()} ${JSON.stringify(probe)}`);

    expect(
      dubLiveGateFailures(resp.status(), probe),
      "the Live Translate probe must dub the clip with sound and a Slovak line",
    ).toEqual([]);
  });
});
