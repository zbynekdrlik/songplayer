/**
 * Unit tests for the pure A/V gate helpers (#147) and the shared baseline-scene
 * picker. Runs in the ubuntu mock suite (playwright.config.ts). These `test()`
 * blocks never touch `page`, so they need no browser and no box.
 */

import { test, expect } from "@playwright/test";
import {
  describeAvSyncExit,
  isPlayingWithFrames,
  nowPlayingVideoId,
  resolveSidecars,
} from "./av-sync-gate";
import { pickBaselineScene } from "./obs-baseline-scene";

test.describe("A/V sync gate helpers (#147)", () => {
  test("isPlayingWithFrames needs Playing AND frames in the last 5 s", () => {
    const health = [
      { playlist_id: 3, state: "Playing", frames_submitted_last_5s: 150 },
      { playlist_id: 4, state: "Playing", frames_submitted_last_5s: 0 },
      { playlist_id: 5, state: "Paused", frames_submitted_last_5s: 150 },
    ];
    expect(isPlayingWithFrames(health, 3)).toBe(true);
    expect(isPlayingWithFrames(health, 4)).toBe(false);
    expect(isPlayingWithFrames(health, 5)).toBe(false);
    expect(isPlayingWithFrames(health, 9)).toBe(false);
  });

  test("nowPlayingVideoId reads the playlist's entry from /api/v1/mix", () => {
    const mix = {
      now_playing: [
        { playlist_id: 3, video_id: 335 },
        { playlist_id: 7, video_id: 12 },
      ],
    };
    expect(nowPlayingVideoId(mix, 3)).toBe(335);
    expect(nowPlayingVideoId(mix, 7)).toBe(12);
    expect(nowPlayingVideoId(mix, 8)).toBeNull();
    expect(nowPlayingVideoId({}, 3)).toBeNull();
  });

  test("resolveSidecars finds the one complete pair (plain and _gf)", () => {
    const files = [
      "Oceans_Hillsong_abc-DEF_12_normalized_video.mp4",
      "Oceans_Hillsong_abc-DEF_12_normalized_audio.flac",
      "Other_Song_zzz_normalized_gf_video.mp4",
      "Other_Song_zzz_normalized_gf_audio.flac",
      "tools",
    ];
    expect(resolveSidecars(files, "abc-DEF_12")).toEqual({
      video: "Oceans_Hillsong_abc-DEF_12_normalized_video.mp4",
      audio: "Oceans_Hillsong_abc-DEF_12_normalized_audio.flac",
    });
    expect(resolveSidecars(files, "zzz")).toEqual({
      video: "Other_Song_zzz_normalized_gf_video.mp4",
      audio: "Other_Song_zzz_normalized_gf_audio.flac",
    });
  });

  test("resolveSidecars never matches a different id that merely ends the same", () => {
    const files = ["A_B_xabc_normalized_video.mp4", "A_B_xabc_normalized_audio.flac"];
    expect(() => resolveSidecars(files, "abc")).toThrow(/found 0/);
  });

  test("resolveSidecars throws on a missing half or an ambiguous pair", () => {
    expect(() => resolveSidecars(["S_A_id1_normalized_video.mp4"], "id1")).toThrow(
      /exactly one.*found 0/,
    );
    const two = [
      "S_A_id1_normalized_video.mp4",
      "S_A_id1_normalized_audio.flac",
      "S_A_id1_normalized_gf_video.mp4",
      "S_A_id1_normalized_gf_audio.flac",
    ];
    expect(() => resolveSidecars(two, "id1")).toThrow(/found 2/);
  });

  test("describeAvSyncExit maps every exit code; cannot-measure is a failure", () => {
    expect(describeAvSyncExit(0)).toBe("pass");
    expect(describeAvSyncExit(1)).toMatch(/FAIL/);
    expect(describeAvSyncExit(2)).toMatch(/CANNOT MEASURE.*never a skip/);
    expect(describeAvSyncExit(null)).toMatch(/failed to run/);
  });
});

test.describe("baseline scene picker (CLAUDE.md OBS discipline)", () => {
  test("prefers sp-slow", () => {
    expect(pickBaselineScene(["sp-fast", "sp-warmup", "QR test", "sp-slow"])).toBe("sp-slow");
  });

  test("never picks sp-fast or sp-warmup while another sp-* exists", () => {
    expect(pickBaselineScene(["sp-fast", "sp-warmup", "sp-90s", "QR test"])).toBe("sp-90s");
  });

  test("falls back to a non-sp scene only when no other sp-* exists", () => {
    expect(pickBaselineScene(["sp-fast", "sp-warmup", "QR test"])).toBe("QR test");
  });
});
