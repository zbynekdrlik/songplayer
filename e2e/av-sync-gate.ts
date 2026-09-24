/**
 * Pure decision helpers for the post-deploy A/V sync + dropout gate (#147).
 *
 * The gate itself (`post-deploy-av-sync.spec.ts`) runs on the win-resolume
 * runner. These helpers are unit-tested in the ubuntu mock suite
 * (`av-sync-gate.spec.ts`) with no box and no browser.
 */

export interface HealthRow {
  playlist_id: number;
  ndi_name?: string;
  state: string;
  frames_submitted_last_5s: number;
}

/**
 * The output is PLAYING and actually emitting frames: its reconciled health
 * `state` is `Playing` (which already means "on OBS program", see
 * obs-ndi-health.md #154), and it submitted frames in the last 5 s.
 */
export function isPlayingWithFrames(health: HealthRow[], playlistId: number): boolean {
  const row = health.find((h) => h.playlist_id === playlistId);
  return !!row && row.state === "Playing" && row.frames_submitted_last_5s > 0;
}

export interface MixNowPlaying {
  now_playing?: Array<{ playlist_id: number; video_id: number }>;
}

/** The video id playing on `playlistId` per `GET /api/v1/mix`, or null. */
export function nowPlayingVideoId(mix: MixNowPlaying, playlistId: number): number | null {
  const entry = (mix.now_playing ?? []).find((e) => e.playlist_id === playlistId);
  return entry ? entry.video_id : null;
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Resolve a video's ORIGINAL cached sidecar pair from the cache directory
 * listing. The pair is named
 * `{song}_{artist}_{youtube_id}_normalized[_gf]_{video.mp4|audio.flac}`. The
 * videos API does not expose `file_path`, so the gate resolves the pair by
 * this naming.
 *
 * Exactly ONE complete pair must exist. A missing half or two candidate pairs
 * throws, and the message names what was found. The gate never guesses which
 * file is playing.
 */
export function resolveSidecars(
  fileNames: string[],
  youtubeId: string,
): { video: string; audio: string } {
  const re = new RegExp(`_${escapeRegExp(youtubeId)}_normalized(?:_gf)?_video\\.mp4$`);
  const names = new Set(fileNames);
  const videos = fileNames.filter((n) => re.test(n));
  const pairs = videos
    .map((v) => ({ video: v, audio: v.replace(/_video\.mp4$/, "_audio.flac") }))
    .filter((p) => names.has(p.audio));
  if (pairs.length !== 1) {
    const related = fileNames.filter((n) => n.includes(`_${youtubeId}_normalized`));
    throw new Error(
      `expected exactly one cached video+audio sidecar pair for youtube id "${youtubeId}", ` +
        `found ${pairs.length}; related files: ${JSON.stringify(related)}`,
    );
  }
  return pairs[0];
}

export type AvSyncStatus = "pass" | "fail" | "cannot_measure" | "error";

export interface AvSyncRun {
  status: AvSyncStatus;
  /** One human line for the assertion message. */
  detail: string;
  /** True only for a cannot-measure caused by the PICTURE (the audio matched):
   * a still or overlaid video. Another song may be measurable. An audio-side
   * cannot-measure is never retaken, because it can be a real audio fault. */
  retakeable: boolean;
}

const EXPECTED_EXIT: Record<string, number> = { pass: 0, fail: 1, cannot_measure: 2 };
const MIN_AUDIO_CORR = 0.9; // scripts/av_sync_check.py MIN_AUDIO_CORR

/**
 * Classify one `scripts/av_sync_check.py` run from its stdout JSON AND its exit
 * code. They must agree. A missing or unparseable JSON, or a code that
 * contradicts it, means the analysis itself did not run properly. That is
 * reported as `error` (e.g. a numpy import failure exits 1 and an argparse
 * error exits 2, and neither is a measurement).
 */
export function classifyAvSyncRun(code: number | null, stdout: string): AvSyncRun {
  let parsed: {
    status?: string;
    reasons?: string[];
    av_ms?: number;
    audio?: { corr?: number };
  };
  try {
    parsed = JSON.parse(stdout);
  } catch {
    return {
      status: "error",
      detail: `analysis produced no JSON (exit ${code}) — it failed to run`,
      retakeable: false,
    };
  }
  const status = parsed.status ?? "";
  if (!(status in EXPECTED_EXIT) || EXPECTED_EXIT[status] !== code) {
    return {
      status: "error",
      detail: `analysis status "${status}" disagrees with exit ${code} — it failed to run`,
      retakeable: false,
    };
  }
  const reasons = (parsed.reasons ?? []).join("; ");
  if (status === "pass") {
    return { status: "pass", detail: `pass (A/V ${parsed.av_ms} ms)`, retakeable: false };
  }
  if (status === "fail") {
    return { status: "fail", detail: `FAIL: ${reasons}`, retakeable: false };
  }
  const audioOk = (parsed.audio?.corr ?? 0) >= MIN_AUDIO_CORR;
  return {
    status: "cannot_measure",
    detail: `CANNOT MEASURE (a gate failure, never a skip): ${reasons}`,
    retakeable: audioOk,
  };
}

/**
 * Every file one OBS recording can leave behind. With "Automatically remux to
 * mp4" on (OBS profile `Video/AutoRemux`), a non-mp4 recording also gets a
 * sibling `<base>.mp4`.
 */
export function recordingFiles(outputPath: string, autoRemux: boolean): string[] {
  const m = outputPath.match(/^(.*)\.([^.\\/]+)$/);
  if (!autoRemux || !m || m[2].toLowerCase() === "mp4") return [outputPath];
  return [outputPath, `${m[1]}.mp4`];
}
