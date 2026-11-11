/**
 * Unit tests for the Gemini Live Translate live gate
 * `post-deploy-dub-live.spec.ts` applies on the box (#232 unit 2). Runs in the
 * ubuntu mock suite (playwright.config.ts): no browser and no deployed box;
 * these `test()` blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { DubProbe, DUB_MIN_RMS_DBFS, dubLiveGateFailures } from "./dub-live-gate";

/** A probe that dubbed its clip: the shape the box sends on success. */
function dubbed(): DubProbe {
  return {
    ok: true,
    model: "gemini-3.5-live-translate-preview",
    voice: "speaker",
    youtube_id: "gq-4FVRr_ow",
    start_ms: 12_345,
    clip_ms: 20_000,
    source: "vocal_stem",
    elapsed_ms: 41_200,
    dub_ms: 21_480,
    dub_rms_dbfs: -21.7,
    sk_lines: 3,
    sample: "Svätý je Pán, Boh všemohúci.",
    session: {
      connections: 1,
      reconnects: 0,
      output_to_input_ratio: 1.01,
      max_voiced_gap_s: 2.1,
      latency_ms: 3_100,
      drain_end_reason: "quiet",
    },
    error: null,
  };
}

test.describe("Live Translate live gate (#232 unit 2)", () => {
  test("a dub with sound and a Slovak line passes", () => {
    expect(dubLiveGateFailures(200, dubbed())).toEqual([]);
  });

  test("the floor itself still holds sound", () => {
    expect(dubLiveGateFailures(200, { ...dubbed(), dub_rms_dbfs: DUB_MIN_RMS_DBFS })).toEqual([]);
  });

  test("a refused probe fails with its reason, whatever else it says", () => {
    const refused: DubProbe = {
      ...dubbed(),
      ok: false,
      youtube_id: null,
      start_ms: null,
      clip_ms: 0,
      source: null,
      elapsed_ms: 0,
      dub_ms: 0,
      dub_rms_dbfs: null,
      sk_lines: 0,
      sample: "",
      session: null,
      error: "no Gemini API key configured (setting gemini_api_key is empty)",
    };
    expect(dubLiveGateFailures(409, refused)).toEqual([
      "the probe was refused (HTTP 409): no Gemini API key configured (setting gemini_api_key is empty)",
      "the probe made no dub",
      "the dub holds no sound (not read dBFS, floor -45)",
      "the session gave no Slovak transcript line",
    ]);
  });

  test("a failed session fails with the probe's error", () => {
    const failed: DubProbe = {
      ...dubbed(),
      ok: false,
      dub_ms: 0,
      dub_rms_dbfs: null,
      sk_lines: 0,
      session: null,
      error: "the dub session failed: Live reconnect (connection 1) refused: 403",
    };
    expect(dubLiveGateFailures(200, failed)[0]).toBe(
      "the probe failed: the dub session failed: Live reconnect (connection 1) refused: 403",
    );
  });

  test("a silent dub fails even when the probe said ok", () => {
    expect(dubLiveGateFailures(200, { ...dubbed(), dub_rms_dbfs: -60.5 })).toEqual([
      "the dub holds no sound (-60.5 dBFS, floor -45)",
    ]);
  });

  test("a dub with no Slovak line fails", () => {
    expect(dubLiveGateFailures(200, { ...dubbed(), sk_lines: 0, sample: "" })).toEqual([
      "the session gave no Slovak transcript line",
    ]);
  });

  test("an empty dub fails", () => {
    expect(dubLiveGateFailures(200, { ...dubbed(), dub_ms: 0 })).toEqual(["the probe made no dub"]);
  });

  test("a failure with no error text still names it", () => {
    expect(dubLiveGateFailures(200, { ...dubbed(), ok: false, error: null })).toEqual([
      "the probe failed: (no error text)",
    ]);
  });
});
