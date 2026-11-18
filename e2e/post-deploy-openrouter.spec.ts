/**
 * #321 post-deploy gate: the OpenRouter text path must answer from the box.
 *
 * Every text AI task (the EN→SK translation, the register check, the
 * metadata, the description extraction, the scraped-lyrics cleanup, the
 * Spotify search) can be switched to OpenRouter per task in Nastavenia. A
 * dead or refused `openrouter_api_key`, a model OpenRouter no longer lists
 * or a request it refuses would stay invisible with CI green until a task
 * is switched. The owner's rule (29.9.2026): every external provider and
 * model gets a live post-deploy check.
 *
 * `POST /api/v1/ai/openrouter/probe {}` sends one tiny completion with the
 * default model through the tasks' own call (the paid-AI switch, the key
 * read live from the database, the client and its retries). One very
 * small paid call per deploy. SNV only: PP's paid AI is off (its subset
 * config does not list this spec).
 *
 * API-level on purpose: the probe has no dashboard surface. The decision is
 * the pure `openrouter-gate.ts`, unit-tested in the mock suite.
 */

import { test, expect } from "@playwright/test";
import { OpenRouterProbe, openRouterGateFailures } from "./openrouter-gate";

test.describe("OpenRouter live gate (#321)", () => {
  test("the box's OpenRouter path answers with the default model", async ({ request }) => {
    // The probe bounds one request at 60 s (`openrouter_probe::PROBE_TIMEOUT_S`)
    // and retries a 429 / 5xx after the policy's waits: its `Retry-After` up
    // to 120 s, else 5 / 20 / 60 s. Worst case 4 × 60 s + 3 × 120 s = 600 s,
    // so a slow refusal fails with the probe's own named error, never as a
    // Playwright timeout.
    test.setTimeout(700_000);
    const resp = await request.post("/api/v1/ai/openrouter/probe", {
      data: {},
      timeout: 660_000,
    });
    expect(resp.status(), "POST /api/v1/ai/openrouter/probe").toBe(200);
    const probe = (await resp.json()) as OpenRouterProbe;
    console.log(`[#321 openrouter probe] ${JSON.stringify(probe)}`);
    expect(
      openRouterGateFailures(probe),
      "the OpenRouter probe must answer with text from the default model",
    ).toEqual([]);
  });
});
