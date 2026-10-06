/**
 * Thin wrapper around obs-websocket-js for post-deploy Playwright tests.
 *
 * Used by the post-deploy suite to switch scenes and verify that SongPlayer's
 * program (the playback authority since #221 L4b) reacts correctly.
 *
 * #221 L3: the SCENE driver connects to SongPlayer's obs-websocket facade
 * (`FACADE_WS_URL`, :4456), the server Companion's buttons talk to: studio
 * mode is ON there, so `switchScene` takes Companion's exact path (preview,
 * then transition), and the program scene and the transition events are
 * SongPlayer's own. A second driver on cg OBS (`OBS_WS_URL`, :4455) is kept
 * for the A/V gate: its recording and profile requests, and (#221 lane 3) the
 * gate's probe scene — provisioning it (`av-sync-probe.ts`: the scene, its one
 * DistroAV receiver of `SP-program`) and putting cg OBS on it for the take and
 * back, each switch only when cg OBS is on another scene — and (#221 dev.18)
 * the wait for the probe's audio before every take (`waitForInputAudio`, the
 * only user of the high-volume `InputVolumeMeters`, subscribed for the wait
 * alone).
 */

// The bare import: in Node it resolves to the MSGPACK build, which offers
// only `obswebsocket.msgpack` — Companion's exact encoding (its obs-studio
// module runs in Node). SongPlayer's facade speaks it since #221 L2b and cg
// OBS always did, so both drivers go through it; never import
// "obs-websocket-js/json" here, it would test a path Companion never takes
// (obs-driver-protocol.spec.ts).
import OBSWebSocket, { EventSubscription } from "obs-websocket-js";
import {
  meterInputs,
  waitForInputAudio,
  type AudioWaitOptions,
  type AudioWaitReport,
} from "./obs-audio-wait";
import { waitForPreviewApplied, waitForSceneSwitchApplied } from "./obs-scene-wait";

