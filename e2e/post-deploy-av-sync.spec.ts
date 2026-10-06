/**
 * Post-deploy A/V sync + audio-dropout gate (#147).
 *
 * The owner reported a MAJOR lipsync regression while every gate was green.
 * This gate measures the REAL output. It records the OBS PROGRAM while an sp-*
 * output plays, then compares the recording with the ORIGINAL cached sidecars
 * (`scripts/av_sync_check.py`: audio cross-correlation, video frame alignment,
 * sliding 10 ms dropout windows). It FAILS when |A/V| > 40 ms, when any
 * dropout exists, or when the measurement is not trustworthy (low
 * correlation, match or contrast). A "cannot measure" result is a failure,
 * never a skip.
 *
 * Takes: a take is repeated (up to MAX_TAKES in total, and only while the
 * time budget allows a full take) in two cases only:
 * - the song changed during it;
 * - it was unmeasurable because of the PICTURE alone (a still or overlaid
 *   video). The playlist is skipped to the next song first; the skip moves
 *   the playlist position and is not undone.
 * A take that FAILS is never repeated, and neither is an unmeasurable AUDIO
 * side. Dropouts are reported as a FAIL even when the picture is
 * unmeasurable.
 *
 * OBS discipline (CLAUDE.md): the program goes to the shared baseline scene
 * (sp-slow preferred, never sp-warmup/sp-fast). The scene the operator was on
 * is captured first and restored after. #221 L3: scenes are switched through
 * SongPlayer's obs-websocket facade (`FACADE_WS_URL`, Companion's exact
 * studio-mode path: preview + transition, SongPlayer's own program feedback);
 * the recording and the profile read stay on cg OBS (`OBS_WS_URL`). #221
 * lane 3: a playlist has no NDI output of its own, so the take records
 * SongPlayer's PROGRAM, `SP-program`, the output every consumer takes,
 * through the gate's own cg OBS probe scene (`av-sync-probe.ts`: one DistroAV
 * input, provisioned by the gate, never an sp-* scene). The probe is IDLE
 * (`ndi_source_name` "") outside the take — DistroAV keeps a receiver whether
 * the input is shown or not — and is pointed at `SP-program` only once
 * `SP-program` carries the baseline playlist (never "OBS manuál": cg OBS
 * would record itself); the take waits until SP-program's receivers rose.
 * #221 dev.18: every StartRecord then waits until the probe's AUDIO flows
 * (`obs-audio-wait.ts`: its InputVolumeMeters input peak above -60 dBFS for
 * 1 s in a row, bounded at 20 s): a freshly attached DistroAV receiver
 * delivers its picture at once and its audio with gaps until camera-box's
 * genlock audio pairing locks (~4 s after the bind), and a take started
 * before that opened with two dropouts (dev.17, run 37423917199). The
 * dropout check is unchanged.
 * afterAll idles the probe first (an idle probe shows nothing, so restoring
 * the program to "OBS manuál" can never loop the picture), restores the
 * program scene, then cg OBS's own scene only when the program restore did
 * not already (a manual scene is set on cg OBS by the facade), and no probe
 * receiver stays on `SP-program`. Every
 * recording file (plus its auto-remux sibling) is deleted, and an operator's
 * own running recording is never touched (`startRecord` refuses). The SONG
 * mixer faders are set to unity for the measurement and restored after.
 *
 * Evidence: a take that was analysed and did NOT pass (fail, cannot_measure,
 * an analysis error) is copied first, with its auto-remux sibling, the
 * analysis JSON and its stderr, into the Playwright output dir
 * (`e2e/test-results/<test>/av-sync-evidence/take<N>-*`). The CI upload step
 * keeps that dir when the job fails. The OBS folder itself is still emptied,
 * so a pass leaves no copy anywhere.
 *
 * afterAll is the safety net for a test body that timed out. In order, it:
 * - kills the analysis;
 * - settles a pending start (at most 10 s);
 * - stops our recording;
 * - restores the faders, idles the probe, restores the program scene, then
 *   cg OBS's scene;
 * - then deletes every recording made.
 * Every step is attempted even if an earlier one fails.
 *
 * Box paths (override via env): `SP_AVSYNC_PYTHON` = the gate's OWN venv with
 * pinned numpy (#221: created by the CI post-deploy step; never the lyrics
 * venv, whose packages SongPlayer may reinstall at startup), `SP_FFMPEG` = the
 * app's bundled ffmpeg. There is no ffprobe on the box, and the script does
 * not need one.
 */

