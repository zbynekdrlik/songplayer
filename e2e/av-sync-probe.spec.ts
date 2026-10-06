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
  probeIdleSettings,
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

  test("the probe input copies a template, forces the certified receive path, starts idle", () => {
    const template = {
      ndi_source_name: "RESOLUME-SNV (SP-slow)",
      ndi_behavior: 2,
      ndi_sync: 1,
      ndi_bw_mode: 2, // audio only on the template: never on the probe
      ndi_audio: false,
      genlock_monitor: true,
      genlock_burn: true,
      genlock_fifo: false,
    };
    expect(probeInputSettings(template, "")).toEqual({
      ndi_source_name: "",
      // Kept from the template; DistroAV forces them once genlock_fifo is on.
      ndi_behavior: 2,
      ndi_sync: 1,
      // Forced by the probe.
      genlock_fifo: true,
      ndi_bw_mode: 0,
      ndi_audio: true,
      genlock_monitor: false,
      genlock_burn: false,
    });
    expect(template.ndi_source_name, "the template is not changed").toBe("RESOLUME-SNV (SP-slow)");
    expect(probeInputSettings(null, "")).toEqual({
      ndi_source_name: "",
      genlock_fifo: true,
      ndi_audio: true,
      genlock_monitor: false,
      genlock_burn: false,
      ndi_bw_mode: 0,
    });
  });

  test("an existing probe is reset to its fixed settings, idle", () => {
    expect(probeIdleSettings()).toEqual({
      ndi_source_name: "",
      genlock_fifo: true,
      ndi_audio: true,
      genlock_monitor: false,
      genlock_burn: false,
      ndi_bw_mode: 0,
    });
  });

  test("the provisioning steps leave the probe ready, reset and idle, never removed", () => {
    const missing = { sceneExists: false, inputSource: undefined, inputInScene: false };
    // A new input is created idle with its fixed settings: no reset needed.
    expect(probeSteps(missing)).toEqual(["create_scene", "create_input"]);
    expect(probeSteps({ ...missing, sceneExists: true })).toEqual(["create_input"]);
    // The input exists elsewhere: the scene is made, the input put in it, reset.
    expect(probeSteps({ sceneExists: false, inputSource: "", inputInScene: false })).toEqual([
      "create_scene",
      "add_to_scene",
      "reset",
    ]);
    // A run that died mid-take left it pointed at SP-program: reset (idle) it.
    expect(
      probeSteps({ sceneExists: true, inputSource: "X (SP-program)", inputInScene: true }),
    ).toEqual(["reset"]);
    expect(
      probeSteps({ sceneExists: true, inputSource: "X (SP-program)", inputInScene: false }),
    ).toEqual(["add_to_scene", "reset"]);
    // Ready and idle: still reset, so a hand-edited setting never survives a run.
    expect(probeSteps({ sceneExists: true, inputSource: "", inputInScene: true })).toEqual([
      "reset",
    ]);
    expect(probeSteps({ sceneExists: true, inputSource: null, inputInScene: true })).toEqual([
      "reset",
    ]);
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
