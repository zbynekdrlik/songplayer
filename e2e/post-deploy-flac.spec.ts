/**
 * Post-deploy FLAC pipeline verification.
 *
 * Asserts that the split-file layout introduced by issue #10 actually
 * produced video+audio sidecars in the live cache on win-resolume.
 *
 * Specifically: every normalized video across all playlists must have
 * exactly one `…_{youtube_id}_normalized[_gf]_video.mp4` + `_audio.flac` pair
 * in the cache directory (read from disk — the videos API exposes no file
 * paths), and no legacy single-file `_normalized.mp4` may remain. If the
 * download worker fell back to the legacy layout, or a half-pair is left, the
 * test fails loudly.
 *
 * This test runs against the deployed server; no OBS interaction is
 * required. The complementary scene-switch flow is covered by
 * post-deploy.spec.ts.
 */

import * as fs from "node:fs";
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
  normalized: boolean;
  gemini_failed: boolean;
}

/** A pre-FLAC single-file cache entry: `{…}_{id}_normalized[_gf].mp4` (no
 * `_video`/`_audio` suffix). The split-file migration must have removed all of
 * them (`startup::self_heal_cache`). */
const LEGACY_SINGLE_RE = /_normalized(?:_gf)?\.mp4$/;

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

  test("every normalized video has exactly one video+audio sidecar pair in the cache", async ({
    request,
  }) => {
    // The videos API does not expose file paths, so this reads the cache
    // directory itself (the post-deploy suite runs ON the box). It used to
    // test `v.file_path`, which the API never returns — the check skipped
    // every video and always passed (#147, found by the A/V-gate lane).
    const settings = await request.get("/api/v1/settings");
    expect(settings.status()).toBe(200);
    const cacheDir = ((await settings.json()) as { cache_dir?: string }).cache_dir;
    expect(cacheDir, "/api/v1/settings must report cache_dir").toBeTruthy();
    const names = fs.readdirSync(cacheDir!);

    const playlistsResp = await request.get("/api/v1/playlists");
    expect(playlistsResp.status()).toBe(200);
    const playlists = (await playlistsResp.json()) as PlaylistEntry[];
    expect(playlists.length).toBeGreaterThan(0);

    // At least one complete pair per normalized youtube id. The `_gf` part of
    // a name is NOT a reliable key: a Gemini retry flips `gemini_failed`
    // without renaming the files, and the same video can sit in two
    // playlists as rows with different Gemini outcomes (then it has both a
    // plain and a `_gf` pair). Playback reads the real path from the DB row.
    const ids = new Set<string>();
    for (const pl of playlists) {
      const videosResp = await request.get(`/api/v1/playlists/${pl.id}/videos`);
      expect(videosResp.status()).toBe(200);
      for (const v of (await videosResp.json()) as VideoEntry[]) {
        if (v.normalized) ids.add(v.youtube_id);
      }
    }
    expect(ids.size, "at least one normalized video must exist").toBeGreaterThan(0);

    const nameSet = new Set(names);
    const videoOf = (audio: string) => audio.replace(/_audio\.flac$/, "_video.mp4");
    const audioOf = (video: string) => video.replace(/_video\.mp4$/, "_audio.flac");
    const missing = [...ids].filter(
      (id) =>
        !names.some(
          (n) =>
            (n.endsWith(`_${id}_normalized_video.mp4`) ||
              n.endsWith(`_${id}_normalized_gf_video.mp4`)) &&
            nameSet.has(audioOf(n)),
        ),
    );
    expect(missing, `normalized videos with no complete video+audio pair: ${missing.join(", ")}`).toEqual([]);

    // No half-pairs: every sidecar has its sibling (startup::self_heal_cache
    // deletes orphans, so one here means the heal or a download broke).
    const half = names.filter(
      (n) =>
        (/_normalized(?:_gf)?_video\.mp4$/.test(n) && !nameSet.has(audioOf(n))) ||
        (/_normalized(?:_gf)?_audio\.flac$/.test(n) && !nameSet.has(videoOf(n))),
    );
    expect(half, `half sidecar pairs in the cache: ${half.join(", ")}`).toEqual([]);

    const legacy = names.filter((n) => LEGACY_SINGLE_RE.test(n));
    expect(legacy, `legacy single-file cache entries still present: ${legacy.join(", ")}`).toEqual([]);

    console.log(`FLAC layout check: ${ids.size} normalized videos, each with a complete video+audio pair; no half pairs`);
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
    // ONE retrying assertion over BOTH settled surfaces: the live box can
    // transition mid-check (a song ends → the next one loads → lines/empty
    // flip), so a branch decided from a stale `count()` and then asserted on the
    // other surface races (`element(s) not found`, run 35606544776). `or()`
    // keeps the assertion atomic with the state it observes.
    await expect(
      lines.first().or(lyricsView.locator(".lyrics-empty").first()),
    ).toBeVisible();
    if ((await lines.count()) === 0) {
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
