/**
 * Thin wrapper around obs-websocket-js for post-deploy Playwright tests.
 *
 * Used by the post-deploy suite to switch OBS scenes and verify that
 * SongPlayer's scene-driven playback engine reacts correctly.
 */

import OBSWebSocket from "obs-websocket-js";
import {
  shouldSkipSceneSwitch,
  waitForSceneSwitchApplied,
} from "./obs-scene-wait";

export class ObsDriver {
  // Cached once per driver (studio mode does not change mid-suite).
  private studioMode: boolean | null = null;

  private constructor(private obs: OBSWebSocket) {}

  static async connect(url: string, password?: string): Promise<ObsDriver> {
    const obs = new OBSWebSocket();
    await obs.connect(url, password);
    return new ObsDriver(obs);
  }

  async currentProgramScene(): Promise<string> {
    const r = await this.obs.call("GetCurrentProgramScene");
    return (r as { currentProgramSceneName: string }).currentProgramSceneName;
  }

  async listScenes(): Promise<string[]> {
    const r = await this.obs.call("GetSceneList");
    return (r as { scenes: { sceneName: string }[] }).scenes.map((s) => s.sceneName);
  }

  private async studioModeEnabled(): Promise<boolean> {
    if (this.studioMode === null) {
      const r = await this.obs.call("GetStudioModeEnabled");
      this.studioMode = (r as { studioModeEnabled: boolean }).studioModeEnabled;
    }
    return this.studioMode;
  }

  /**
   * Switch the OBS program scene and wait until the switch has ACTUALLY taken
   * effect (#170). On win-resolume OBS runs Studio Mode with a 2000ms Fade:
   *
   *  - Never issue a same-scene switch — it runs a pointless fade that leaves
   *    `preview == program`, from which OBS then DROPS the next switch's
   *    `CurrentProgramSceneChanged` event (reproduced live, round 3).
   *  - In studio mode, drive the transition the studio way
   *    (`SetCurrentPreviewScene` + `TriggerStudioModeTransition`) so OBS emits
   *    the program-scene-changed event SongPlayer reacts to; fall back to
   *    `SetCurrentProgramScene` when studio mode is off.
   *  - Wait until the program scene equals the target AND the transition has
   *    ENDED (a name-only read is satisfied mid-fade), then a short settle so
   *    SongPlayer processes the post-transition event (its reaction is <1ms).
   */
  async switchScene(sceneName: string): Promise<void> {
    const current = await this.currentProgramScene();
    if (shouldSkipSceneSwitch(current, sceneName)) return;

    // Track the transition so we can wait for it to END, not just for the
    // program-scene name to flip (which happens mid-fade). Subscribe BEFORE
    // triggering so the Started event is never missed.
    let transitionActive = false;
    const onStart = () => {
      transitionActive = true;
    };
    const onEnd = () => {
      transitionActive = false;
    };
    this.obs.on("SceneTransitionStarted", onStart);
    this.obs.on("SceneTransitionEnded", onEnd);

    try {
      if (await this.studioModeEnabled()) {
        await this.obs.call("SetCurrentPreviewScene", { sceneName });
        await this.obs.call("TriggerStudioModeTransition");
      } else {
        await this.obs.call("SetCurrentProgramScene", { sceneName });
      }
      await waitForSceneSwitchApplied(
        () => this.currentProgramScene(),
        () => transitionActive,
        sceneName,
      );
    } finally {
      this.obs.off("SceneTransitionStarted", onStart);
      this.obs.off("SceneTransitionEnded", onEnd);
    }

    await new Promise((r) => setTimeout(r, 400));
  }

  async disconnect(): Promise<void> {
    try {
      await this.obs.disconnect();
    } catch {
      // Ignore errors during disconnect.
    }
  }
}
