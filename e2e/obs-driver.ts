/**
 * Thin wrapper around obs-websocket-js for post-deploy Playwright tests.
 *
 * Used by the post-deploy suite to switch OBS scenes and verify that
 * SongPlayer's scene-driven playback engine reacts correctly.
 */

import OBSWebSocket from "obs-websocket-js";

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
    // Give OBS + SongPlayer a moment to propagate the change.
    await new Promise((r) => setTimeout(r, 300));
  }

  async disconnect(): Promise<void> {
    try {
      await this.obs.disconnect();
    } catch {
      // Ignore errors during disconnect.
    }
  }
}
