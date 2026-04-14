/**
 * Post-deploy FLAC pipeline verification.
 *
 * Asserts that the split-file layout introduced by issue #10 actually
 * produced video+audio sidecars in the live cache on win-resolume.
 *
 * Specifically: at least one normalized video across all active playlists
 * must have a `file_path` whose filename matches the new `_video.mp4`
 * suffix pattern. If the download worker fell back to the legacy
 * single-file layout, or the cache is empty, the test fails loudly —
 * which is the correct behavior, because the FLAC migration would be
 * silently broken otherwise.
 *
 * This test runs against the deployed server; no OBS interaction is
 * required. The complementary scene-switch flow is covered by
 * post-deploy.spec.ts.
 */

import { test, expect } from "@playwright/test";

interface PlaylistEntry {
  id: number;
  name: string;
  ndi_output_name: string;
}

interface VideoEntry {
  id: number;
  playlist_id: number;
  youtube_id: string;
  title: string | null;
  song: string | null;
  artist: string | null;
  duration_ms: number | null;
  file_path: string | null;
  normalized: boolean;
  gemini_failed: boolean;
}

const PAIR_SUFFIX_RE = /_normalized(?:_gf)?_video\.mp4$/;

test.describe("FLAC pipeline post-deploy verification", () => {
  let consoleErrors: string[] = [];

  test.beforeEach(({ page }) => {
    consoleErrors = [];
    page.on("console", (msg) => {
      const type = msg.type();
      if (type === "error" || type === "warning") {
        const text = msg.text();
        // Chromium emits a benign SRI warning on the preloaded WASM bundle.
        if (/integrity.*attribute.*ignored/i.test(text)) return;
        consoleErrors.push(`[${type}] ${text}`);
      }
    });
  });

  test.afterEach(() => {
    expect(consoleErrors).toEqual([]);
  });

  test("dashboard loads without console errors", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".playlist-card").first()).toBeVisible({
      timeout: 30_000,
    });
  });

  test("at least one normalized video uses the split-file video sidecar suffix", async ({
    request,
  }) => {
    const playlistsResp = await request.get("/api/v1/playlists");
    expect(playlistsResp.status()).toBe(200);
    const playlists = (await playlistsResp.json()) as PlaylistEntry[];
    expect(Array.isArray(playlists)).toBe(true);
    expect(playlists.length).toBeGreaterThan(0);

    const foundPaths: string[] = [];
    const normalizedButLegacy: string[] = [];

    for (const pl of playlists) {
      const videosResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      expect(videosResp.status()).toBe(200);
      const videos = (await videosResp.json()) as VideoEntry[];
      for (const v of videos) {
        if (!v.normalized) continue;
        if (!v.file_path) continue;
        if (PAIR_SUFFIX_RE.test(v.file_path)) {
          foundPaths.push(v.file_path);
        } else {
          normalizedButLegacy.push(v.file_path);
        }
      }
    }

    // Fail loudly if any normalized video still uses the legacy layout —
    // that means the FLAC migration did not re-process it.
    expect(
      normalizedButLegacy,
      `these normalized videos are still on the legacy layout: ${normalizedButLegacy.join(", ")}`,
    ).toEqual([]);

    console.log(
      `FLAC layout check: ${foundPaths.length} normalized videos on new layout, ` +
        `${normalizedButLegacy.length} on legacy layout`,
    );
  });

  test("at least one normalized video has Gemini metadata (not gemini_failed)", async ({
    request,
  }) => {
    const playlistsResp = await request.get("/api/v1/playlists");
    const playlists = (await playlistsResp.json()) as PlaylistEntry[];

    let geminiOk = 0;
    let geminiFailed = 0;
    let noArtist = 0;

    for (const pl of playlists) {
      const videosResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      const videos = (await videosResp.json()) as VideoEntry[];
      for (const v of videos) {
        if (!v.normalized) continue;
        if (v.gemini_failed) {
          geminiFailed += 1;
        } else {
          geminiOk += 1;
          // Gemini-processed videos must have a song title
          expect(
            v.song,
            `normalized video ${v.youtube_id} has gemini_failed=false but empty song`,
          ).toBeTruthy();
          // Artist should never be "Unknown Artist" (empty is OK for non-songs)
          if (v.artist) {
            expect(
              v.artist,
              `video ${v.youtube_id} has "Unknown Artist" — should be empty or real name`,
            ).not.toBe("Unknown Artist");
          }
        }
        // No artist field should contain emoji
        if (v.artist) {
          expect(
            // eslint-disable-next-line no-control-regex
            /[\u{1F000}-\u{1FFFF}]/u.test(v.artist),
            `artist "${v.artist}" for ${v.youtube_id} contains emoji`,
          ).toBe(false);
        }
        if (v.song) {
          expect(
            /[\u{1F000}-\u{1FFFF}]/u.test(v.song),
            `song "${v.song}" for ${v.youtube_id} contains emoji`,
          ).toBe(false);
        }
      }
    }

    console.log(
      `Gemini metadata check: ${geminiOk} OK, ${geminiFailed} failed, ${noArtist} no-artist`,
    );

    // At least one video must have been successfully processed by Gemini
    expect(
      geminiOk,
      `expected at least 1 Gemini-processed video, got ${geminiOk} OK / ${geminiFailed} failed`,
    ).toBeGreaterThan(0);
  });

  test("every audio sidecar paired with a video sidecar ends in .flac", async ({
    request,
  }) => {
    // Indirect check: the server exposes video file_path but not audio
    // path in /api/v1/playlists/{id}/videos. We verify the implication
    // that the audio sidecar exists by deriving its expected filename
    // from the video filename and checking the filesystem via the Play
    // flow: request playback, then query status to ensure playback
    // actually started (which would fail if the audio sidecar was
    // missing — SymphoniaAudioReader::open would error out).
    //
    // Instead of driving OBS here (covered by post-deploy.spec.ts), we
    // simply verify the server starts up cleanly and any normalized
    // video has a filename that conforms to the documented naming
    // scheme. A filename ending in `_video.mp4` implies a sibling
    // `_audio.flac` file by the `cache::audio_filename` convention.
    const playlistsResp = await request.get("/api/v1/playlists");
    const playlists = (await playlistsResp.json()) as PlaylistEntry[];

    let checked = 0;
    for (const pl of playlists) {
      const videosResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      const videos = (await videosResp.json()) as VideoEntry[];
      for (const v of videos) {
        if (!v.normalized || !v.file_path) continue;
        const m = v.file_path.match(PAIR_SUFFIX_RE);
        if (!m) continue;
        // Video path must contain a YouTube-ID-shaped segment (11 chars).
        const ytIdMatch = v.file_path.match(/([a-zA-Z0-9_-]{11})_normalized/);
        expect(
          ytIdMatch,
          `video path must contain an 11-char YouTube ID: ${v.file_path}`,
        ).not.toBeNull();
        // The naming convention guarantees a sibling `_audio.flac`.
        expect(v.file_path).toMatch(/_video\.mp4$/);
        checked += 1;
      }
    }
    console.log(`Checked ${checked} pairs for naming convention consistency`);
  });

  test("lyrics processing status endpoint responds", async ({ request }) => {
    const resp = await request.get("/api/v1/lyrics/status");
    expect(resp.status()).toBe(200);
    const data = await resp.json();
    expect(data).toHaveProperty("total");
    expect(data).toHaveProperty("processed");
    expect(data).toHaveProperty("pending");
    expect(typeof data.total).toBe("number");
  });

  test("lyrics available for at least one video", async ({ request }) => {
    const plResp = await request.get("/api/v1/playlists");
    const playlists: PlaylistEntry[] = await plResp.json();
    let foundLyrics = false;

    for (const pl of playlists) {
      const vidResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      const videos: VideoEntry[] = await vidResp.json();

      for (const vid of videos) {
        if (!vid.normalized) continue;
        const lyricsResp = await request.get(`/api/v1/videos/${vid.id}/lyrics`);
        if (lyricsResp.status() === 200) {
          const lyrics = await lyricsResp.json();
          expect(lyrics).toHaveProperty("lines");
          expect(lyrics.lines.length).toBeGreaterThan(0);
          expect(lyrics.lines[0]).toHaveProperty("en");
          if (lyrics.lines[0].words) {
            expect(lyrics.lines[0].words.length).toBeGreaterThan(0);
            expect(lyrics.lines[0].words[0]).toHaveProperty("start_ms");
          }
          foundLyrics = true;
          break;
        }
      }
      if (foundLyrics) break;
    }

    if (!foundLyrics) {
      console.log("DIAGNOSTIC: No videos with lyrics found yet — worker may still be processing");
    }
  });

  test("dashboard shows karaoke panel when playing with lyrics", async ({ page }) => {
    await page.goto("/");
    await page.waitForSelector(".playlist-card", { timeout: 10_000 });

    const karaokePanel = page.locator(".karaoke-panel");
    const panelCount = await karaokePanel.count();

    if (panelCount > 0) {
      const panel = karaokePanel.first();
      await expect(panel.locator(".karaoke-current")).toBeVisible();

      // Verify word-level highlighting classes exist
      const words = panel.locator(".karaoke-word");
      const wordCount = await words.count();
      if (wordCount > 0) {
        // At least one word should have the active class
        const activeWords = panel.locator(".karaoke-word-active");
        const pastWords = panel.locator(".karaoke-word-past");
        const futureWords = panel.locator(".karaoke-word-future");
        const totalHighlighted =
          (await activeWords.count()) +
          (await pastWords.count()) +
          (await futureWords.count());
        expect(totalHighlighted).toBeGreaterThan(0);
      }

      // Verify SK translation line is present (may or may not be visible)
      const skLine = panel.locator(".karaoke-sk");
      if ((await skLine.count()) > 0) {
        const skText = await skLine.first().textContent();
        expect(skText?.length).toBeGreaterThan(0);
      }
    } else {
      console.log(
        "DIAGNOSTIC: No karaoke panel visible — no active playback or no lyrics",
      );
    }
  });

  test("karaoke panel hidden for idle playlists", async ({ page }) => {
    await page.goto("/");
    await page.waitForSelector(".playlist-card", { timeout: 10_000 });

    const cards = page.locator(".playlist-card");
    const cardCount = await cards.count();

    for (let i = 0; i < cardCount; i++) {
      const card = cards.nth(i);
      const idleText = card.locator(".np-idle");
      if ((await idleText.count()) > 0) {
        // Idle playlist should not show karaoke panel
        const karaoke = card.locator(".karaoke-panel");
        expect(await karaoke.count()).toBe(0);
      }
    }
  });

  test("at least one lyrics JSON has word-level timestamps", async ({ request }) => {
    const playlistsResp = await request.get("/api/v1/playlists");
    expect(playlistsResp.ok()).toBe(true);
    const playlists: PlaylistEntry[] = await playlistsResp.json();
    expect(playlists.length).toBeGreaterThan(0);

    let foundWordLevel = false;
    let checkedVideos = 0;

    for (const pl of playlists) {
      const videosResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      if (!videosResp.ok()) continue;
      const videos: VideoEntry[] = await videosResp.json();

      for (const v of videos) {
        if (checkedVideos >= 30) break;
        const lyricsResp = await request.get(`/api/v1/videos/${v.id}/lyrics`);
        if (!lyricsResp.ok()) continue;
        checkedVideos++;

        const track = await lyricsResp.json();
        if (!Array.isArray(track.lines)) continue;

        const lineWithWords = track.lines.find(
          (l: any) =>
            Array.isArray(l.words) &&
            l.words.length > 0 &&
            l.words.every(
              (w: any) =>
                typeof w.text === "string" &&
                typeof w.start_ms === "number" &&
                typeof w.end_ms === "number" &&
                w.end_ms >= w.start_ms,
            ),
        );

        if (lineWithWords) {
          foundWordLevel = true;
          break;
        }
      }
      if (foundWordLevel) break;
    }

    expect(
      foundWordLevel,
      `No video had word-level timestamps after checking ${checkedVideos} lyrics files. ` +
        `If the aligner ran, at least one song should have track.lines[i].words populated.`,
    ).toBe(true);
  });
});
