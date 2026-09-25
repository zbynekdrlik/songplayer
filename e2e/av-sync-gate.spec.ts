/**
 * Unit tests for the pure A/V gate helpers (#147) and the shared baseline-scene
 * picker. Runs in the ubuntu mock suite (playwright.config.ts). These `test()`
 * blocks never touch `page`, so they need no browser and no box.
 */

import { test, expect } from "@playwright/test";
import {
  classifyAvSyncRun,
  evidenceName,
  isPlayingWithFrames,
  keepsEvidence,
  nowPlayingVideoId,
  recordingFiles,
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

  test("classifyAvSyncRun: pass / fail / cannot-measure from JSON + exit code", () => {
    const pass = classifyAvSyncRun(0, JSON.stringify({ status: "pass", reasons: [], av_ms: 13 }));
    expect(pass).toEqual({ status: "pass", detail: "pass (A/V 13 ms)", retakeable: false });

    const fail = classifyAvSyncRun(
      1,
      JSON.stringify({ status: "fail", reasons: ["|A/V| 120.0 ms > 40 ms"], unmeasurable_sides: [] }),
    );
    expect(fail.status).toBe("fail");
    expect(fail.detail).toContain("120.0 ms");
    expect(fail.retakeable).toBe(false); // a real FAIL is never retaken

    // Dropouts with an unmeasurable picture are a FAIL, never a retake (#147 review).
    const dropouts = classifyAvSyncRun(
      1,
      JSON.stringify({
        status: "fail",
        reasons: ["2 audio dropout(s)", "video match 0.5 < 0.95"],
        unmeasurable_sides: [],
      }),
    );
    expect(dropouts.status).toBe("fail");
    expect(dropouts.retakeable).toBe(false);
  });

  test("classifyAvSyncRun: only a picture-side cannot-measure is retakeable", () => {
    const picture = classifyAvSyncRun(
      2,
      JSON.stringify({
        status: "cannot_measure",
        reasons: ["video contrast 0.0001 < 0.002 (no motion to align on)"],
        unmeasurable_sides: ["video"],
      }),
    );
    expect(picture.status).toBe("cannot_measure");
    expect(picture.detail).toMatch(/never a skip/);
    expect(picture.retakeable).toBe(true);

    const audio = classifyAvSyncRun(
      2,
      JSON.stringify({
        status: "cannot_measure",
        reasons: ["audio correlation 0.4 < 0.9", "video match 0.5 < 0.95"],
        unmeasurable_sides: ["audio", "video"],
      }),
    );
    expect(audio.retakeable).toBe(false); // may be a real audio fault
    const crashed = classifyAvSyncRun(
      2,
      JSON.stringify({ status: "cannot_measure", reasons: ["analysis error: RuntimeError: ffmpeg"] }),
    );
    expect(crashed.retakeable).toBe(false);

    // A picture-step EXCEPTION (probe/decode/crop bug) is deterministic: never retaken.
    const videoError = classifyAvSyncRun(
      2,
      JSON.stringify({
        status: "cannot_measure",
        reasons: ["video analysis error: RuntimeError: decoded 0 frames"],
        unmeasurable_sides: ["video_error"],
      }),
    );
    expect(videoError.status).toBe("cannot_measure");
    expect(videoError.retakeable).toBe(false);
  });

  test("classifyAvSyncRun: no JSON or a contradicting exit code is an error, not a verdict", () => {
    // e.g. numpy failed to import (exit 1) or argparse rejected the args (exit 2)
    expect(classifyAvSyncRun(1, "Traceback (most recent call last): ...").status).toBe("error");
    expect(classifyAvSyncRun(2, "usage: av_sync_check.py ...").status).toBe("error");
    expect(classifyAvSyncRun(null, "").status).toBe("error");
    expect(classifyAvSyncRun(1, JSON.stringify({ status: "pass" })).status).toBe("error");
    expect(classifyAvSyncRun(0, JSON.stringify({ status: "weird" })).status).toBe("error");
    expect(classifyAvSyncRun(0, JSON.stringify({ status: "toString" })).status).toBe("error");
  });

  test("recordingFiles adds the auto-remux mp4 sibling only when it applies", () => {
    const mkv = "C:\\Users\\op\\Videos\\2026-09-24 20-00-00.mkv";
    expect(recordingFiles(mkv, false)).toEqual([mkv]);
    expect(recordingFiles(mkv, true)).toEqual([mkv, "C:\\Users\\op\\Videos\\2026-09-24 20-00-00.mp4"]);
    const mp4 = "C:\\Videos\\rec.mp4";
    expect(recordingFiles(mp4, true)).toEqual([mp4]);
  });

  test("keepsEvidence: every analysed take that did not pass keeps its recording", () => {
    const run = (status: "pass" | "fail" | "cannot_measure" | "error") => ({
      status,
      detail: status,
      retakeable: false,
    });
    expect(keepsEvidence(true, run("fail"))).toBe(true);
    expect(keepsEvidence(true, run("cannot_measure"))).toBe(true);
    expect(keepsEvidence(true, run("error"))).toBe(true);
    // The analysis threw (timeout, spawn error): no verdict, so keep it.
    expect(keepsEvidence(true, null)).toBe(true);
    // Only a pass deletes the recording without a copy.
    expect(keepsEvidence(true, run("pass"))).toBe(false);
    // A take discarded before analysis (the song changed) is not evidence.
    expect(keepsEvidence(false, null)).toBe(false);
  });

  test("evidenceName prefixes the take and keeps only the file name", () => {
    expect(evidenceName(2, "C:\\Users\\op\\Videos\\2026-09-24 20-00-00.mkv")).toBe(
      "take2-2026-09-24 20-00-00.mkv",
    );
    expect(evidenceName(1, "/home/op/rec.mp4")).toBe("take1-rec.mp4");
    expect(evidenceName(3, "av_sync.json")).toBe("take3-av_sync.json");
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
