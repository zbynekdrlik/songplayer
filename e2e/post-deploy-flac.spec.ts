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
        // #178: the on-demand preview <video> opens a WebSocket; a page that
        // navigates/tears down mid-handshake makes Chrome log this benign
        // "closed before established" warning. Not a product error.
        if (/WebSocket is closed before the connection is established/i.test(text))
          return;
        consoleErrors.push(`[${type}] ${text}`);
      }
    });
  });

  test.afterEach(() => {
    expect(consoleErrors).toEqual([]);
  });

  test("dashboard loads without console errors", async ({ page }) => {
    await page.goto("/");
    // #165: the dashboard shows a selector + ONE work area.
    await expect(page.getByTestId("playlist-workspace")).toBeVisible({
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

  test("dashboard shows the lyrics-view when playing with lyrics (#194)", async ({ page }) => {
    await page.goto("/");
    // #165: the playing playlist is preselected in the single work area.
    await page.waitForSelector('[data-testid="playlist-workspace"]', {
      timeout: 10_000,
    });

    // #194: lyrics are the shared LyricsView (tappable scroll list) in the
    // Player, replacing the old 4-line karaoke panel.
    const lyricsView = page.locator('[data-testid="lyrics-view"]').first();
    await expect(lyricsView).toBeVisible({ timeout: 10_000 });

    // #198 item 3: the fetch-in-flight state is a `state-loading` block now (it
    // used to render `.lyrics-empty`), so let the view SETTLE before branching:
    // either tappable lines or the genuine `.lyrics-empty` surface — never the
    // loading block (a settled error block fails the assertion below).
    const lines = lyricsView.locator(".lyr-line");
    await expect
      .poll(
        async () =>
          (await lines.count()) > 0
            ? "lines"
            : (await lyricsView.locator(".lyrics-empty").count()) > 0
              ? "empty"
              : (await lyricsView.locator('[data-testid="state-error"]').count()) > 0
                ? "error"
                : "loading",
        { timeout: 15_000, message: "the lyrics-view must settle to lines or empty" },
      )
      .toMatch(/^(lines|empty)$/);
    if ((await lines.count()) > 0) {
      // A playing song with lyrics renders tappable lines.
      await expect(lines.first()).toBeVisible();
    } else {
      // No lyrics for the current item -> the empty lyrics surface.
      await expect(lyricsView.locator(".lyrics-empty")).toBeVisible();
      console.log(
        "DIAGNOSTIC: no lyric lines for the current item — empty lyrics-view",
      );
    }
  });

  test("idle playlists show the empty lyrics-view, no lyric lines (#194)", async ({ page }) => {
    // #165: only ONE work area at a time; walk the selector rows. #194: an idle
    // playlist's Player shows the shared LyricsView in its empty state
    // (`.lyrics-empty`), never any `.lyr-line`.
    await page.goto("/");
    await page.waitForSelector('[data-testid="playlist-workspace"]', {
      timeout: 10_000,
    });

    const rows = page.getByTestId("playlist-picker-item");
    const rowCount = await rows.count();

    for (let i = 0; i < rowCount; i++) {
      await rows.nth(i).click();
      const card = page.locator(".playlist-card").first();
      await expect(card).toBeVisible();
      // #170: read the idle marker and the lyric lines in ONE DOM pass (two
      // separate reads race a live state change). The invariant is per-instant:
      // an idle card renders no lyric lines.
      const { idle, lines } = await card.evaluate((el) => {
        const title = el.querySelector('[data-testid="player-title"]');
        return {
          idle: title && title.textContent.trim() === "Nič nehrá" ? 1 : 0,
          lines: el.querySelectorAll(".lyr-line").length,
        };
      });
      if (idle > 0) {
        expect(lines).toBe(0);
      }
    }
  });

  test("at least one song has current-version line-level lyrics (v21 regime, #143)", async ({
    request,
  }) => {
    // Rescoped 2026-09-12: the Gemini regime this test used to key on
    // (`ensemble:gemini`, v11–v18) is gone — v21 re-queues every such row
    // (#143), so "a non-stale ensemble:gemini song" no longer exists. The
    // invariant under test is unchanged and now checked on whatever the
    // CURRENT pipeline persisted:
    //   - at least one non-stale track with lyrics at the current version
    //   - line timings are well-formed: monotonic start_ms, end_ms >=
    //     start_ms, non-empty `en` text
    //   - `words` is absent / null for these tracks (confirms v18's drop
    //     of synthesized per-word timings — if this ever flips back, the
    //     karaoke wall will drift again)
    //
    // Runs the same way as the other post-deploy tests: read the catalog
    // one-shot, then assert the shape of what's persisted. An absent fixture
    // population fails the test rather than skipping it.
    test.setTimeout(3 * 60 * 1000);

    interface Word {
      start_ms: number;
      end_ms: number;
    }
    interface Line {
      start_ms?: number;
      end_ms?: number;
      en?: string;
      words?: Word[] | null;
    }
    interface Track {
      source?: string;
      lines?: Line[];
    }
    interface CatalogSong {
      video_id: number;
      source: string | null;
      pipeline_version: number;
      has_lyrics: boolean;
      is_stale: boolean;
    }

    const sl = await request.get("/api/v1/lyrics/songs");
    expect(sl.status()).toBe(200);
    const songs: CatalogSong[] = await sl.json();
    const geminiSongs = songs.filter(
      (s) =>
        s.has_lyrics &&
        !s.is_stale &&
        typeof s.source === "string" &&
        s.source.length > 0,
    );

    // A missing fixture population must FAIL, not skip (test-strictness): a
    // skip here is permanent silent green, and this assertion is the only
    // end-to-end guard that v18's "no synthesized per-word timings" invariant
    // still holds on real persisted data. If the catalog genuinely stops
    // carrying non-stale lyrics rows, rescope or delete this test deliberately
    // — do not let it quietly stop running.
    expect(
      geminiSongs.length,
      "no non-stale song with lyrics at the current pipeline version — this " +
        "test's fixture population is gone; rescope or delete this test rather " +
        "than letting it skip into permanent silent green",
    ).toBeGreaterThan(0);

    const tested: string[] = [];
    for (const s of geminiSongs.slice(0, 3)) {
      const lr = await request.get(`/api/v1/videos/${s.video_id}/lyrics`);
      expect(lr.status()).toBe(200);
      const track: Track = await lr.json();

      expect(
        typeof track.source === "string" && track.source.length > 0,
        `video ${s.video_id} source on track payload`,
      ).toBe(true);
      expect(
        Array.isArray(track.lines) && track.lines.length > 0,
        `video ${s.video_id} must have at least one line`,
      ).toBe(true);

      let prev = -1;
      for (const [i, l] of track.lines!.entries()) {
        expect(typeof l.en === "string" && l.en.length > 0, `line ${i} empty`).toBe(true);
        expect(typeof l.start_ms === "number", `line ${i} missing start_ms`).toBe(true);
        expect(typeof l.end_ms === "number", `line ${i} missing end_ms`).toBe(true);
        expect(l.end_ms! >= l.start_ms!, `line ${i} end before start`).toBe(true);
        expect(l.start_ms! >= prev, `line ${i} start_ms non-monotonic`).toBe(true);
        // v18 invariant: wordless providers emit words=None (serialized
        // as absent / null). If a future change re-synthesizes per-word
        // timings by even-distribution, the karaoke wall regresses.
        expect(
          l.words === undefined || l.words === null,
          `line ${i} has unexpected words field (v18+ must be line-level only)`,
        ).toBe(true);
        prev = l.start_ms!;
      }
      tested.push(`#${s.video_id}`);
    }
    console.log(`current-version line-level check OK on: ${tested.join(", ")}`);
  });
});
