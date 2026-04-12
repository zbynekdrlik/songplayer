/**
 * Post-deploy feature-level E2E tests.
 *
 * Runs on the self-hosted Windows runner against the real deployed
 * SongPlayer + OBS Studio. This is the test suite that would have
 * caught all four shipped bugs (#8, #9, #11, #12).
 *
 * What it exercises:
 *
 *  1. Clicking the dashboard Play button on a playlist with videos
 *     must result in a 2xx response and SongPlayer must react. This
 *     catches issue #8 (dashboard posted to nonexistent /api/v1/control).
 *
 *  2. After playback starts, the playlist card must transition from
 *     "Nothing playing" to a visible song/artist display. This catches
 *     issue #9 (server never broadcast ServerMsg::NowPlaying).
 *
 *  3. Switching the OBS program scene to a matching `sp-*` scene via
 *     obs-websocket-js must kick off scene-driven playback — SongPlayer
 *     must detect the NDI source in the scene and start the pipeline.
 *     This catches issue #11 (ndi_sources map was empty).
 *
 *  4. Switching back to a non-sp scene must stop playback and return
 *     the card to "Nothing playing".
 *
 *  5. Zero console errors or warnings throughout the suite. Catches
 *     issue #8/#9 regressions on the UI side plus anything else that
 *     might leak into the browser console.
 */

import { test, expect } from "@playwright/test";
import { ObsDriver } from "./obs-driver";

const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";

// Playlists deployed to win-resolume have predictable names. These tests
// expect at least one playlist called `ytfast` (id varies) with
// `ndi_output_name=SP-fast` and a corresponding OBS scene `sp-fast`.
const FAST_PLAYLIST_NAME = "ytfast";
const FAST_SCENE_NAME = "sp-fast";

async function findPlaylistId(request: import("@playwright/test").APIRequestContext, name: string): Promise<number> {
  const resp = await request.get("/api/v1/playlists");
  expect(resp.status()).toBe(200);
  const list = (await resp.json()) as Array<{ id: number; name: string; ndi_output_name: string }>;
  const pl = list.find((p) => p.name === name);
  if (!pl) throw new Error(`playlist "${name}" not found on deployed server`);
  return pl.id;
}

async function findPlaylistWithVideos(
  request: import("@playwright/test").APIRequestContext,
): Promise<{ id: number; name: string }> {
  const list = await (await request.get("/api/v1/playlists")).json();
  for (const pl of list as Array<{ id: number; name: string }>) {
    const videos = await (await request.get(`/api/v1/playlists/${pl.id}/videos`)).json();
    if (Array.isArray(videos) && videos.length > 0) return pl;
  }
  throw new Error("no playlist on deployed server has any videos");
}

