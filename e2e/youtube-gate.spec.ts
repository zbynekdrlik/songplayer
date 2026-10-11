/**
 * Unit tests for the YouTube live gate `post-deploy-youtube.spec.ts` applies
 * on the box (#232). Runs in the ubuntu mock suite (playwright.config.ts):
 * no browser and no deployed box; these `test()` blocks never touch `page`.
 */

import { test, expect } from "@playwright/test";
import { YOUTUBE_MIN_HEIGHT, YoutubeProbeReport, youtubeGateFailures } from "./youtube-gate";

/** SNV's answer of 10.10.2026. */
function resolved(): YoutubeProbeReport {
  return {
    ok: true,
    youtube_id: "gq-4FVRr_ow",
    cap: 1440,
    cookies: true,
    format: { format_id: "399", codec: "av01.0.08M.08", width: 1920, height: 1080, fps: 30 },
    error: null,
    elapsed_ms: 2643,
  };
}

test.describe("YouTube live gate (#232)", () => {
  test("a resolved 1080p format with the cookies passes", () => {
    expect(youtubeGateFailures(resolved())).toEqual([]);
  });

  test("a bot check fails, naming yt-dlp's error", () => {
    const r: YoutubeProbeReport = {
      ...resolved(),
      ok: false,
      format: null,
      error: "ERROR: [youtube] gq-4FVRr_ow: Sign in to confirm you're not a bot",
    };
    expect(youtubeGateFailures(r)).toEqual([
      "yt-dlp resolved no format: ERROR: [youtube] gq-4FVRr_ow: Sign in to confirm you're not a bot",
    ]);
  });

  test("a box with no cookie file fails even when this video resolves", () => {
    expect(youtubeGateFailures({ ...resolved(), cookies: false })).toEqual([
      "no cookie file on the box: YouTube asks every anonymous download to sign in (#141)",
    ]);
  });

  test("a pick under 720 rows fails: the selector lost its tiers", () => {
    const r = resolved();
    r.format = { format_id: "243", codec: "vp9", width: 640, height: 360, fps: 25 };
    expect(youtubeGateFailures(r)).toEqual([
      "the selector picked 243 at 360 rows, under 720 (the video has 1080p)",
    ]);
    expect(YOUTUBE_MIN_HEIGHT).toBe(720);
  });

  test("exactly 720 rows passes; a format with no height fails", () => {
    const at720 = resolved();
    at720.format = { format_id: "398", codec: "av01", width: 1280, height: 720, fps: 30 };
    expect(youtubeGateFailures(at720)).toEqual([]);
    const noHeight = resolved();
    noHeight.format = { format_id: "399", codec: null, width: null, height: null, fps: null };
    expect(youtubeGateFailures(noHeight)).toHaveLength(1);
  });

  test("ok with no format, or another video, fails", () => {
    expect(youtubeGateFailures({ ...resolved(), format: null })).toEqual([
      "yt-dlp resolved no format: no error given",
    ]);
    expect(youtubeGateFailures({ ...resolved(), youtube_id: "dQw4w9WgXcQ" })).toEqual([
      "probed dQw4w9WgXcQ, not gq-4FVRr_ow",
    ]);
  });
});