import {
  test,
  expect,
  request as apiRequest,
  type APIRequestContext,
} from "@playwright/test";
import { spawn, spawnSync, type ChildProcess } from "child_process";
import * as fs from "fs";
import * as path from "path";
import { ObsDriver } from "./obs-driver";
import { pickBaselineScene } from "./obs-baseline-scene";
import {
  AV_PROBE_INPUT,
  AV_PROBE_SCENE,
  NDI_INPUT_KIND,
  pickTemplateInput,
  probeIdleSettings,
  probeInputSettings,
  probeReceiverAttached,
  probeSteps,
  programCarriesBaseline,
  programSourceName,
  receiversSettled,
} from "./av-sync-probe";
import { EVIDENCE_COPY_MS, keepRecording, keepText, type Evidence } from "./av-sync-evidence";
import { AUDIO_WAIT_TIMEOUT_MS } from "./obs-audio-wait";
import {
  classifyAvSyncRun,
  isPlayingWithFrames,
  keepsEvidence,
  nowPlayingVideoId,
  recordingFiles,
  resolveSidecars,
  type AvSyncRun,
  type HealthRow,
  type MixNowPlaying,
} from "./av-sync-gate";

const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";
// #221 L3: scene switches go through SongPlayer's facade; cg OBS only records.
const FACADE_WS_URL = process.env.FACADE_WS_URL || "ws://localhost:4456";
const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";
const PYTHON =
  process.env.SP_AVSYNC_PYTHON ||
  "C:\\ProgramData\\SongPlayer\\e2e\\avsync_venv\\Scripts\\python.exe";
const FFMPEG = process.env.SP_FFMPEG || "C:\\ProgramData\\SongPlayer\\cache\\tools\\ffmpeg.exe";
const SCRIPT = path.resolve(__dirname, "..", "scripts", "av_sync_check.py");

const MAX_AV_MS = 40;
const RECORD_MS = 20_000;
const ANALYSIS_TIMEOUT_MS = 60_000; // ~5-10 s on the box
const MAX_TAKES = 3;
// #221 dev.18: 300 → 320 s, by the probe audio wait's bound, so a take keeps
// the retake room it had before the wait (RETAKE_BEFORE_MS stays 110 s).
const TEST_TIMEOUT_MS = 320_000;
// The worst case of one take: skip 15 + play 30 + the probe audio wait
// (AUDIO_WAIT_TIMEOUT_MS, 20) + record (RECORD_MS, 20) + stop 10 + analysis
// (ANALYSIS_TIMEOUT_MS, 60) + cleanup 35 + evidence copy 2 x
// EVIDENCE_COPY_MS (5) = 200 s.
const WORST_TAKE_MS =
  15_000 +
  30_000 +
  AUDIO_WAIT_TIMEOUT_MS +
  RECORD_MS +
  10_000 +
  ANALYSIS_TIMEOUT_MS +
  35_000 +
  2 * EVIDENCE_COPY_MS;
// A retake starts only while this much of the budget has been used. A full
// worst-case take then still fits, with 10 s for the calls not counted above
// (the StartRecord pre-check, the /mix and /videos reads, spawning the
// analysis).
const RETAKE_BEFORE_MS = TEST_TIMEOUT_MS - WORST_TAKE_MS - 10_000;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function getJson<T>(request: APIRequestContext, url: string): Promise<T> {
  const resp = await request.get(url);
  expect(resp.status(), `GET ${url}`).toBe(200);
  return (await resp.json()) as T;
}

async function pollUntil<T>(
  what: string,
  timeoutMs: number,
  read: () => Promise<T>,
  ok: (v: T) => boolean,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last = await read();
  while (!ok(last)) {
    if (Date.now() >= deadline) {
      throw new Error(`${what} not reached within ${timeoutMs} ms; last=${JSON.stringify(last)}`);
    }
    await sleep(500);
    last = await read();
  }
  return last;
}

