/**
 * The decision of the YouTube live gate (#232, `post-deploy-youtube.spec.ts`),
 * as a pure function over the box's answer so the mock suite tests it
 * (`youtube-gate.spec.ts`).
 *
 * Every song comes from YouTube through the box's yt-dlp, its cookie file
 * (#141: YouTube asks an anonymous request to sign in) and its deno (the
 * n-challenge, #189). The only check was the deno self-check, which passes
 * on a bot check, so expired cookies or a YouTube change yt-dlp cannot
 * extract failed every download with CI green (the owner's rule, 29.9.2026:
 * every external boundary gets a live post-deploy check).
 *
 * `POST /api/v1/youtube/probe` resolves one fixed real video exactly as a
 * download does (the production selector at the live cap, the cookies) and
 * downloads nothing (`downloader::probe`).
 */

/** The fixed video: 1080p on YouTube (10.10.2026: AV1 1920×1080 at 30). */
export const YOUTUBE_PROBE_VIDEO = "gq-4FVRr_ow";

/** The fewest rows the probe's pick may have: the video has 1080p, so a
 * smaller pick is a selector that lost the tiers (D8's 360p VP9). */
export const YOUTUBE_MIN_HEIGHT = 720;

/** The format the probe resolved (`downloader::format::DownloadedFormat`). */
export interface ProbedFormat {
  format_id: string;
  codec: string | null;
  width: number | null;
  height: number | null;
  fps: number | null;
}

/** `POST /api/v1/youtube/probe`'s answer (`downloader::probe::YoutubeProbeReport`). */
export interface YoutubeProbeReport {
  ok: boolean;
  youtube_id: string;
  cap: number;
  cookies: boolean;
  format: ProbedFormat | null;
  error: string | null;
  elapsed_ms: number;
}

/** Why the gate fails (empty = it passes). */
export function youtubeGateFailures(r: YoutubeProbeReport): string[] {
  const failures: string[] = [];
  if (r.youtube_id !== YOUTUBE_PROBE_VIDEO) {
    failures.push(`probed ${r.youtube_id}, not ${YOUTUBE_PROBE_VIDEO}`);
  }
  if (!r.cookies) {
    failures.push("no cookie file on the box: YouTube asks every anonymous download to sign in (#141)");
  }
  if (!r.ok || r.format === null) {
    failures.push(`yt-dlp resolved no format: ${r.error ?? "no error given"}`);
    return failures;
  }
  const height = r.format.height ?? 0;
  if (height < Math.min(YOUTUBE_MIN_HEIGHT, r.cap)) {
    failures.push(
      `the selector picked ${r.format.format_id} at ${height} rows, under ${YOUTUBE_MIN_HEIGHT} (the video has 1080p)`,
    );
  }
  return failures;
}
