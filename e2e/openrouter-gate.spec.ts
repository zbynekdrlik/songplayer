/**
 * Unit tests for the OpenRouter live gate `post-deploy-openrouter.spec.ts`
 * applies on the box (#321). Runs in the ubuntu mock suite: no browser and
 * no deployed box; these `test()` blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { OpenRouterProbe, openRouterGateFailures } from "./openrouter-gate";

function answered(): OpenRouterProbe {
  return {
    ok: true,
    model: "google/gemini-4-hydrogen",
    answered_model: "google/gemini-4-hydrogen-20261112",
    latency_ms: 1_204,
    error: null,
    sample: "ok",
  };
}

test.describe("OpenRouter live gate (#321)", () => {
  test("a probe the default model answered with text passes", () => {
    expect(openRouterGateFailures(answered())).toEqual([]);
  });

  test("a failed probe fails naming its reason", () => {
    const probe: OpenRouterProbe = {
      ...answered(),
      ok: false,
      answered_model: null,
      error: "OpenRouter refused (HTTP 401): No auth credentials found",
      sample: "",
    };
    expect(openRouterGateFailures(probe)).toEqual([
      "the probe failed: OpenRouter refused (HTTP 401): No auth credentials found",
    ]);
  });

  test("paid AI off or no key fails: SNV runs paid AI and holds the key", () => {
    const probe: OpenRouterProbe = {
      ...answered(),
      ok: false,
      error: "openrouter_api_key is not set: nothing was sent",
      sample: "",
    };
    expect(openRouterGateFailures(probe)).toHaveLength(1);
  });

  test("an answer with no text fails", () => {
    expect(openRouterGateFailures({ ...answered(), sample: "  " })).toEqual([
      "the model answered no text",
    ]);
  });

  test("a build that probes another model fails", () => {
    expect(openRouterGateFailures({ ...answered(), model: "openai/gpt-x" })).toEqual([
      'the build probes "openai/gpt-x", not the default google/gemini-4-hydrogen',
    ]);
  });
});