export class ObsDriver {
  // Cached once per driver (studio mode does not change mid-suite).
  private studioMode: boolean | null = null;
  /** #147: the file of the last StopRecord, kept even if the inactive-poll
   * below times out, so a caller's cleanup can still delete it. */
  lastRecordingPath: string | null = null;
  /** #147: a StartRecord was SENT after the no-operator-recording pre-check.
   * A recording active after that is ours, even if the call never answered. */
  startIssued = false;

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
    // obs-websocket-js types `scenes` as plain JSON objects.
    return (r as unknown as { scenes: { sceneName: string }[] }).scenes.map((s) => s.sceneName);
  }

  private async studioModeEnabled(): Promise<boolean> {
    if (this.studioMode === null) {
      const r = await this.obs.call("GetStudioModeEnabled");
      this.studioMode = (r as { studioModeEnabled: boolean }).studioModeEnabled;
    }
    return this.studioMode;
  }

  /**
   * Switch the program scene and wait until the switch has ACTUALLY taken
   * effect (#170).
   *
   *  - #221 L3: a switch to the scene already on program is ALWAYS sent. The
   *    driver talks to SongPlayer's facade, where a same-scene transition is
   *    the designed re-kick (it plays a playlist paused out of band). The skip
   *    existed for cg OBS's own same-scene 2 s fade, which dropped the next
   *    switch's event (#170). The scene driver never switches cg OBS (#221
   *    B4 step 6: a playlist press tells cg OBS nothing at all); the A/V
   *    gate's recorder does, and guards each of its switches against the
   *    same scene itself.
   *  - In studio mode, drive the transition the studio way
   *    (`SetCurrentPreviewScene` + `TriggerStudioModeTransition`) so OBS emits
   *    the program-scene-changed event SongPlayer reacts to; fall back to
   *    `SetCurrentProgramScene` when studio mode is off.
   *  - Wait until the program scene equals the target AND the transition has
   *    ENDED (a name-only read is satisfied mid-fade), then a short settle so
   *    SongPlayer processes the post-transition event (its reaction is <1ms).
   */
  async switchScene(sceneName: string): Promise<void> {
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
        // We KNOW a transition is about to run — mark it active BEFORE the
        // trigger, so the settle check cannot fire before the
        // SceneTransitionStarted frame arrives. GetCurrentProgramScene can
        // report the target near the transition's start, so relying on the
        // async Started event would reinstate the round-2 name-only early
        // return. Only the real SceneTransitionEnded frame clears it. #221 L3:
        // set BEFORE the call, never after: SongPlayer's facade ends a Cut at
        // once, and its Ended frame may be handled before the call's promise
        // continuation runs, which a flag raised after the call would undo.
        transitionActive = true;
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

  /**
   * Whether the current OBS profile remuxes finished recordings to mp4
   * (`Video/AutoRemux`), which leaves a sibling `<base>.mp4` to clean up.
   */
  async autoRemuxEnabled(): Promise<boolean> {
    const r = await this.obs.call("GetProfileParameter", {
      parameterCategory: "Video",
      parameterName: "AutoRemux",
    });
    const v = (r as { parameterValue: string | null; defaultParameterValue: string | null });
    // OBS's config_get_bool accepts "true" or any non-zero number.
    const raw = (v.parameterValue ?? v.defaultParameterValue ?? "false").trim().toLowerCase();
    return raw === "true" || (raw !== "" && !Number.isNaN(Number(raw)) && Number(raw) !== 0);
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
    this.startIssued = false; // per call: true only once THIS call passed the pre-check
    if (await this.isRecording()) {
      throw new Error(
        "OBS is already recording (an operator recording?) — the A/V gate will not stop or reuse it",
      );
    }
    this.startIssued = true;
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
    this.lastRecordingPath = outputPath;
    const deadline = Date.now() + timeoutMs;
    while (await this.isRecording()) {
      if (Date.now() >= deadline) {
        throw new Error(`OBS recording did not stop within ${timeoutMs} ms (${outputPath})`);
      }
      await new Promise((res) => setTimeout(res, 200));
    }
    return outputPath;
  }

  // ---- #221 lane 3: the A/V gate's probe scene (cg OBS only) ----

  /** The names of the inputs of `inputKind` (e.g. DistroAV's `ndi_source`). */
  async listInputs(inputKind: string): Promise<string[]> {
    const r = await this.obs.call("GetInputList", { inputKind });
    return (r as { inputs: { inputName: string }[] }).inputs.map((i) => i.inputName);
  }

  /** An input's settings, or null when OBS has no input of that name. */
  async inputSettings(inputName: string): Promise<Record<string, unknown> | null> {
    try {
      const r = await this.obs.call("GetInputSettings", { inputName });
      return (r as { inputSettings: Record<string, unknown> }).inputSettings;
    } catch (e) {
      if (isNotFound(e)) return null;
      throw e;
    }
  }

  /** The id of `sourceName`'s item in `sceneName`, or null when it has none. */
  async sceneItemId(sceneName: string, sourceName: string): Promise<number | null> {
    try {
      const r = await this.obs.call("GetSceneItemId", { sceneName, sourceName });
      return (r as { sceneItemId: number }).sceneItemId;
    } catch (e) {
      if (isNotFound(e)) return null;
      throw e;
    }
  }

  async createScene(sceneName: string): Promise<void> {
    await this.obs.call("CreateScene", { sceneName });
  }

  /** Create an input in `sceneName`; returns its scene item id. */
  async createInput(
    sceneName: string,
    inputName: string,
    inputKind: string,
    inputSettings: Record<string, unknown>,
  ): Promise<number> {
    const r = await this.obs.call("CreateInput", {
      sceneName,
      inputName,
      inputKind,
      // obs-websocket-js types settings as a JSON object.
      inputSettings: inputSettings as never,
      sceneItemEnabled: true,
    });
    return (r as { sceneItemId: number }).sceneItemId;
  }

  /** Put an existing source into `sceneName`; returns its scene item id. */
  async addSceneItem(sceneName: string, sourceName: string): Promise<number> {
    const r = await this.obs.call("CreateSceneItem", {
      sceneName,
      sourceName,
      sceneItemEnabled: true,
    });
    return (r as { sceneItemId: number }).sceneItemId;
  }

  /** Merge `inputSettings` into an input's settings. */
  async setInputSettings(inputName: string, inputSettings: Record<string, unknown>): Promise<void> {
    await this.obs.call("SetInputSettings", {
      inputName,
      inputSettings: inputSettings as never,
      overlay: true,
    });
  }

  /** Fit a scene item to the canvas, aspect kept (bounds = the base size). */
  async fitToCanvas(sceneName: string, sceneItemId: number): Promise<void> {
    const v = (await this.obs.call("GetVideoSettings")) as {
      baseWidth: number;
      baseHeight: number;
    };
    await this.obs.call("SetSceneItemTransform", {
      sceneName,
      sceneItemId,
      sceneItemTransform: {
        positionX: 0,
        positionY: 0,
        alignment: 5, // top left
        boundsType: "OBS_BOUNDS_SCALE_INNER",
        boundsAlignment: 0, // centred in the bounds
        boundsWidth: v.baseWidth,
        boundsHeight: v.baseHeight,
      },
    });
  }

  /**
   * #221 dev.18: wait until `inputName`'s audio flows — its input peak above
   * the floor for 1 s in a row, bounded (`obs-audio-wait.ts`; the A/V gate
   * calls it on the probe before every StartRecord).
   *
   * `InputVolumeMeters` is a HIGH-VOLUME event (every active input, every
   * 50 ms) that obs-websocket sends only to a session that asks for it, so
   * this session asks with a `Reidentify` for the wait alone and drops it
   * again after, whatever the outcome. The drop names the default `All`
   * explicitly: a `Reidentify` without `eventSubscriptions` keeps the current
   * ones. A connect never asks for it (`obs-driver-protocol.spec.ts`). A
   * connection that closes mid-wait ends it at once, naming the close, not
   * after the bound as "no event".
   */
  async waitForInputAudio(
    inputName: string,
    opts: AudioWaitOptions = {},
  ): Promise<AudioWaitReport> {
    await this.obs.reidentify({
      eventSubscriptions: EventSubscription.All | EventSubscription.InputVolumeMeters,
    });
    let report: AudioWaitReport;
    try {
      report = await waitForInputAudio(
        (onMeters, onClosed) => {
          const meters = (data: { inputs: unknown }) => onMeters(meterInputs(data.inputs));
          const closed = (e: { code?: number; message?: string }) =>
            onClosed(`code ${e.code ?? "?"}${e.message ? `, ${e.message}` : ""}`);
          this.obs.on("InputVolumeMeters", meters);
          this.obs.on("ConnectionClosed", closed);
          return () => {
            this.obs.off("InputVolumeMeters", meters);
            this.obs.off("ConnectionClosed", closed);
          };
        },
        inputName,
        opts,
      );
    } catch (e) {
      // Keep the wait's own error; a failed drop is only logged here.
      await this.dropVolumeMeters().catch((d) =>
        console.warn(`OBS: could not drop InputVolumeMeters after a failed audio wait: ${d}`),
      );
      throw e;
    }
    await this.dropVolumeMeters();
    return report;
  }

  /** Back to the default subscriptions (`All`, no high-volume event). */
  private async dropVolumeMeters(): Promise<void> {
    await this.obs.reidentify({ eventSubscriptions: EventSubscription.All });
  }

  async disconnect(): Promise<void> {
    try {
      await this.obs.disconnect();
    } catch {
      // Ignore errors during disconnect.
    }
  }
}

/** obs-websocket's "resource not found" (600): no input / scene item of that name. */
function isNotFound(e: unknown): boolean {
  return (e as { code?: number } | null)?.code === 600;
}
