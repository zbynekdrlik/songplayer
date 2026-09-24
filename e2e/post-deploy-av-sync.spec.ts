/**
 * Post-deploy A/V sync + audio-dropout gate (#147).
 *
 * The owner reported a MAJOR lipsync regression while every gate was green.
 * This gate measures the REAL output. It records the OBS PROGRAM while an sp-*
 * output plays, then compares the recording with the ORIGINAL cached sidecars
 * (`scripts/av_sync_check.py`: audio cross-correlation, video frame alignment,
 * 10 ms dropout blocks). It FAILS when |A/V| > 40 ms, when any dropout block
 * exists, or when the measurement is not trustworthy (low correlation, match or
 * contrast). A "cannot measure" result is a failure, never a skip.
 *
 * Takes: a take is repeated (up to MAX_TAKES in total) only when the song
 * changed during it, or when it was unmeasurable because of the PICTURE while
 * the audio matched (a still or overlaid video). In the second case the
 * playlist is skipped to the next song first. A take that FAILS is never
 * repeated, and neither is an unmeasurable AUDIO side.
 *
 * OBS discipline (CLAUDE.md): the program goes to the shared baseline scene
 * (sp-slow preferred, never sp-warmup/sp-fast). The scene the operator was on
 * is captured first and restored after. Every recording file (plus its
 * auto-remux sibling) is deleted, and an operator's own running recording is
 * never touched (`startRecord` refuses). The SONG mixer faders are set to
 * unity for the measurement and restored after. afterAll restores the faders
 * and stops the recording even if the test body timed out.
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
import { spawn } from "child_process";
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
const ANALYSIS_TIMEOUT_MS = 90_000;
const MAX_TAKES = 3;

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
    await new Promise((r) => setTimeout(r, 500));
    last = await read();
  }
  return last;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Run the analysis without blocking the event loop (OBS-ws, Playwright timeout). */
function runAnalysis(args: string[]): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn(PYTHON, [SCRIPT, ...args], { windowsHide: true });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`av_sync_check.py did not finish within ${ANALYSIS_TIMEOUT_MS} ms\n${stderr}`));
    }, ANALYSIS_TIMEOUT_MS);
    child.on("error", (e) => {
      clearTimeout(timer);
      reject(e);
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr });
    });
  });
}

/**
 * Delete a recording and, with auto-remux on, its `<base>.mp4` sibling. Waits
 * (bounded) for the remux to produce the sibling, and retries while OBS still
 * holds a file (Windows EBUSY/EPERM). Returns the paths it could not remove.
 */
async function removeRecording(outputPath: string, autoRemux: boolean): Promise<string[]> {
  const files = recordingFiles(outputPath, autoRemux);
  if (files.length > 1) {
    const deadline = Date.now() + 15_000;
    while (!fs.existsSync(files[1]) && Date.now() < deadline) await sleep(500);
  }
  const left: string[] = [];
  for (const f of files) {
    const deadline = Date.now() + 10_000;
    for (;;) {
      try {
        fs.rmSync(f, { force: true });
        break;
      } catch (e) {
        if (Date.now() >= deadline) {
          console.error(`A/V gate: could not delete ${f}: ${e}`);
          left.push(f);
          break;
        }
        await sleep(500);
      }
    }
  }
  return left;
}

test.describe("post-deploy A/V sync + dropout gate (#147)", () => {
  let obs: ObsDriver | null = null;
  let initialScene: string | null = null;
  // Cleanup state shared with afterAll: a timed-out test body never reaches
  // its own finally, so afterAll restores whatever is still marked here.
  let recordingOurs = false;
  let autoRemux = false;
  let fadersToRestore: { vokaly: number; podklad: number } | null = null;

  async function restoreFaders(request: APIRequestContext): Promise<void> {
    if (!fadersToRestore) return;
    const r = await request.patch("/api/v1/mix", { data: { kind: "song", ...fadersToRestore } });
    expect(r.status(), "restore the SONG faders").toBe(200);
    fadersToRestore = null;
  }

  test.beforeAll(async () => {
    obs = await ObsDriver.connect(OBS_WS_URL);
    initialScene = await obs.currentProgramScene();
    autoRemux = await obs.autoRemuxEnabled();
  });

  test.afterAll(async () => {
    const driver = obs;
    if (!driver) return;
    const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
    try {
      if (recordingOurs) {
        const leftover = await driver.stopRecord();
        recordingOurs = false;
        await removeRecording(leftover, autoRemux);
      }
      await restoreFaders(ctx);
      if (initialScene) {
        await driver.switchScene(initialScene);
        expect(
          await driver.currentProgramScene(),
          `the A/V gate must restore the program scene "${initialScene}"`,
        ).toBe(initialScene);
      }
    } finally {
      await ctx.dispose();
      await driver.disconnect();
    }
  });

  test("OBS program recording is in lipsync with the original and has no audio dropouts", async ({
    request,
  }) => {
    test.setTimeout(240_000);
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
        await driver.startRecord();
        recordingOurs = true;
        await sleep(RECORD_MS);
        const recording = await driver.stopRecord();
        recordingOurs = false;
        const after = await currentVideo();
        console.log(
          `A/V gate take ${take}: video ${videoId} "${video!.title}" (${video!.youtube_id}) -> ${recording}`,
        );

        try {
          if (after !== videoId) {
            console.log(`A/V gate take ${take}: the song changed (${videoId} -> ${after}) during the recording`);
            run = null;
            continue;
          }
          // 5. Analyse against the originals. Every number goes to the CI log.
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
          console.log(`A/V gate take ${take}: ${run.detail}`);
        } finally {
          // Collected, not asserted here: an assertion in a finally would mask
          // the analysis error that got us here.
          undeleted.push(...(await removeRecording(recording, autoRemux)));
        }

        if (!run!.retakeable || take === MAX_TAKES) break;
        // The picture was unmeasurable but the audio matched: try another song.
        await request.post(`/api/v1/playback/${playlistId}/skip`);
        await pollUntil(
          `playlist ${playlistId} to move off video ${videoId}`,
          15_000,
          currentVideo,
          (v) => v !== null && v !== videoId,
        );
        await waitPlaying();
      }

      expect(run, `no take could be analysed in ${MAX_TAKES} attempts (the song kept changing)`).not.toBeNull();
      expect(run!.status, `av_sync_check: ${run!.detail}`).toBe("pass");
      expect(undeleted, "the A/V gate must delete every OBS recording it made").toEqual([]);
    } finally {
      await restoreFaders(request);
    }
  });
});
