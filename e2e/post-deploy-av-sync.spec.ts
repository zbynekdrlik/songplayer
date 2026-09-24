/**
 * Post-deploy A/V sync + audio-dropout gate (#147).
 *
 * The owner reported a MAJOR lipsync regression while every gate was green.
 * This gate measures the REAL output. It records the OBS PROGRAM while an sp-*
 * output plays, then compares the recording with the ORIGINAL cached sidecars
 * (`scripts/av_sync_check.py`: audio cross-correlation, video frame alignment,
 * 50 ms dropout blocks). It FAILS when |A/V| > 40 ms, when any dropout block
 * exists, or when the measurement is not trustworthy (low correlation, match or
 * contrast). A "cannot measure" result is a failure, never a skip.
 *
 * OBS discipline (CLAUDE.md): the program goes to the shared baseline scene
 * (sp-slow preferred, never sp-warmup/sp-fast). The scene the operator was on
 * is captured first and restored after. The recording file is always deleted,
 * and an operator's own running recording is never touched (`startRecord`
 * refuses). The SONG mixer faders are set to unity for the measurement and
 * restored after, so the output is comparable to the original.
 *
 * Box paths (override via env): `SP_AVSYNC_PYTHON` = a Python with numpy (the
 * lyrics venv), `SP_FFMPEG` = the app's bundled ffmpeg. There is no ffprobe on
 * the box, and the script does not need one.
 */

import { test, expect, type APIRequestContext } from "@playwright/test";
import { spawnSync } from "child_process";
import * as fs from "fs";
import * as path from "path";
import { ObsDriver } from "./obs-driver";
import { pickBaselineScene } from "./obs-baseline-scene";
import {
  describeAvSyncExit,
  isPlayingWithFrames,
  nowPlayingVideoId,
  resolveSidecars,
  type HealthRow,
  type MixNowPlaying,
} from "./av-sync-gate";

const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";
const PYTHON =
  process.env.SP_AVSYNC_PYTHON ||
  "C:\\ProgramData\\SongPlayer\\cache\\tools\\lyrics_venv\\Scripts\\python.exe";
const FFMPEG = process.env.SP_FFMPEG || "C:\\ProgramData\\SongPlayer\\cache\\tools\\ffmpeg.exe";
const SCRIPT = path.resolve(__dirname, "..", "scripts", "av_sync_check.py");

const MAX_AV_MS = 40;
const RECORD_MS = 20_000;
// A song change during the recording makes the comparison meaningless (two
// originals). It is detected by the video id and the take is re-recorded once.
const MAX_TAKES = 2;

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