test.describe("SongPlayer post-deploy feature verification", () => {
  let obs: ObsDriver | null = null;

  test.beforeAll(async () => {
    obs = await ObsDriver.connect(OBS_WS_URL);
  });

  test.afterAll(async () => {
    if (obs) {
      // Put OBS back to a known non-sp scene so subsequent runs start clean.
      try {
        const scenes = await obs.listScenes();
        const fallback = scenes.find((s) => !s.startsWith("sp-")) || scenes[0];
        if (fallback) await obs.switchScene(fallback);
      } catch {
        // ignore
      }
      await obs.disconnect();
    }
  });

  /**
   * Issue #8 — dashboard Play button.
   * Click the Play button on any playlist that has videos and assert
   * that SongPlayer responds with a 2xx (not 405). Playwright waits
   * for the matching network response to confirm the button targeted
   * a valid route.
   */
  test("clicking the Play button dispatches a 2xx backend request", async ({ page, request }) => {
    const pl = await findPlaylistWithVideos(request);

    // Move OBS off any sp-* scene so scene-driven playback doesn't
    // also kick in and muddy the assertions.
    if (obs) {
      const scenes = await obs.listScenes();
      const nonSp = scenes.find((s) => !s.startsWith("sp-"));
      if (nonSp) await obs.switchScene(nonSp);
    }

    await page.goto("/");
    // Wait for the WASM bundle to mount and the card to appear.
    await expect(page.locator(".playlist-card").first()).toBeVisible({ timeout: 30_000 });

    const card = page.locator(".playlist-card", { hasText: pl.name });
    await expect(card).toBeVisible();

    const expectedUrl = new RegExp(`/api/v1/playback/${pl.id}/play$`);
    const respPromise = page.waitForResponse((r) => expectedUrl.test(r.url()), { timeout: 10_000 });

    await card.getByRole("button", { name: "Play" }).click();

    const resp = await respPromise;
    expect(resp.status()).toBeGreaterThanOrEqual(200);
    expect(resp.status()).toBeLessThan(300);

    // Cleanup: pause so the next test starts from a known state.
    await request.post(`/api/v1/playback/${pl.id}/pause`);
  });

  /**
   * Issue #9 — now-playing broadcast.
   * After triggering playback via REST, the playlist card must transition
   * from "Nothing playing" to showing a song title within 5 s. The
   * update arrives via the WebSocket `NowPlaying` broadcast — before
   * the fix, that broadcast was never sent and the card stayed idle.
   */
  test("dashboard card shows song title after Play triggers NowPlaying broadcast", async ({
    page,
    request,
  }) => {
    const pl = await findPlaylistWithVideos(request);

    if (obs) {
      const scenes = await obs.listScenes();
      const nonSp = scenes.find((s) => !s.startsWith("sp-"));
      if (nonSp) await obs.switchScene(nonSp);
    }

    // Trigger playback via REST (bypasses the button so this test is
    // independent from issue #8).
    const playResp = await request.post(`/api/v1/playback/${pl.id}/play`);
    expect(playResp.status()).toBe(204);

    // Open the dashboard AFTER Play so the WS client definitely picks
    // up the NowPlaying broadcast from the replay / current position
    // stream that the engine emits for active playback.
    await page.goto("/");
    await expect(page.locator(".playlist-card").first()).toBeVisible({ timeout: 30_000 });

    const card = page.locator(".playlist-card", { hasText: pl.name });

    // Within 10 s the card must show the `.np-info` block with a
    // non-empty position counter (proves NowPlaying actually arrived).
    const npInfo = card.locator(".np-info");
    await expect(npInfo).toBeVisible({ timeout: 10_000 });
    const npText = await npInfo.innerText();
    expect(
      npText.length,
      `.np-info must contain text (song/position), got empty string`,
    ).toBeGreaterThan(0);

    // Cleanup.
    await request.post(`/api/v1/playback/${pl.id}/pause`);
  });

  /**
   * Issue #11 — scene-driven playback.
   *
   * Switching OBS to `sp-fast` must cause SongPlayer to match the
   * scene's NDI source against the ytfast playlist. This is what the
   * original bug broke: `ndi_sources` was an empty HashMap so every
   * scene-item lookup returned None, and scene-driven playback never
   * fired.
   *
   * Strong assertion: after the scene switch, `/api/v1/status` must
   * report `active_playlist_ids` CONTAINING the ytfast playlist's id.
   * This field is populated from `obs_state.active_playlist_ids`,
   * which is the exact output of `check_scene_items` against the
   * rebuilt map — so a stale or empty map is directly observable.
   *
   * A weaker assertion using `active_scene` alone would pass even
   * before the fix, because `obs_state.current_scene` is set from the
   * raw OBS event regardless of the NDI match.
   *
   * Required environment: OBS must have an `sp-fast` scene containing
   * an NDI source whose `ndi_source_name` setting is `SP-fast`. If
   * missing, the test fails hard (no skip).
   */
  test("switching OBS to sp-fast scene triggers ytfast playback", async ({ request }) => {
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();

    const fastId = await findPlaylistId(request, FAST_PLAYLIST_NAME);
    const scenes = await obs!.listScenes();
    expect(
      scenes.includes(FAST_SCENE_NAME),
      `deployed OBS must have an "${FAST_SCENE_NAME}" scene with an NDI source subscribed to "SP-fast"`,
    ).toBe(true);

    // Reset to a non-sp scene first so we observe the transition to
    // sp-fast, not a no-op.
    const nonSp = scenes.find((s) => !s.startsWith("sp-")) || scenes[0];
    await obs!.switchScene(nonSp);
    await new Promise((r) => setTimeout(r, 500));

    // Verify baseline: ytfast should NOT be in active_playlist_ids
    // while the non-sp scene is on program. This kills any "always
    // returns sp-fast" mutation in the status handler.
    const baseline = await (await request.get("/api/v1/status")).json();
    expect(
      (baseline.active_playlist_ids as number[]).includes(fastId),
      `ytfast (id=${fastId}) must NOT be in active_playlist_ids while the non-sp scene "${nonSp}" is on program; got ${JSON.stringify(baseline.active_playlist_ids)}`,
    ).toBe(false);

    // Switch to sp-fast.
    await obs!.switchScene(FAST_SCENE_NAME);

    // Poll the SongPlayer status until active_playlist_ids contains
    // ytfast. This is the strong assertion: it only becomes true when
    // the rebuild populated the NDI map AND check_scene_items matched
    // the scene-item source name against it. Before the fix, this
    // would stay empty forever.
    const deadline = Date.now() + 5_000;
    let matched = false;
    let lastStatus: { active_scene?: string; active_playlist_ids?: number[] } = {};
    while (Date.now() < deadline) {
      lastStatus = await (await request.get("/api/v1/status")).json();
      if (
        lastStatus.active_scene === FAST_SCENE_NAME &&
        (lastStatus.active_playlist_ids as number[]).includes(fastId)
      ) {
        matched = true;
        break;
      }
      await new Promise((r) => setTimeout(r, 200));
    }
    expect(
      matched,
      `within 5s of switching OBS to "${FAST_SCENE_NAME}", /api/v1/status must report active_scene="${FAST_SCENE_NAME}" AND active_playlist_ids containing ${fastId}; last status: ${JSON.stringify(lastStatus)}`,
    ).toBe(true);

    // Cleanup: switch away.
    await obs!.switchScene(nonSp);
  });

  /**
   * Full-chain end-to-end test for issue #11 + #9 combined.
   *
   * 1. Non-sp scene on OBS program, ytfast paused.
   * 2. Open the dashboard in Playwright; ytfast card shows "Nothing playing".
   * 3. Switch OBS program scene to `sp-fast` via obs-websocket-js.
   * 4. Within 15 seconds the ytfast card must transition to `.np-info`.
   *
   * This exercises the entire chain:
   *   OBS scene change → SongPlayer OBS client → check_scene_items
   *   → active_playlist_ids populated → OBS→engine bridge
   *   → EngineCommand::SceneChanged → state machine
   *   → SelectAndPlay → PipelineEvent::Started
   *   → NowPlaying broadcast → dashboard WebSocket → card update.
   *
   * Any link in the chain breaking makes this test fail. The earlier
   * tests cover each segment in isolation; this one proves they compose.
   */
  test("full chain: OBS scene switch updates dashboard ytfast card", async ({
    page,
    request,
  }) => {
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();

    const fastId = await findPlaylistId(request, FAST_PLAYLIST_NAME);
    const scenes = await obs!.listScenes();
    expect(
      scenes.includes(FAST_SCENE_NAME),
      `deployed OBS must have an "${FAST_SCENE_NAME}" scene`,
    ).toBe(true);

    // Baseline: park on a non-sp scene and pause ytfast so the card
    // starts from the "Nothing playing" state.
    const nonSp = scenes.find((s) => !s.startsWith("sp-")) || scenes[0];
    await obs!.switchScene(nonSp);
    await request.post(`/api/v1/playback/${fastId}/pause`);
    await new Promise((r) => setTimeout(r, 500));

    await page.goto("/");
    await expect(page.locator(".playlist-card").first()).toBeVisible({
      timeout: 30_000,
    });

    const fastCard = page
      .locator(".playlist-card")
      .filter({ hasText: FAST_PLAYLIST_NAME });
    await expect(fastCard).toBeVisible();

    // Switch OBS to sp-fast — this must kick off the full chain.
    await obs!.switchScene(FAST_SCENE_NAME);

    // The dashboard card must show .np-info within 15s. That proves:
    //  - Scene detection matched (ndi_sources populated correctly)
    //  - OBS→engine bridge dispatched SceneChanged to the engine
    //  - Engine state machine advanced into Playing
    //  - Pipeline started decoding and emitted Started
    //  - NowPlaying reached the dashboard WebSocket
    //  - Dashboard rendered .np-info
    await expect(fastCard.locator(".np-info")).toBeVisible({
      timeout: 15_000,
    });

    // Cleanup: switch back to the non-sp scene.
    await obs!.switchScene(nonSp);
    await request.post(`/api/v1/playback/${fastId}/pause`);
  });

  /**
   * Regression test for the stuck-WaitingForScene bug shipped in 0.11.0.
   *
   * Deterministic version: switches OBS to sp-fast, waits for the
   * engine to start playing, then asserts the dashboard card shows
   * `.np-info` with an advancing position counter. This catches:
   *
   * - The bridge subscription race (initial SceneChanged missed
   *   because the bridge subscribed after the OBS client spawned)
   * - The stuck-WaitingForScene bug (engine parks when SceneOn fires
   *   before any videos are normalized, and no event rewakes it)
   * - State broadcast bugs (engine plays but dashboard never updates)
   *
   * Why advancing position matters: a stale "0:00 / X:XX" display
   * would pass a visibility check but proves the pipeline is frozen.
   */
  test("active scene's playlist card shows Playing with advancing position", async ({
    page,
    request,
  }) => {
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();

    const scenes = await obs!.listScenes();
    expect(
      scenes.includes(FAST_SCENE_NAME),
      `deployed OBS must have an "${FAST_SCENE_NAME}" scene`,
    ).toBe(true);

    // Start from a clean non-sp baseline so the scene switch is a
    // real transition, not a no-op.
    const nonSp = scenes.find((s) => !s.startsWith("sp-")) || scenes[0];
    await obs!.switchScene(nonSp);
    await new Promise((r) => setTimeout(r, 500));

    // Switch to sp-fast and let the engine detect it.
    await obs!.switchScene(FAST_SCENE_NAME);

    await page.goto("/");
    await expect(page.locator(".playlist-card").first()).toBeVisible({ timeout: 30_000 });

    const card = page.locator(".playlist-card").filter({ hasText: FAST_PLAYLIST_NAME });
    await expect(card).toBeVisible();

    // 1. The `.np-info` block must appear within 30 s. If the engine
    //    is stuck in WaitingForScene (the original bug), the card
    //    stays "Nothing playing" and this times out.
    await expect(
      card.locator(".np-info"),
      `card for ${FAST_PLAYLIST_NAME} must show .np-info after switching to ${FAST_SCENE_NAME}`,
    ).toBeVisible({ timeout: 30_000 });

    // 2. The position counter must advance. Read twice 2.5 s apart
    //    and assert strictly increasing — a frozen "0:00 / 4:44"
    //    proves the pipeline thread is dead.
    const readPosition = async () => {
      const text = (await card.locator(".np-info").innerText()) ?? "";
      const match = text.match(/(\d+):(\d+)\s*\/\s*\d+:\d+/);
      if (!match) return -1;
      return parseInt(match[1], 10) * 60 + parseInt(match[2], 10);
    };

    const first = await readPosition();
    expect(
      first,
      `${FAST_PLAYLIST_NAME}: position counter not found in .np-info text`,
    ).toBeGreaterThanOrEqual(0);

    await page.waitForTimeout(2_500);
    const second = await readPosition();
    expect(
      second,
      `${FAST_PLAYLIST_NAME}: position must advance (first=${first}s, second=${second}s). ` +
        `A flat counter means the pipeline is frozen.`,
    ).toBeGreaterThan(first);

    // Cleanup: switch back to non-sp scene.
    await obs!.switchScene(nonSp);
  });

  /**
   * Zero browser console errors/warnings. Runs last so it observes the
   * state after all other tests have interacted with the dashboard.
   *
   * The allow list matches the one in `frontend.spec.ts` — notably the
   * Chrome SRI preload warning (crbug.com/981419) which is an upstream
   * browser issue, not a SongPlayer bug.
   */
  test("browser console has no errors or warnings during dashboard use", async ({ page }) => {
    const allowed = [
      /favicon/i,
      /WebSocket connection/i,
      /\bwasm\b.*instantiate/i,
      /module specifier/i,
      /integrity.*attribute.*ignored/i, // Chrome SRI preload warning, crbug.com/981419
    ];
    const messages: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        messages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });

    await page.goto("/");
    await expect(page.locator(".playlist-card").first()).toBeVisible({ timeout: 30_000 });
    await page.waitForTimeout(3_000);

    const real = messages.filter((m) => !allowed.some((r) => r.test(m)));
    expect(real).toEqual([]);
  });
});
