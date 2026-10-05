/**
 * Unit tests for the A/V gate's probe-scene decisions (#221 lane 3). Runs in
 * the ubuntu mock suite (playwright.config.ts); these `test()` blocks never
 * touch `page`, so they need no browser and no box.
 */

import { test, expect } from "@playwright/test";
import {
  AV_PROBE_INPUT,
  AV_PROBE_SCENE,
  ndiHost,
  probeInputSettings,
  probeReceiverAttached,
  probeSteps,
  programCarriesBaseline,
  programSourceName,
} from "./av-sync-probe";
import { pickBaselineScene } from "./obs-baseline-scene";

test.describe("A/V gate probe scene (#221 lane 3)", () => {
  test("the probe is never a playlist scene, nor the baseline", () => {
    expect(AV_PROBE_SCENE.startsWith("sp-")).toBe(false);
    expect(AV_PROBE_INPUT.startsWith("sp-")).toBe(false);
    expect(pickBaselineScene([AV_PROBE_SCENE, "sp-slow"])).toBe("sp-slow");
    // Even when cg OBS has no sp-* scene left, the probe is never the baseline.
    expect(pickBaselineScene([AV_PROBE_SCENE, "fotky loop"])).toBe("fotky loop");
  });

  test("ndiHost reads the host of an advertised source name", () => {
    expect(ndiHost("RESOLUME-SNV (SP-slow)")).toBe("RESOLUME-SNV");
    expect(ndiHost("CG-OBS (manual (2))")).toBe("CG-OBS");
    expect(ndiHost("SP-slow")).toBeNull();
    expect(ndiHost("")).toBeNull();
  });

  test("SP-program's source name takes COMPUTERNAME, else the template's host", () => {
    expect(programSourceName("RESOLUME-SNV", "resolume-snv (SP-slow)")).toBe(
      "RESOLUME-SNV (SP-program)",
    );
    expect(programSourceName(undefined, "RESOLUME-SNV (SP-slow)")).toBe(
      "RESOLUME-SNV (SP-program)",
    );
    expect(programSourceName("  ", "RESOLUME-SNV (SP-slow)")).toBe("RESOLUME-SNV (SP-program)");
    expect(programSourceName(undefined, "no-parens")).toBeNull();
    expect(programSourceName(undefined, null)).toBeNull();
  });

  test("the probe input copies a template's settings with SP-program's name", () => {
    const template = {
      ndi_source_name: "RESOLUME-SNV (SP-slow)",
      ndi_behavior: 0,
      ndi_behavior_timeout: 1,
      ndi_bw_mode: 0,
    };
    expect(probeInputSettings(template, "RESOLUME-SNV (SP-program)")).toEqual({
      ndi_source_name: "RESOLUME-SNV (SP-program)",
      ndi_behavior: 0,
      ndi_behavior_timeout: 1,
      ndi_bw_mode: 0,
    });
    expect(template.ndi_source_name, "the template is not changed").toBe("RESOLUME-SNV (SP-slow)");
    expect(probeInputSettings(null, "X (SP-program)")).toEqual({
      ndi_source_name: "X (SP-program)",
    });
  });

  test("the provisioning steps: create, add, re-point, or nothing", () => {
    const wanted = "RESOLUME-SNV (SP-program)";
    const missing = { sceneExists: false, inputSource: undefined, inputInScene: false };
    expect(probeSteps(missing, wanted)).toEqual(["create_scene", "create_input"]);
    expect(probeSteps({ ...missing, sceneExists: true }, wanted)).toEqual(["create_input"]);
    // The input exists elsewhere: the scene is made and the input put in it.
    expect(
      probeSteps({ sceneExists: false, inputSource: wanted, inputInScene: false }, wanted),
    ).toEqual(["create_scene", "add_to_scene"]);
    expect(
      probeSteps({ sceneExists: true, inputSource: "OLD (SP-program)", inputInScene: true }, wanted),
    ).toEqual(["repoint"]);
    expect(
      probeSteps({ sceneExists: true, inputSource: null, inputInScene: false }, wanted),
    ).toEqual(["add_to_scene", "repoint"]);
    expect(
      probeSteps({ sceneExists: true, inputSource: wanted, inputInScene: true }, wanted),
    ).toEqual([]);
  });

  test("the take records SP-program only while it carries the baseline playlist", () => {
    expect(programCarriesBaseline(7, 7)).toBe(true);
    expect(programCarriesBaseline(-1, 7), "OBS manuál: cg OBS would record itself").toBe(
      false,
    );
    expect(programCarriesBaseline(9, 7)).toBe(false);
    expect(programCarriesBaseline(null, 7)).toBe(false);
  });

  test("the probe receiver is attached once SP-program's receivers rise", () => {
    // Switched just now: the count must rise above the one before the switch.
    expect(probeReceiverAttached(true, 3, 3)).toBe(false);
    expect(probeReceiverAttached(true, 3, 4)).toBe(true);
    expect(probeReceiverAttached(true, 0, 1)).toBe(true);
    // A "no reading" before the switch counts as 0.
    expect(probeReceiverAttached(true, -1, 0)).toBe(false);
    expect(probeReceiverAttached(true, -1, 1)).toBe(true);
    // cg OBS already showed the probe: any receiver will do.
    expect(probeReceiverAttached(false, 5, 1)).toBe(true);
    expect(probeReceiverAttached(false, 5, 0)).toBe(false);
  });
});
