/**
 * Thin wrapper around obs-websocket-js for post-deploy Playwright tests.
 *
 * Used by the post-deploy suite to switch OBS scenes and verify that
 * SongPlayer's scene-driven playback engine reacts correctly.
 */

import OBSWebSocket from "obs-websocket-js";
import {
  shouldSkipSceneSwitch,
  waitForPreviewApplied,
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

  async currentPreviewScene(): Promise<string> {
    const r = await this.obs.call("GetCurrentPreviewScene");
    return (r as { currentPreviewSceneName: string }).currentPreviewSceneName;
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
        // The preview change is applied asynchronously — trigger only once OBS
        // reports it, else the transition fades the program scene to itself.
        await waitForPreviewApplied(() => this.currentPreviewScene(), sceneName);
        await this.obs.call("TriggerStudioModeTransition");
        // We KNOW a transition is now running — mark it active synchronously so
        // the settle check cannot fire before the SceneTransitionStarted frame
        // arrives. In Studio Mode GetCurrentProgramScene can report the target
        // near fade start, so relying on the async Started event to raise this
        // flag would reinstate the round-2 name-only early return. The real
        // SceneTransitionEnded frame (a 2s Fade always emits one) clears it.
        transitionActive = true;
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

  /** Whether OBS is currently recording (`GetRecordStatus.outputActive`). */
  async isRecording(): Promise<boolean> {
    const r = await this.obs.call("GetRecordStatus");
    return (r as { outputActive: boolean }).outputActive;
  }

  /**
   * #147: start recording the OBS PROGRAM. Refuses to hijack a recording that
   * is already running: it belongs to the operator, and its file must never be
   * stopped or deleted by CI.
   */
  async startRecord(): Promise<void> {
    if (await this.isRecording()) {
      throw new Error(
        "OBS is already recording (an operator recording?) — the A/V gate will not stop or reuse it",
      );
    }
    await this.obs.call("StartRecord");
  }

  /**
   * Stop the recording started by `startRecord` and return the file path OBS
   * wrote. Resolves once `GetRecordStatus` reports the output inactive, so the
   * file is finalized before anyone reads it. Throws if it never goes inactive.
   */
  async stopRecord(timeoutMs = 10_000): Promise<string> {
    const r = await this.obs.call("StopRecord");
    const outputPath = (r as { outputPath: string }).outputPath;
    const deadline = Date.now() + timeoutMs;
    while (await this.isRecording()) {
      if (Date.now() >= deadline) {
        throw new Error(`OBS recording did not stop within ${timeoutMs} ms (${outputPath})`);
      }
      await new Promise((res) => setTimeout(res, 200));
    }
    return outputPath;
  }

  async disconnect(): Promise<void> {
    try {
      await this.obs.disconnect();
    } catch {
      // Ignore errors during disconnect.
    }
  }
}
