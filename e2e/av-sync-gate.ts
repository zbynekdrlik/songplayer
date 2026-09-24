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

/** Human meaning of `scripts/av_sync_check.py`'s exit code. */
export function describeAvSyncExit(code: number | null): string {
  switch (code) {
    case 0:
      return "pass";
    case 1:
      return "FAIL (|A/V| over the limit or an audio dropout)";
    case 2:
      return "CANNOT MEASURE (low correlation/match/contrast or an analysis error) — a gate failure, never a skip";
    default:
      return `analysis process failed to run (exit ${code})`;
  }
}
