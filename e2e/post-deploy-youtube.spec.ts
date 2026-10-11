/**
 * #232 post-deploy gate: the box's yt-dlp must resolve a real YouTube video
 * exactly as a download does — its cookie file, its deno, the production
 * selector at the live cap — and pick a format of at least 720 rows.
 *
 * Every song comes from YouTube this way. The only check before this gate
 * was the deno self-check (`tools.js_runtime_ok`), which passes on a bot
 * check, so expired cookies or a YouTube change yt-dlp cannot extract
 * failed every download with CI green; the owner's rule (29.9.2026): every
 * external boundary gets a live post-deploy check.
 *
 * `POST /api/v1/youtube/probe` downloads nothing (`downloader::probe`);
 * the decision is the pure `youtube-gate.ts`. API-level on purpose: the
 * probe has no dashboard surface.
 */

import { test, expect } from "@playwright/test";
import { YoutubeProbeReport, youtubeGateFailures } from "./youtube-gate";

test.describe("YouTube live gate (#232)", () => {
  test("the box's yt-dlp resolves a real video as a download would", async ({ request }) => {
    // yt-dlp answered in 2.6 s on SNV; the probe's own bound is 120 s.
    test.setTimeout(240_000);

    // The tools come up a few seconds after a restart; a read that throws
    // counts as "not yet" (`expect.poll` does not retry a throwing generator).
    await expect
      .poll(
        async () => {
          try {
            const s = await request.get("/api/v1/status", { timeout: 10_000 });
            return s.ok() && ((await s.json()) as { tools?: { ytdlp_available?: boolean } }).tools?.ytdlp_available === true;
          } catch {
            return false;
          }
        },
        { timeout: 120_000, intervals: [2_000] },
      )
      .toBe(true);

    const resp = await request.post("/api/v1/youtube/probe", { timeout: 180_000 });
    expect(resp.status(), "POST /api/v1/youtube/probe").toBe(200);
    const report = (await resp.json()) as YoutubeProbeReport;
    console.log(`[#232 youtube] ${JSON.stringify(report)}`);
    expect(youtubeGateFailures(report), "YouTube must resolve the fixed video with the box's cookies").toEqual([]);
  });
});
