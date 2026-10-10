/**
 * #232 post-deploy gate: Genius must answer the lyrics worker's own fetch
 * with the box's token.
 *
 * Genius is one of the lyrics worker's text sources (the community-lyrics
 * tier and the title search). Its token had only a check that the setting
 * is masked, so a dead token or an unreachable API stayed invisible with CI
 * green; the owner's rule (29.9.2026): every external provider gets a live
 * post-deploy check.
 *
 * `POST /api/v1/lyrics/probe-sources` runs the worker's fetches for one
 * song, Genius among them (`lyrics::probe`). The gate asks up to six
 * catalog songs whose served lyrics came from Genius and passes on the
 * first one Genius answers with lyrics (`genius-gate.ts`). API-level on
 * purpose: the probe has no dashboard surface.
 */

import { test, expect } from "@playwright/test";
import { LyricsSongRow, ProbeReport, geniusGateFailures, geniusHit, geniusRows } from "./genius-gate";

test.describe("Genius live gate (#232)", () => {
  test("Genius answers the worker's fetch with the box's token", async ({ request }) => {
    // Each probe also runs yt-dlp for captions and the other text sources
    // (~4 s a song live); six songs at most.
    test.setTimeout(600_000);

    const songsResp = await request.get("/api/v1/lyrics/songs", { timeout: 30_000 });
    expect(songsResp.status(), "GET /api/v1/lyrics/songs").toBe(200);
    const asked = geniusRows((await songsResp.json()) as LyricsSongRow[]);
    console.log(`[#232 genius] asking ${asked.map((s) => `${s.video_id} ${s.youtube_id}`).join(", ")}`);

    const reports: ProbeReport[] = [];
    for (const song of asked) {
      const resp = await request.post("/api/v1/lyrics/probe-sources", {
        data: { video_id: song.video_id },
        timeout: 90_000,
      });
      expect(resp.status(), `POST /api/v1/lyrics/probe-sources ${song.video_id}`).toBe(200);
      const report = (await resp.json()) as ProbeReport;
      const genius = report.probes.find((p) => p.provider === "genius");
      console.log(`[#232 genius] ${report.youtube_id} "${report.song}": ${JSON.stringify(genius)}`);
      reports.push(report);
      if (geniusHit(report)) break;
    }

    expect(geniusGateFailures(asked, reports), "Genius must answer one Genius song with lyrics").toEqual([]);
  });
});
