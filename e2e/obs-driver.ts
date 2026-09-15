/**
 * Thin wrapper around obs-websocket-js for post-deploy Playwright tests.
 *
 * Used by the post-deploy suite to switch OBS scenes and verify that
 * SongPlayer's scene-driven playback engine reacts correctly.
 */

import OBSWebSocket from "obs-websocket-js";
import { waitForProgramScene } from "./obs-scene-wait";

export class ObsDriver {
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

  async switchScene(sceneName: string): Promise<void> {
    await this.obs.call("SetCurrentProgramScene", { sceneName });
    // Wait for the switch to ACTUALLY take effect before returning. OBS on
    // win-resolume runs Studio Mode with a 2000ms Fade, so the program scene
    // — and the CurrentProgramSceneChanged event SongPlayer reacts to — only
    // reports `sceneName` after the fade completes ~2s later. A blind sleep
    // races that fade and made tests 15/17 flaky (#170). Poll
    // GetCurrentProgramScene until it applies (throws loudly if it never
    // does), then a short settle so SongPlayer processes the post-transition
    // event (its reaction is <1ms, so 400ms is ample margin).
    await waitForProgramScene(
      () => this.currentProgramScene(),
      sceneName,
    );
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
