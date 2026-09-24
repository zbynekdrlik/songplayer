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
 * is captured first and restored after. Every recording file (plus its
 * auto-remux sibling) is deleted, and an operator's own running recording is
 * never touched (`startRecord` refuses). The SONG mixer faders are set to
 * unity for the measurement and restored after.
 *
 * afterAll is the safety net for a test body that timed out. In order, it:
 * - kills the analysis;
 * - settles a pending start (at most 10 s);
 * - stops our recording;
 * - restores the faders and the scene;
 * - then deletes every recording made.
 * Every step is attempted even if an earlier one fails.
 *
 * Box paths (override via env): `SP_AVSYNC_PYTHON` = a Python with numpy (the
 * lyrics venv), `SP_FFMPEG` = the app's bundled ffmpeg. There is no ffprobe on
 * the box, and the script does not need one.
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
  classifyAvSyncRun,
  isPlayingWithFrames,
  nowPlayingVideoId,
  recordingFiles,
  resolveSidecars,
  type AvSyncRun,
  type HealthRow,
  type MixNowPlaying,
} from "./av-sync-gate";

const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";
const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";
const PYTHON =
  process.env.SP_AVSYNC_PYTHON ||
  "C:\\ProgramData\\SongPlayer\\cache\\tools\\lyrics_venv\\Scripts\\python.exe";
const FFMPEG = process.env.SP_FFMPEG || "C:\\ProgramData\\SongPlayer\\cache\\tools\\ffmpeg.exe";
const SCRIPT = path.resolve(__dirname, "..", "scripts", "av_sync_check.py");

const MAX_AV_MS = 40;
const RECORD_MS = 20_000;
const ANALYSIS_TIMEOUT_MS = 60_000; // ~5-10 s on the box
const MAX_TAKES = 3;
const TEST_TIMEOUT_MS = 300_000;
// A retake starts only while this much of the budget has been used. A full
// worst-case take (skip 15 + play 30 + record 20 + stop 10 + analysis 60 +
// cleanup 35 s) then still fits within TEST_TIMEOUT_MS.
const RETAKE_BEFORE_MS = 120_000;

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
 * - Retries while OBS still holds a file (Windows EBUSY/EPERM).
 * - Re-checks that nothing is left.
 * Returns the paths still present. A sibling that never appeared is logged
 * (it may be created later; afterAll sweeps again).
 */
async function removeRecording(
  outputPath: string,
  autoRemux: boolean,
  siblingWaitMs: number,
): Promise<string[]> {
  const files = recordingFiles(outputPath, autoRemux);
  if (files.length > 1) {
    const deadline = Date.now() + siblingWaitMs;
    while (!fs.existsSync(files[1]) && Date.now() < deadline) await sleep(500);
    if (!fs.existsSync(files[1])) {
      console.warn(`A/V gate: auto-remux is on but ${files[1]} has not appeared (yet)`);
    }
  }
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
  let obs: ObsDriver | null = null;
  let initialScene: string | null = null;
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
    obs = await ObsDriver.connect(OBS_WS_URL);
    initialScene = await obs.currentProgramScene();
    autoRemux = await obs.autoRemuxEnabled();
  });

  test.afterAll(async () => {
    tornDown = true;
    // Worst case: start settle 10 + stop 10 + faders/scene ~10 + deleting up
    // to MAX_TAKES+1 recordings (15 s remux wait + 2 x 10 s busy retries).
    test.setTimeout(180_000);
    const driver = obs;
    if (!driver) return;
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
          if (driver.startIssued && (await driver.isRecording())) ours = true;
          throw new Error("StartRecord did not settle within 10 s");
        }
      });
      // Paths stopped HERE have not been remuxed yet: wait for their sibling.
      const stoppedHere: string[] = [];
      await step("stop our recording", async () => {
        if (ours && (await driver.isRecording())) stoppedHere.push(await driver.stopRecord());
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
      await step("restore the program scene", async () => {
        if (!initialScene) return;
        await driver.switchScene(initialScene);
        expect(
          await driver.currentProgramScene(),
          `the A/V gate must restore the program scene "${initialScene}"`,
        ).toBe(initialScene);
      });
      await step("delete the recordings", async () => {
        // A StopRecord whose inactive-poll timed out still left its path.
        const last = driver.lastRecordingPath;
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
    }
    expect(errors, "A/V gate cleanup").toEqual([]);
  });

  test("OBS program recording is in lipsync with the original and has no audio dropouts", async ({
    request,
  }) => {
    test.setTimeout(TEST_TIMEOUT_MS);
    const testStart = Date.now();
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();
    const driver = obs!;
    for (const [label, p] of [
      ["analysis script", SCRIPT],
      ["python (SP_AVSYNC_PYTHON)", PYTHON],
      ["ffmpeg (SP_FFMPEG)", FFMPEG],
    ]) {
      expect(fs.existsSync(p), `${label} must exist at ${p}`).toBe(true);
    }

    // 1. Put the baseline sp-* output on program.
    const baseline = pickBaselineScene(await driver.listScenes());
    expect(
      baseline.startsWith("sp-"),
      `baseline scene must be an sp-* output, got "${baseline}"`,
    ).toBe(true);
    await driver.switchScene(baseline);
    const status = await pollUntil(
      `engine active_scene=${baseline} with an active playlist`,
      10_000,
      () =>
        getJson<{ active_scene: string | null; active_playlist_ids: number[] }>(
          request,
          "/api/v1/status",
        ),
      (s) => s.active_scene === baseline && s.active_playlist_ids.length > 0,
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
      `A/V gate: scene=${baseline} playlist=${playlistId} ndi=${out.ndi_name} ` +
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
        // 3. Which video is playing, and its ORIGINAL sidecars.
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
        startInFlight = driver.startRecord();
        await startInFlight;
        startInFlight = null;
        recordingOurs = true;
        // afterAll may have begun while OBS was starting: leave the stop to it.
        assertNotTornDown("the recording wait");
        await sleep(RECORD_MS);
        const recording = await driver.stopRecord();
        madeRecordings.push(recording);
        recordingOurs = false;
        const after = await currentVideo();
        console.log(
          `A/V gate take ${take}: video ${videoId} "${video!.title}" (${video!.youtube_id}) -> ${recording}`,
        );

        run = null;
        try {
          if (after !== videoId) {
            lastTakeNote = `take ${take} was discarded: the song changed (${videoId} -> ${after}) during the recording`;
            console.log(`A/V gate: ${lastTakeNote}`);
          } else {
            // 5. Analyse against the originals. Every number goes to the CI log.
            assertNotTornDown("the analysis");
            const proc = await runAnalysis([
              "--recording", recording,
              "--orig-audio", path.join(cacheDir, pair.audio),
              "--orig-video", path.join(cacheDir, pair.video),
              "--max-av-ms", String(MAX_AV_MS),
              "--ffmpeg", FFMPEG,
            ]); // prettier-ignore
            console.log(proc.stdout);
            console.log(proc.stderr.trim().split(/\r?\n/).slice(-15).join("\n"));
            run = classifyAvSyncRun(proc.code, proc.stdout);
            lastTakeNote = `take ${take}: ${run.detail}`;
            console.log(`A/V gate ${lastTakeNote}`);
          }
        } finally {
          // Collected, not asserted here: an assertion in a finally would mask
          // the analysis error that got us here.
          undeleted.push(...(await removeRecording(recording, autoRemux, 15_000)));
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