/** What the gate reads of `GET /api/v1/program`. */
interface ProgramView {
  source: number | null;
  health: { connections: number };
}

/**
 * #221 lane 3: make cg OBS's probe scene ready and its input IDLE
 * (`av-sync-probe.ts`): the scene and its one DistroAV input, created when
 * missing (the input's settings copied from an existing cg OBS NDI input,
 * with the certified receive path forced, and no source), put in the scene,
 * and reset (its fixed settings, idle) on every run; never removed.
 * Returns the source name `SP-program` is advertised under, which the take
 * points the probe at.
 */
async function ensureProbeScene(rec: ObsDriver): Promise<string> {
  const template = pickTemplateInput(await rec.listInputs(NDI_INPUT_KIND));
  const templateSettings = template ? await rec.inputSettings(template) : null;
  const templateSource =
    typeof templateSettings?.ndi_source_name === "string" ? templateSettings.ndi_source_name : null;
  const wanted = programSourceName(process.env.COMPUTERNAME, templateSource);
  if (!wanted) {
    throw new Error(
      "A/V gate: cannot name SP-program's NDI source — no COMPUTERNAME and no cg OBS NDI input to read the host from",
    );
  }
  const sceneExists = (await rec.listScenes()).includes(AV_PROBE_SCENE);
  const current = await rec.inputSettings(AV_PROBE_INPUT);
  const inputSource =
    current === null
      ? undefined
      : typeof current.ndi_source_name === "string"
        ? current.ndi_source_name
        : null;
  const inputInScene =
    sceneExists && current !== null && (await rec.sceneItemId(AV_PROBE_SCENE, AV_PROBE_INPUT)) !== null;
  for (const step of probeSteps({ sceneExists, inputSource, inputInScene })) {
    console.log(`A/V gate: probe scene — ${step} (${AV_PROBE_SCENE}, template ${template})`);
    if (step === "create_scene") {
      await rec.createScene(AV_PROBE_SCENE);
    } else if (step === "create_input") {
      const id = await rec.createInput(
        AV_PROBE_SCENE,
        AV_PROBE_INPUT,
        NDI_INPUT_KIND,
        probeInputSettings(templateSettings, ""),
      );
      await rec.fitToCanvas(AV_PROBE_SCENE, id);
    } else if (step === "add_to_scene") {
      const id = await rec.addSceneItem(AV_PROBE_SCENE, AV_PROBE_INPUT);
      await rec.fitToCanvas(AV_PROBE_SCENE, id);
    } else {
      await rec.setInputSettings(AV_PROBE_INPUT, probeIdleSettings());
    }
  }
  return wanted;
}

/**
 * SP-program's receiver count once it has settled (two reads ~1.5 s apart
 * agree, at most ~15 s): the idle probe's receiver leaves asynchronously and
 * the sender polls its count about once a second.
 */
async function settledReceivers(request: APIRequestContext): Promise<number> {
  const read = async () =>
    (await getJson<ProgramView>(request, "/api/v1/program")).health.connections;
  let previous: number | null = null;
  let current = await read();
  for (let i = 0; i < 10 && !receiversSettled(previous, current); i++) {
    await sleep(1_500);
    previous = current;
    current = await read();
  }
  return current;
}

/** Kill a child AND its children (python -> ffmpeg), which keep the recording open. */
function killTree(child: ChildProcess): void {
  if (child.exitCode !== null || child.pid === undefined) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true });
  } else {
    child.kill("SIGKILL");
  }
}

/**
 * Delete a recording and, with auto-remux on, its `<base>.mp4` sibling.
 * - Waits up to `siblingWaitMs` for the remux to produce the sibling.
 * - With `keep` (a take that did not pass), copies the files into the
 *   evidence dir first.
 * - Retries while OBS still holds a file (Windows EBUSY/EPERM).
 * - Re-checks that nothing is left.
 * Returns the paths still present. A sibling that never appeared is logged
 * (it may be created later; afterAll sweeps again).
 */
