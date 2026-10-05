/**
 * Unit tests for the Gemini 3.5 Transcribe live gate `post-deploy-g35t.spec.ts`
 * applies on the box (#144). Runs in the ubuntu mock suite
 * (playwright.config.ts): no browser and no deployed box; these `test()`
 * blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { G35tProbe, g35tGateFailures } from "./g35t-gate";

/** A probe that answered: the shape the box sends on success. */
function answered(): G35tProbe {
  return {
    ok: true,
    model: "gemini-3.5-transcribe",
    key_index: 1,
    language_codes: ["en-US", "es-419"],
    word_count: 37,
    latency_ms: 6_412,
    error: null,
    clip: { youtube_id: "gq-4FVRr_ow", source: "vocals", start_ms: 12_345, duration_ms: 20_000 },
    sample: "Holy is the Lord God Almighty the earth",
  };
}

test.describe("g35t live gate (#144)", () => {
  test("a probe that heard words under the production request passes", () => {
    expect(g35tGateFailures(answered())).toEqual([]);
  });

  test("a refused key fails the gate with the API's message", () => {
    const probe: G35tProbe = {
      ...answered(),
      ok: false,
      key_index: 1,
      word_count: 0,
      error: "g35t_client: all 2 keys refused; key 2 of 2: g35t_client upload: key refused status=403",
      sample: "",
    };
    expect(g35tGateFailures(probe)).toEqual([
      "the probe failed: g35t_client: all 2 keys refused; key 2 of 2: g35t_client upload: key refused status=403",
      "the model transcribed no words",
    ]);
  });

  test("an answer with no words fails the gate even if marked ok", () => {
    expect(g35tGateFailures({ ...answered(), word_count: 0 })).toEqual([
      "the model transcribed no words",
    ]);
  });

  test("a failed probe without an error text still fails", () => {
    expect(g35tGateFailures({ ...answered(), ok: false })).toEqual([
      "the probe failed: (no error text)",
    ]);
  });

  test("another model or language hint than the worker's fails the gate", () => {
    expect(g35tGateFailures({ ...answered(), model: "gemini-3-transcribe" })).toEqual([
      'the request named model "gemini-3-transcribe", not gemini-3.5-transcribe',
    ]);
    expect(g35tGateFailures({ ...answered(), language_codes: ["en-US"] })).toEqual([
      'the request hinted ["en-US"], not ["en-US","es-419"]',
    ]);
    expect(g35tGateFailures({ ...answered(), language_codes: ["es-419", "en-US"] })).toEqual([
      'the request hinted ["es-419","en-US"], not ["en-US","es-419"]',
    ]);
  });
});