test.describe("post-deploy A/V sync + dropout gate (#147)", () => {
  let obs: ObsDriver | null = null;
  let initialScene: string | null = null;
  // True from our StartRecord until our StopRecord returns. afterAll stops the
  // recording if the test was cut off in between (a timed-out test body never
  // reaches its own finally).
  let recordingOurs = false;

  test.beforeAll(async () => {
    obs = await ObsDriver.connect(OBS_WS_URL);
    initialScene = await obs.currentProgramScene();
  });

  test.afterAll(async () => {
    const driver = obs;
    if (!driver) return;
    try {
      if (recordingOurs) {
        const leftover = await driver.stopRecord();
        recordingOurs = false;
        fs.rmSync(leftover, { force: true });
      }
      if (initialScene) {
        await driver.switchScene(initialScene);
        expect(
          await driver.currentProgramScene(),
          `the A/V gate must restore the program scene "${initialScene}"`,
        ).toBe(initialScene);
      }
    } finally {
      await driver.disconnect();
    }
  });

  test("OBS program recording is in lipsync with the original and has no audio dropouts", async ({
    request,
  }) => {
    test.setTimeout(180_000);
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();
    const driver = obs!;
    for (const [label, p] of [
      ["analysis script", SCRIPT],
      ["python (SP_AVSYNC_PYTHON)", PYTHON],
      ["ffmpeg (SP_FFMPEG)", FFMPEG],
    ]) {
      expect(fs.existsSync(p), `${label} must exist at ${p}`).toBe(true);
    }

    // 1. Put the baseline sp-* output on program and prove it is PLAYING.
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
    const health = await pollUntil(
      `an on-program playlist of ${JSON.stringify(active)} Playing with frames_submitted_last_5s > 0`,
      30_000,
      () => getJson<HealthRow[]>(request, "/api/v1/ndi/health"),
      (h) => active.some((id) => isPlayingWithFrames(h, id)),
    );
    const playlistId = active.find((id) => isPlayingWithFrames(health, id))!;
    const out = health.find((h) => h.playlist_id === playlistId)!;
    console.log(
      `A/V gate: scene=${baseline} playlist=${playlistId} ndi=${out.ndi_name} ` +
        `frames_5s=${out.frames_submitted_last_5s}`,
    );

    const cacheDir = (await getJson<{ cache_dir: string }>(request, "/api/v1/settings"))
      .cache_dir;

    // 2. Unity SONG faders for the measurement (restored in finally).
    const mixBefore = await getJson<{ song?: { vokaly?: number; podklad?: number } }>(
      request,
      "/api/v1/mix",
    );
    const vokaly = mixBefore.song?.vokaly ?? 1.0;
    const podklad = mixBefore.song?.podklad ?? 1.0;
    const needUnity = vokaly !== 1.0 || podklad !== 1.0;

    let recording: string | null = null;
    try {
      if (needUnity) {
        const r = await request.patch("/api/v1/mix", {
          data: { kind: "song", vokaly: 1.0, podklad: 1.0 },
        });
        expect(r.status(), "PATCH /api/v1/mix to unity").toBe(200);
      }

      let result: { code: number | null; stdout: string; stderr: string } | null = null;
      for (let take = 1; take <= MAX_TAKES && result === null; take++) {
        // 3. Which video is playing, and its ORIGINAL sidecars.
        const videoId = nowPlayingVideoId(
          await getJson<MixNowPlaying>(request, "/api/v1/mix"),
          playlistId,
        );
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
        await new Promise((r) => setTimeout(r, RECORD_MS));
        recording = await driver.stopRecord();
        recordingOurs = false;
        const after = nowPlayingVideoId(
          await getJson<MixNowPlaying>(request, "/api/v1/mix"),
          playlistId,
        );
        console.log(
          `A/V gate take ${take}: video ${videoId} "${video!.title}" (${video!.youtube_id}) ` +
            `-> ${recording}`,
        );
        if (after !== videoId) {
          console.log(
            `A/V gate take ${take}: song changed (${videoId} -> ${after}) during the recording; re-recording`,
          );
          fs.rmSync(recording, { force: true });
          recording = null;
          continue;
        }

        // 5. Analyse against the originals.
        const proc = spawnSync(
          PYTHON,
          [
            SCRIPT,
            "--recording", recording,
            "--orig-audio", path.join(cacheDir, pair.audio),
            "--orig-video", path.join(cacheDir, pair.video),
            "--max-av-ms", String(MAX_AV_MS),
            "--ffmpeg", FFMPEG,
          ],
          { encoding: "utf8", timeout: 90_000, windowsHide: true },
        );
        if (proc.error) throw proc.error;
        result = { code: proc.status, stdout: proc.stdout, stderr: proc.stderr };
      }
      expect(result, `the song changed during all ${MAX_TAKES} recordings`).not.toBeNull();

      // Every number, always, in the CI log.
      console.log(result!.stdout);
      console.log(result!.stderr.trim().split(/\r?\n/).slice(-15).join("\n"));
      expect(
        result!.code,
        `av_sync_check: ${describeAvSyncExit(result!.code)}\n${result!.stdout}`,
      ).toBe(0);
      const parsed = JSON.parse(result!.stdout) as { status: string; av_ms: number };
      expect(parsed.status).toBe("pass");
      expect(Math.abs(parsed.av_ms)).toBeLessThanOrEqual(MAX_AV_MS);
    } finally {
      if (recordingOurs) {
        recording = await driver.stopRecord();
        recordingOurs = false;
      }
      if (recording) fs.rmSync(recording, { force: true });
      if (needUnity) {
        await request.patch("/api/v1/mix", { data: { kind: "song", vokaly, podklad } });
      }
    }
  });
});