async function removeRecording(
  outputPath: string,
  autoRemux: boolean,
  siblingWaitMs: number,
  keep: Evidence | null = null,
): Promise<string[]> {
  const files = recordingFiles(outputPath, autoRemux);
  const siblingDeadline = Date.now() + siblingWaitMs;
  if (files.length > 1) {
    while (!fs.existsSync(files[1]) && Date.now() < siblingDeadline) await sleep(500);
    if (!fs.existsSync(files[1])) {
      console.warn(`A/V gate: auto-remux is on but ${files[1]} has not appeared (yet)`);
    }
  }
  if (keep) await keepRecording(files.filter((f) => fs.existsSync(f)), keep, siblingDeadline);
  for (const f of files) {
    const deadline = Date.now() + 10_000;
    for (;;) {
      try {
        fs.rmSync(f, { force: true });
        break;
      } catch (e) {
        if (Date.now() >= deadline) {
          console.error(`A/V gate: could not delete ${f}: ${e}`);
          break;
        }
        await sleep(500);
      }
    }
  }
  return files.filter((f) => fs.existsSync(f));
}

test.describe("post-deploy A/V sync + dropout gate (#147)", () => {
  // The scene driver (SongPlayer's facade) and the recorder (cg OBS).
  let scenes: ObsDriver | null = null;
  let recorder: ObsDriver | null = null;
  let initialScene: string | null = null;
  // #221 lane 3: cg OBS's own program scene before the gate, restored in
  // afterAll once the gate switched it to the probe scene (`cgSwitched`), and
  // whether the probe was pointed at SP-program (idled again in afterAll).
  let cgInitialScene: string | null = null;
  let cgSwitched = false;
  let probePointed = false;
  // cg OBS was ON the probe scene before the gate (a dead run): the body
  // fails at its start (nothing to restore it to), afterAll idles the probe.
  let cgStuckOnProbe = false;
  let autoRemux = false;
  // Cleanup state shared with afterAll. A timed-out test body never reaches
  // its own finally, so afterAll finishes whatever is still marked here.
  let recordingOurs = false;
  // An in-flight StartRecord. afterAll awaits it: if the body timed out while
  // OBS was starting, the recording is ours even though `recordingOurs` was
  // never set. A REJECTED start (e.g. an operator recording was running) is
  // never ours to stop.
  let startInFlight: Promise<void> | null = null;
  // Recordings the body's own per-take cleanup already handled.
  const removedByBody = new Set<string>();
  const madeRecordings: string[] = [];
  let liveChild: ChildProcess | null = null;
  let fadersToRestore: { vokaly: number; podklad: number } | null = null;
  // Set first thing in afterAll. Playwright does not cancel a timed-out body,
  // so the body checks this before starting a recording or skipping a song.
  let tornDown = false;
  const assertNotTornDown = (what: string) => {
    if (tornDown) throw new Error(`A/V gate torn down (test timed out) — not starting ${what}`);
  };

  async function restoreFaders(request: APIRequestContext): Promise<void> {
    if (!fadersToRestore) return;
    const r = await request.patch("/api/v1/mix", { data: { kind: "song", ...fadersToRestore } });
    expect(r.status(), "restore the SONG faders").toBe(200);
    fadersToRestore = null;
  }

  /** Run the analysis without blocking the event loop (OBS-ws, Playwright timeout). */
  function runAnalysis(args: string[]): Promise<{ code: number | null; stdout: string; stderr: string }> {
    return new Promise((resolve, reject) => {
      if (tornDown) {
        reject(new Error("A/V gate torn down (test timed out) — not starting the analysis"));
        return;
      }
      const child = spawn(PYTHON, [SCRIPT, ...args], { windowsHide: true });
      liveChild = child;
      let stdout = "";
      let stderr = "";
      child.stdout!.on("data", (d: Buffer) => (stdout += d.toString()));
      child.stderr!.on("data", (d: Buffer) => (stderr += d.toString()));
      const timer = setTimeout(() => {
        killTree(child);
        reject(new Error(`av_sync_check.py did not finish within ${ANALYSIS_TIMEOUT_MS} ms\n${stderr}`));
      }, ANALYSIS_TIMEOUT_MS);
      child.on("error", (e: Error) => {
        clearTimeout(timer);
        liveChild = null;
        reject(e);
      });
      child.on("close", (code: number | null) => {
        clearTimeout(timer);
        liveChild = null;
        resolve({ code, stdout, stderr });
      });
    });
  }

  test.beforeAll(async () => {
    scenes = await ObsDriver.connect(FACADE_WS_URL);
    recorder = await ObsDriver.connect(OBS_WS_URL);
    try {
      initialScene = await scenes.currentProgramScene();
    } catch {
      // #221 L3: the facade answers 604 while nothing is on SP-program; then
      // there is no scene to restore (afterAll skips the restore).
      initialScene = null;
    }
    cgInitialScene = await recorder.currentProgramScene();
    cgStuckOnProbe = cgInitialScene === AV_PROBE_SCENE;
    autoRemux = await recorder.autoRemuxEnabled();
  });

  test.afterAll(async () => {
    tornDown = true;
    // Worst case: start settle 10 + stop 10 + faders/scene ~10 + deleting up
    // to MAX_TAKES+1 recordings (15 s remux wait + 2 x 10 s busy retries).
    test.setTimeout(180_000);
    const driver = scenes;
    const rec = recorder;
    if (!driver || !rec) {
      await driver?.disconnect();
      await rec?.disconnect();
      return;
    }
    const errors: string[] = [];
    const step = async (what: string, fn: () => Promise<void>) => {
      try {
        await fn();
      } catch (e) {
        errors.push(`${what}: ${e}`);
      }
    };
    try {
      await step("kill the analysis", async () => {
        if (liveChild) killTree(liveChild);
      });
      let ours = recordingOurs;
      const pendingStart = startInFlight;
      await step("settle an in-flight StartRecord", async () => {
        if (!pendingStart) return;
        const settled = await Promise.race([
          pendingStart.then(
            () => "started",
            () => "rejected", // nothing of ours started
          ),
          sleep(10_000).then(() => "pending"),
        ]);
        if (settled === "started") ours = true;
        if (settled === "pending") {
          // The call never answered, but it was SENT after the pre-check proved
          // no operator recording was running: an active recording is ours.
          if (rec.startIssued && (await rec.isRecording())) ours = true;
          throw new Error("StartRecord did not settle within 10 s");
        }
      });
      // Paths stopped HERE have not been remuxed yet: wait for their sibling.
      const stoppedHere: string[] = [];
      await step("stop our recording", async () => {
        if (ours && (await rec.isRecording())) stoppedHere.push(await rec.stopRecord());
      });
      // Restore the operator's wall BEFORE the slow file deletion, so a hook
      // that runs out of time never leaves the program on the baseline.
      await step("restore the SONG faders", async () => {
        if (!fadersToRestore) return;
        const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
        try {
          await restoreFaders(ctx);
        } finally {
          await ctx.dispose();
        }
      });
      // #221 lane 3: idle the probe FIRST. An idle probe holds no receiver on
      // SP-program (DistroAV keeps one whether the input is shown or not) and
      // shows nothing, so the program restore below can never loop the
      // picture through cg OBS. Also after a dead run that left cg OBS on the
      // probe scene (`cgStuckOnProbe`, the body failed at its start).
      await step("idle the probe", async () => {
        if (!probePointed && !cgStuckOnProbe) return;
        await rec.setInputSettings(AV_PROBE_INPUT, { ndi_source_name: "" });
        probePointed = false;
      });
      await step("restore the program scene", async () => {
        if (!initialScene) return;
        await driver.switchScene(initialScene);
        expect(
          await driver.currentProgramScene(),
          `the A/V gate must restore the program scene "${initialScene}"`,
        ).toBe(initialScene);
      });
      // Then cg OBS's own scene, guarded: a manual scene the program restore
      // put back is already on cg OBS (the facade sets it there), so no
      // same-scene studio transition on cg OBS (its 2 s self-fade, and the
      // #170 dropped-next-event state).
      await step("restore cg OBS's program scene", async () => {
        if (!cgSwitched || !cgInitialScene) return;
        if ((await rec.currentProgramScene()) !== cgInitialScene) {
          await rec.switchScene(cgInitialScene);
        }
        expect(
          await rec.currentProgramScene(),
          `the A/V gate must restore cg OBS's program scene "${cgInitialScene}"`,
        ).toBe(cgInitialScene);
      });
      await step("delete the recordings", async () => {
        // A StopRecord whose inactive-poll timed out still left its path.
        const last = rec.lastRecordingPath;
        if (ours && last && !madeRecordings.includes(last) && !stoppedHere.includes(last)) {
          stoppedHere.push(last);
        }
        recordingOurs = false;
        const left: string[] = [];
        // Not yet deleted by the body (it died first): wait for the remux sibling.
        const pending = [...stoppedHere, ...madeRecordings.filter((r) => !removedByBody.has(r))];
        for (const rec of pending) left.push(...(await removeRecording(rec, autoRemux, 15_000)));
        // Deleted by the body: a re-sweep catches a sibling that appeared late.
        for (const rec of removedByBody) left.push(...(await removeRecording(rec, autoRemux, 0)));
        expect(left, "every OBS recording the gate made must be deleted").toEqual([]);
      });
    } finally {
      await driver.disconnect();
      await rec.disconnect();
    }
    expect(errors, "A/V gate cleanup").toEqual([]);
  });

  test("OBS program recording is in lipsync with the original and has no audio dropouts", async ({
    request,
  }, testInfo) => {
    test.setTimeout(TEST_TIMEOUT_MS);
    const testStart = Date.now();
    expect(scenes, "the facade's scene driver must be connected").not.toBeNull();
    expect(recorder, "cg OBS's recording driver must be connected").not.toBeNull();
    const driver = scenes!;
    const rec = recorder!;
    for (const [label, p] of [
      ["analysis script", SCRIPT],
      ["python (SP_AVSYNC_PYTHON)", PYTHON],
      ["ffmpeg (SP_FFMPEG)", FFMPEG],
    ]) {
      expect(fs.existsSync(p), `${label} must exist at ${p}`).toBe(true);
    }
    // #221 lane 3: cg OBS left on the probe scene (a run that died mid-take,
    // a genlock soak, a hand press) has no scene of the owner's to restore
    // it to (afterAll idles the probe), so fail before touching anything.
    expect(
      cgStuckOnProbe,
      `cg OBS is on the A/V gate's probe scene "${AV_PROBE_SCENE}" (a previous run that died mid-take, a genlock soak or a hand press left it there) — put cg OBS back on its own scene`,
    ).toBe(false);

    // 1. Put the baseline sp-* output on program.
    const baseline = pickBaselineScene(await driver.listScenes());
    expect(
      baseline.startsWith("sp-"),
      `baseline scene must be an sp-* output, got "${baseline}"`,
    ).toBe(true);
    await driver.switchScene(baseline);
    // #221 L4b: the playback authority puts the baseline's playlist on air
    // a moment after the facade's switch; wait until it alone is on air.
    const status = await pollUntil(
      `engine active_scene=${baseline} with its playlist alone on air`,
      10_000,
      () =>
        getJson<{ active_scene: string | null; active_playlist_ids: number[] }>(
          request,
          "/api/v1/status",
        ),
      (s) => s.active_scene === baseline && s.active_playlist_ids.length === 1,
    );
    // #221 lane 3: the take records SP-program through cg OBS's probe scene
    // (the file doc), and only while SP-program carries the baseline playlist
    // (never "OBS manuál": cg OBS would record itself).
    const baselinePid = status.active_playlist_ids[0];
    const program = await getJson<ProgramView>(request, "/api/v1/program");
    expect(
      programCarriesBaseline(program.source, baselinePid),
      `SP-program must carry the baseline playlist ${baselinePid} (got source ${program.source})`,
    ).toBe(true);
    const probeSource = await ensureProbeScene(rec);
    // The probe is idle now: SP-program's receivers without it.
    const receiversBefore = await settledReceivers(request);
    assertNotTornDown("pointing the probe at SP-program");
    probePointed = true;
    await rec.setInputSettings(AV_PROBE_INPUT, { ndi_source_name: probeSource });
    if ((await rec.currentProgramScene()) !== AV_PROBE_SCENE) {
      assertNotTornDown("cg OBS's probe scene switch");
      cgSwitched = true;
      await rec.switchScene(AV_PROBE_SCENE);
    }
    // Wait until cg OBS's probe receiver is on SP-program (its receivers
    // rose), else fail naming it, never as an unmeasurable recording.
    await pollUntil(
      `cg OBS's probe receiver on ${probeSource} (SP-program's receivers above ${receiversBefore})`,
      30_000,
      async () => (await getJson<ProgramView>(request, "/api/v1/program")).health.connections,
      (now) => probeReceiverAttached(receiversBefore, now),
    );
    const active = status.active_playlist_ids;
    const first = await getJson<HealthRow[]>(request, "/api/v1/ndi/health");
    if (!active.some((id) => isPlayingWithFrames(first, id))) {
      // The scene switch normally starts playback; nudge once if it is paused.
      await request.post(`/api/v1/playback/${active[0]}/play`);
    }
    const waitPlaying = () =>
      pollUntil(
        `an on-program playlist of ${JSON.stringify(active)} Playing with frames_submitted_last_5s > 0`,
        30_000,
        () => getJson<HealthRow[]>(request, "/api/v1/ndi/health"),
        (h) => active.some((id) => isPlayingWithFrames(h, id)),
      );
    const health = await waitPlaying();
    const playlistId = active.find((id) => isPlayingWithFrames(health, id))!;
    const out = health.find((h) => h.playlist_id === playlistId)!;
    console.log(
      `A/V gate: scene=${baseline} playlist=${playlistId} pipeline=${out.ndi_name} ` +
        `recorded=${AV_PROBE_SCENE} (${probeSource}) ` +
        `frames_5s=${out.frames_submitted_last_5s} autoRemux=${autoRemux}`,
    );

    const cacheDir = (await getJson<{ cache_dir: string }>(request, "/api/v1/settings"))
      .cache_dir;
    const currentVideo = async () =>
      nowPlayingVideoId(await getJson<MixNowPlaying>(request, "/api/v1/mix"), playlistId);

    // 2. Unity SONG faders for the measurement (restored in finally/afterAll).
    const mixBefore = await getJson<{ song?: { vokaly?: number; podklad?: number } }>(
      request,
      "/api/v1/mix",
    );
    const vokaly = mixBefore.song?.vokaly ?? 1.0;
    const podklad = mixBefore.song?.podklad ?? 1.0;
    if (vokaly !== 1.0 || podklad !== 1.0) {
      fadersToRestore = { vokaly, podklad };
      const r = await request.patch("/api/v1/mix", {
        data: { kind: "song", vokaly: 1.0, podklad: 1.0 },
      });
      expect(r.status(), "PATCH /api/v1/mix to unity").toBe(200);
    }

    try {
      let run: AvSyncRun | null = null;
      let lastTakeNote = "";
      const undeleted: string[] = [];
      for (let take = 1; take <= MAX_TAKES; take++) {
        // 3. The probe's AUDIO must flow first (#221 dev.18, the file doc): a
        // take started inside the receiver's audio warm-up opens with
        // dropouts that are not SongPlayer's. Waited before the video is
        // read, so the song read is the one playing at StartRecord.
        assertNotTornDown("the probe audio wait");
        const audio = await rec.waitForInputAudio(AV_PROBE_INPUT);
        console.log(
          `A/V gate take ${take}: probe audio flowing after ${Math.round(audio.waitedMs)} ms ` +
            `(${audio.withInput}/${audio.events} meter events with the probe, ` +
            `loudest ${audio.loudestDbfs.toFixed(1)} dBFS)`,
        );

        // Which video is playing, and its ORIGINAL sidecars.
        const videoId = await currentVideo();
        expect(videoId, `/api/v1/mix now_playing must name playlist ${playlistId}'s video`).not.toBeNull();
        const videos = await getJson<Array<{ id: number; youtube_id: string; title: string }>>(
          request,
          `/api/v1/playlists/${playlistId}/videos`,
        );
        const video = videos.find((v) => v.id === videoId);
        expect(video, `video ${videoId} must be listed in playlist ${playlistId}`).toBeTruthy();
        const pair = resolveSidecars(fs.readdirSync(cacheDir), video!.youtube_id);

        // 4. Record the PROGRAM.
        assertNotTornDown("a recording");
        startInFlight = rec.startRecord();
        await startInFlight;
        startInFlight = null;
        recordingOurs = true;
        // afterAll may have begun while OBS was starting: leave the stop to it.
        assertNotTornDown("the recording wait");
        await sleep(RECORD_MS);
        const recording = await rec.stopRecord();
        madeRecordings.push(recording);
        recordingOurs = false;
        const after = await currentVideo();
        console.log(
          `A/V gate take ${take}: video ${videoId} "${video!.title}" (${video!.youtube_id}) -> ${recording}`,
        );

        run = null;
        let analysed = false;
        const texts: Array<[string, string]> = [];
        try {
          if (after !== videoId) {
            lastTakeNote = `take ${take} was discarded: the song changed (${videoId} -> ${after}) during the recording`;
            console.log(`A/V gate: ${lastTakeNote}`);
          } else {
            // 5. Analyse against the originals. Every number goes to the CI log.
            assertNotTornDown("the analysis");
            analysed = true;
            let proc: { code: number | null; stdout: string; stderr: string };
            try {
              proc = await runAnalysis([
                "--recording", recording,
                "--orig-audio", path.join(cacheDir, pair.audio),
                "--orig-video", path.join(cacheDir, pair.video),
                "--max-av-ms", String(MAX_AV_MS),
                "--ffmpeg", FFMPEG,
              ]); // prettier-ignore
            } catch (e) {
              texts.push(["analysis-error.txt", String(e)]);
              throw e;
            }
            texts.push(["av_sync.json", proc.stdout], ["av_sync.stderr.txt", proc.stderr]);
            console.log(proc.stdout);
            console.log(proc.stderr.trim().split(/\r?\n/).slice(-15).join("\n"));
            run = classifyAvSyncRun(proc.code, proc.stdout);
            lastTakeNote = `take ${take}: ${run.detail}`;
            console.log(`A/V gate ${lastTakeNote}`);
          }
        } finally {
          // A take that did not pass keeps its recording + analysis as CI
          // evidence (#147). Collected, not asserted here: an assertion in a
          // finally would mask the analysis error that got us here.
          const keep = keepsEvidence(analysed, run)
            ? { dir: path.join(testInfo.outputDir, "av-sync-evidence"), take }
            : null;
          if (keep) for (const [name, text] of texts) keepText(keep, name, text);
          undeleted.push(...(await removeRecording(recording, autoRemux, 15_000, keep)));
          removedByBody.add(recording);
        }

        const retake = run === null || run.retakeable;
        if (!retake || take === MAX_TAKES) break;
        if (Date.now() - testStart > RETAKE_BEFORE_MS) {
          console.log(`A/V gate: no time budget left for another take after take ${take}`);
          break;
        }
        if (run !== null) {
          // The picture was unmeasurable but the audio matched: try another song.
          assertNotTornDown("a song skip");
          const skip = await request.post(`/api/v1/playback/${playlistId}/skip`);
          expect(skip.ok(), `POST /skip for playlist ${playlistId}`).toBe(true);
          await pollUntil(
            `playlist ${playlistId} to move off video ${videoId}`,
            15_000,
            currentVideo,
            (v) => v !== null && v !== videoId,
          );
          await waitPlaying();
        }
      }

      expect(run, `no take produced a measurement — ${lastTakeNote}`).not.toBeNull();
      expect(run!.status, `av_sync_check: ${run!.detail}`).toBe("pass");
      expect(undeleted, "the A/V gate must delete every OBS recording it made").toEqual([]);
    } finally {
      await restoreFaders(request);
    }
  });
});
