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
  pickTemplateInput,
  probeInputSettings,
  probeReceiverAttached,
  probeSteps,
  programCarriesBaseline,
  programSourceName,
  receiversSettled,
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

  test("the template is the baseline's input, else an sp-* one, never the probe", () => {
    expect(pickTemplateInput(["cam1", "sp-fast_video", "sp-slow_video"])).toBe("sp-slow_video");
    expect(pickTemplateInput([AV_PROBE_INPUT, "cam1", "sp-fast_video"])).toBe("sp-fast_video");
    expect(pickTemplateInput([AV_PROBE_INPUT, "cam1"])).toBe("cam1");
    expect(pickTemplateInput([AV_PROBE_INPUT])).toBeNull();
    expect(pickTemplateInput([])).toBeNull();
  });

  test("the probe input copies a template, forces audio + full bandwidth, starts idle", () => {
    const template = {
      ndi_source_name: "RESOLUME-SNV (SP-slow)",
      ndi_behavior: 0,
      ndi_bw_mode: 2, // audio only on the template: never on the probe
      ndi_audio: false,
      genlock_monitor: true,
      genlock_fifo: true,
    };
    expect(probeInputSettings(template, "")).toEqual({
      ndi_source_name: "",
      ndi_behavior: 0,
      ndi_bw_mode: 0,
      ndi_audio: true,
      genlock_monitor: false,
      genlock_fifo: true,
    });
    expect(template.ndi_source_name, "the template is not changed").toBe("RESOLUME-SNV (SP-slow)");
    expect(probeInputSettings(null, "")).toEqual({
      ndi_source_name: "",
      ndi_audio: true,
      genlock_monitor: false,
      ndi_bw_mode: 0,
    });
  });

  test("the provisioning steps leave the probe ready and idle, never removed", () => {
    const missing = { sceneExists: false, inputSource: undefined, inputInScene: false };
    expect(probeSteps(missing)).toEqual(["create_scene", "create_input"]);
    expect(probeSteps({ ...missing, sceneExists: true })).toEqual(["create_input"]);
    // The input exists elsewhere: the scene is made and the input put in it.
    expect(probeSteps({ sceneExists: false, inputSource: "", inputInScene: false })).toEqual([
      "create_scene",
      "add_to_scene",
    ]);
    // A run that died mid-take left it pointed at SP-program: idle it.
    expect(
      probeSteps({ sceneExists: true, inputSource: "X (SP-program)", inputInScene: true }),
    ).toEqual(["idle"]);
    expect(
      probeSteps({ sceneExists: true, inputSource: "X (SP-program)", inputInScene: false }),
    ).toEqual(["add_to_scene", "idle"]);
    // Ready and idle: nothing (no name set at all reads as idle too).
    expect(probeSteps({ sceneExists: true, inputSource: "", inputInScene: true })).toEqual([]);
    expect(probeSteps({ sceneExists: true, inputSource: null, inputInScene: true })).toEqual([]);
  });

  test("the take records SP-program only while it carries the baseline playlist", () => {
    expect(programCarriesBaseline(7, 7)).toBe(true);
    expect(programCarriesBaseline(-1, 7), "OBS manuál: cg OBS would record itself").toBe(
      false,
    );
    expect(programCarriesBaseline(9, 7)).toBe(false);
    expect(programCarriesBaseline(null, 7)).toBe(false);
  });

  test("SP-program's receivers are settled once two reads agree", () => {
    expect(receiversSettled(null, 3)).toBe(false);
    expect(receiversSettled(4, 3)).toBe(false);
    expect(receiversSettled(3, 3)).toBe(true);
    expect(receiversSettled(0, 0)).toBe(true);
  });

  test("the probe receiver is attached once SP-program's receivers rise", () => {
    expect(probeReceiverAttached(3, 3)).toBe(false);
    expect(probeReceiverAttached(3, 4)).toBe(true);
    expect(probeReceiverAttached(0, 1)).toBe(true);
    // A "no reading" before the take counts as 0.
    expect(probeReceiverAttached(-1, 0)).toBe(false);
    expect(probeReceiverAttached(-1, 1)).toBe(true);
  });
});
