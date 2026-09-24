/**
 * Unit tests for the A/V gate's evidence files (#147), on a temp dir. Runs in
 * the ubuntu mock suite (playwright.config.ts). No browser, no box.
 */

import { test, expect } from "@playwright/test";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { keepRecording, keepText, settled } from "./av-sync-evidence";

function tmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "av-evidence-"));
}

test.describe("A/V gate evidence files (#147)", () => {
  test("keepRecording copies the recording and its sibling as take<N>-<name>", async () => {
    const src = tmpDir();
    const mkv = path.join(src, "2026-09-25 00-54-01.mkv");
    const mp4 = path.join(src, "2026-09-25 00-54-01.mp4");
    fs.writeFileSync(mkv, "mkv-bytes");
    fs.writeFileSync(mp4, "mp4-bytes");
    const dir = path.join(tmpDir(), "av-sync-evidence");
    await keepRecording([mkv, mp4], { dir, take: 2 }, Date.now() + 5_000);
    expect(fs.readdirSync(dir).sort()).toEqual([
      "take2-2026-09-25 00-54-01.mkv",
      "take2-2026-09-25 00-54-01.mp4",
    ]);
    expect(fs.readFileSync(path.join(dir, "take2-2026-09-25 00-54-01.mkv"), "utf8")).toBe(
      "mkv-bytes",
    );
    expect(fs.readFileSync(path.join(dir, "take2-2026-09-25 00-54-01.mp4"), "utf8")).toBe(
      "mp4-bytes",
    );
    // Copies, not moves: deleting the OBS files stays removeRecording's job.
    expect(fs.existsSync(mkv) && fs.existsSync(mp4)).toBe(true);
  });

  test("a remux sibling still being written is copied only once it is complete", async () => {
    const src = tmpDir();
    const mkv = path.join(src, "rec.mkv");
    const mp4 = path.join(src, "rec.mp4");
    fs.writeFileSync(mkv, "m");
    fs.writeFileSync(mp4, "a");
    // The "remux" appends for ~1.6 s, faster than the settle interval.
    let n = 1;
    const writer = setInterval(() => {
      if (n < 8) {
        fs.appendFileSync(mp4, "a");
        n++;
      }
    }, 200);
    try {
      const dir = path.join(tmpDir(), "ev");
      await keepRecording([mkv, mp4], { dir, take: 1 }, Date.now() + 8_000);
      expect(fs.readFileSync(path.join(dir, "take1-rec.mp4"), "utf8")).toBe("a".repeat(8));
    } finally {
      clearInterval(writer);
    }
  });

  test("a sibling that never appears is skipped, the recording is still kept", async () => {
    const src = tmpDir();
    const mkv = path.join(src, "rec.mkv");
    fs.writeFileSync(mkv, "m");
    const dir = path.join(tmpDir(), "ev");
    await keepRecording([mkv, path.join(src, "rec.mp4")], { dir, take: 3 }, Date.now() + 1_500);
    expect(fs.readdirSync(dir)).toEqual(["take3-rec.mkv"]);
  });

  test("settled needs a non-zero size that holds still", async () => {
    const src = tmpDir();
    const empty = path.join(src, "empty.mp4");
    fs.writeFileSync(empty, "");
    expect(await settled(empty, Date.now() + 2_500)).toBe(false);
    expect(await settled(path.join(src, "missing.mp4"), Date.now() + 1_500)).toBe(false);
    const full = path.join(src, "full.mp4");
    fs.writeFileSync(full, "x");
    expect(await settled(full, Date.now() + 2_500)).toBe(true);
  });

  test("keepText writes take<N>-<name> and never throws on a bad dir", () => {
    const dir = path.join(tmpDir(), "ev");
    keepText({ dir, take: 1 }, "av_sync.json", '{"status":"fail"}');
    expect(fs.readFileSync(path.join(dir, "take1-av_sync.json"), "utf8")).toBe('{"status":"fail"}');
    // A FILE where the dir should be: logged, not thrown (it runs in a finally).
    const blocker = path.join(tmpDir(), "not-a-dir");
    fs.writeFileSync(blocker, "x");
    expect(() => keepText({ dir: blocker, take: 1 }, "a.json", "{}")).not.toThrow();
  });
});
