/**
 * Unit tests for the Gemini 3.5 Transcribe live gate `post-deploy-g35t.spec.ts`
 * applies on the box (#144). Runs in the ubuntu mock suite
 * (playwright.config.ts): no browser and no deployed box; these `test()`
 * blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { G35tProbe, g35tGateFailures } from "./g35t-gate";

/** A probe that answered: the shape the box sends on success (the live
 * probe of 5.10.2026: the first key answered, none refused). */
function answered(): G35tProbe {
  return {
    ok: true,
    model: "gemini-3.5-transcribe",
    key_index: 0,
    language_codes: ["en-US", "es-419"],
    word_count: 37,
    latency_ms: 6_412,
    error: null,
    refused_keys: [],
    clip: {
      youtube_id: "gq-4FVRr_ow",
      source: "isolated_vocal",
      start_ms: 12_345,
      duration_ms: 20_000,
    },
    sample: "Holy is the Lord God Almighty the earth",
  };
}

test.describe("g35t live gate (#144)", () => {
  test("a probe that heard words under the production request passes", () => {
    expect(g35tGateFailures(answered())).toEqual([]);
  });

  test("a rate-limited key before the answering one still passes: a 429 is quota, not a dead key", () => {
    const probe: G35tProbe = {
      ...answered(),
      key_index: 1,
      refused_keys: [
        { key_index: 0, rate_limited: true, error: "g35t_client upload: key refused status=429 body=quota" },
      ],
    };
    expect(g35tGateFailures(probe)).toEqual([]);
  });

  test("a dead or invalid key fails the gate even when a later key answered", () => {
    const probe: G35tProbe = {
      ...answered(),
      key_index: 2,
      refused_keys: [
        { key_index: 0, rate_limited: true, error: "g35t_client upload: key refused status=429 body=quota" },
        {
          key_index: 1,
          rate_limited: false,
          error: "g35t_client upload: key refused status=400 body=API key not valid",
        },
      ],
    };
    expect(g35tGateFailures(probe)).toEqual([
      "key 2 was refused, not rate-limited (a dead or invalid key): " +
        "g35t_client upload: key refused status=400 body=API key not valid",
    ]);
  });

  test("a refused key fails the gate with the API's message", () => {
    const probe: G35tProbe = {
      ...answered(),
      ok: false,
      key_index: 1,
      word_count: 0,
      error:
        "g35t_client: no key answered (2 tried); key 2 of 2: g35t_client upload: key refused status=403",
      refused_keys: [
        { key_index: 0, rate_limited: false, error: "g35t_client upload: key refused status=400" },
        { key_index: 1, rate_limited: false, error: "g35t_client upload: key refused status=403" },
      ],
      sample: "",
    };
    expect(g35tGateFailures(probe)).toEqual([
      "the probe failed: g35t_client: no key answered (2 tried); key 2 of 2: g35t_client upload: key refused status=403",
      "the model transcribed no words",
      "key 1 was refused, not rate-limited (a dead or invalid key): g35t_client upload: key refused status=400",
      "key 2 was refused, not rate-limited (a dead or invalid key): g35t_client upload: key refused status=403",
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
      'the build sends model "gemini-3-transcribe", not gemini-3.5-transcribe',
    ]);
    expect(g35tGateFailures({ ...answered(), language_codes: ["en-US"] })).toEqual([
      'the build hints ["en-US"], not ["en-US","es-419"]',
    ]);
    expect(g35tGateFailures({ ...answered(), language_codes: ["es-419", "en-US"] })).toEqual([
      'the build hints ["es-419","en-US"], not ["en-US","es-419"]',
    ]);
  });
});
