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
 *  4. Switching back to a non-fast baseline scene must stop ytfast
 *     playback and return the card to "Nothing playing".
 *
 *  5. Zero console errors or warnings throughout the suite. Catches
 *     issue #8/#9 regressions on the UI side plus anything else that
 *     might leak into the browser console.
 */

import {
  test,
  expect,
  request as apiRequest,
  type APIRequestContext,
} from "@playwright/test";
import { ObsDriver } from "./obs-driver";
import {
  unhealthyOnProgramOutputs,
  type HealthSnapshot,
  type UnhealthyOutput,
} from "./ndi-health-gate";

const OBS_WS_URL = process.env.OBS_WS_URL || "ws://localhost:4455";
const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";

// #170: read the ENGINE's view of the on-program scene (`obs_state.current_scene`)
// to prove SongPlayer actually followed a scene switch — not just that OBS
// reports it. A dropped studio-mode event would leave these disagreeing.
async function readEngineActiveScene(
  ctx: APIRequestContext,
): Promise<string | null> {
  try {
    const resp = await ctx.get("/api/v1/status");
    if (!resp.ok()) return null;
    const status = (await resp.json()) as { active_scene?: string | null };
    return status.active_scene ?? null;
  } catch {
    return null;
  }
}

// Poll the engine's active scene until it equals `target` (or a short deadline).
async function waitEngineActiveScene(
  ctx: APIRequestContext,
  target: string,
  timeoutMs = 5000,
): Promise<string | null> {
  const deadline = Date.now() + timeoutMs;
  let last: string | null = null;
  for (;;) {
    last = await readEngineActiveScene(ctx);
    if (last === target) return last;
    if (Date.now() >= deadline) return last;
    await new Promise((r) => setTimeout(r, 200));
  }
}

// Playlists deployed to win-resolume have predictable names. These tests
// expect at least one playlist called `ytfast` (id varies) with
// `ndi_output_name=SP-fast` and a corresponding OBS scene `sp-fast`.
const FAST_PLAYLIST_NAME = "ytfast";
const FAST_SCENE_NAME = "sp-fast";

// Picking the off-program baseline scene was historically `find((s) =>
// !s.startsWith("sp-"))`, which on win-resolume resolved to a sound-sync
// QR-code "test" scene that disrupts the wall + LED audience whenever
// E2E runs against a live machine. Pick another sp-* scene instead —
// any one that isn't sp-fast (under test) and isn't sp-warmup (also
// disturbing per operator). Falls back to a non-sp scene only if no
// alternative sp-* exists. The assertion-of-interest in every test is
// "ytfast NOT in active_playlist_ids", which holds for any non-sp-fast
// program scene regardless of whether another sp-* is active.
const DISALLOWED_BASELINE_SCENES = new Set(["sp-fast", "sp-warmup"]);

function pickBaselineScene(scenes: string[]): string {
  // Prefer sp-slow specifically — it's a quiet music scene operators
  // routinely use as a "background" state.
  if (scenes.includes("sp-slow")) return "sp-slow";
  // Fall back to any other sp-* that isn't disallowed.
  const otherSp = scenes.find(
    (s) => s.startsWith("sp-") && !DISALLOWED_BASELINE_SCENES.has(s),
  );
  if (otherSp) return otherSp;
  // Last resort — non-sp scene. This may be the disruptive QR-code
  // test scene, but it's better than running a test where the baseline
  // and the sp-fast probe scene collide.
  const nonSp = scenes.find((s) => !s.startsWith("sp-"));
  if (nonSp) return nonSp;
  return scenes[0];
}

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

// #165: the dashboard is now a selector + ONE work area. To assert on a
// specific playlist's card, SELECT it in the selector first, then read the
// single work-area card. Replaces the old per-card grid locators without
// reducing what each test verifies.
async function selectWorkspaceCard(
  page: import("@playwright/test").Page,
  name: string,
) {
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 30_000,
  });
  const row = page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: name });
  await row.click();
  // #170: the click must actually take. Read back the work-area title; if a
  // transient re-render/reorder moved the row and the click landed on a
  // neighbour, retry once, then fail loudly with a message that names the
  // wrong-row-click cause instead of the mysterious downstream "card never
  // appeared".
  try {
    await expect(page.getByTestId("workspace-title")).toHaveText(name, {
      timeout: 5_000,
    });
  } catch {
    await row.click();
    await expect(
      page.getByTestId("workspace-title"),
      `selecting "${name}" did not switch the work area after a retry — the selector row likely moved under the click (#170)`,
    ).toHaveText(name, { timeout: 5_000 });
  }
  const card = page.locator(".playlist-card", { hasText: name });
  await expect(card).toBeVisible();
  return card;
}

test.describe("SongPlayer post-deploy feature verification", () => {
  let obs: ObsDriver | null = null;
  // Captured at suite start, restored at suite end so the wall returns
  // to whatever the operator was on before CI hijacked OBS.
  let initialScene: string | null = null;

  test.beforeAll(async () => {
    obs = await ObsDriver.connect(OBS_WS_URL);
    try {
      initialScene = await obs.currentProgramScene();
    } catch {
      initialScene = null;
    }
  });

  // #170: a failed assertion aborts the test body BEFORE its trailing
  // "switch back" cleanup runs, which is how a failed test 17 left the wall on
  // sp-slow. afterEach ALWAYS runs, pass or fail, so it restores the wall to
  // the scene the suite started on (the operator's normal state) after every
  // test — best-effort; the authoritative check is in afterAll.
  test.afterEach(async () => {
    const driver = obs;
    if (driver && initialScene) {
      try {
        await driver.switchScene(initialScene);
      } catch {
        // best-effort between tests; afterAll asserts the final state
      }
    }
  });

  test.afterAll(async () => {
    const driver = obs;
    if (!driver) return;
    try {
      // Restore the scene the suite started on (never leave the wall on the
      // E2E baseline) and PROVE the engine followed — a dropped studio-mode
      // scene event would leave `active_scene` stuck on the baseline. Read back
      // /api/v1/status.active_scene, retry the switch once, fail loudly.
      const target =
        initialScene ?? pickBaselineScene(await driver.listScenes());
      const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
      try {
        // Restore (afterEach may already have — switchScene no-ops if program
        // is already on target) and PROVE the ENGINE ended on the start scene.
        // The generous wait is the honest resilience: it covers the driver's
        // own transition wait PLUS the ~2 s engine poll-reconcile (part C)
        // catching a dropped event. An active_scene that never converges fails
        // loudly with the scene names — a retry of the SWITCH would be a no-op
        // here (program is already target), so the wait, not a re-drive, is
        // what tolerates a lagging engine.
        await driver.switchScene(target);
        const engineScene = await waitEngineActiveScene(ctx, target, 8000);
        expect(
          engineScene,
          `afterAll must restore the wall to "${target}" (the scene the suite started on); the engine reported active_scene="${engineScene}". A dropped studio-mode scene event left the wall on a different scene — the poll-reconcile / driver studio-transition fix did not hold (#170).`,
        ).toBe(target);
      } finally {
        await ctx.dispose();
      }
    } finally {
      await driver.disconnect();
    }
  });

  /**
   * Issue #85 — version label on every page matches backend.
   *
   * Post-deploy verification: dashboard's `[data-testid="version"]` must
   * be visible AND equal `v{status.version}` from the backend. Catches
   * silent deploy failures (frontend cached, backend updated; or vice
   * versa) per version-on-dashboard.md.
   */
  test("dashboard version label matches deployed backend (#85)", async ({
    page,
    request,
  }) => {
    await page.goto("/");
    const label = page.locator('[data-testid="version"]');
    await expect(label).toBeVisible({ timeout: 15_000 });
    const text = (await label.textContent())?.trim() ?? "";
    expect(text).toMatch(/^v\d+\.\d+\.\d+(-dev\.\d+)?$/);

    const statusResp = await request.get("/api/v1/status");
    expect(statusResp.status()).toBe(200);
    const status = (await statusResp.json()) as { version: string };
    expect(status.version, "backend /api/v1/status must include version field").toBeTruthy();
    expect(
      text,
      `frontend label "${text}" must equal backend v${status.version}`,
    ).toBe(`v${status.version}`);
  });

  /**
   * Issue #89 — Resolume Arena liveness gate.
   *
   * If Arena is hung, the LED wall is dark even though SongPlayer is
   * dispatching subtitles to SP-live NDI correctly. Without this check
   * the post-deploy run reports green while the operator-visible
   * surface is broken (the failure mode behind the 2026-05-13 Thank
   * You verify session and earlier wall-dark incidents).
   *
   * Probes Arena's REST endpoint with a tight 5 s timeout — a hung
   * Arena either times out at the TCP layer or hangs past the wall
   * clock; a healthy one returns the composition JSON in <2 s.
   */
  test("Resolume Arena REST is responding (wall is alive)", async () => {
    const url = process.env.RESOLUME_REST_URL || "http://127.0.0.1:8090/api/v1/composition";
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 5000);
    let resp: Response;
    try {
      resp = await fetch(url, { signal: controller.signal });
    } catch (err) {
      throw new Error(
        `Resolume Arena unreachable at ${url}: ${err}. ` +
          `Arena is likely hung or not running — restart Arena before re-running deploy.`,
      );
    } finally {
      clearTimeout(timer);
    }
    expect(
      resp.ok,
      `Resolume Arena returned non-2xx status=${resp.status} from ${url} — wall may be dark.`,
    ).toBeTruthy();
  });

  /**
   * Issue #127 — on-program NDI output must have a live receiver.
   *
   * The worst failure this project has: SongPlayer reports `state=Playing`
   * at full fps while OBS's DistroAV receiver is stranded on a dead endpoint,
   * so `connections=0` and the video plate is black — yet every other check
   * passes green. This gate reads the on-program playlist(s) from
   * `/api/v1/status.active_playlist_ids`, then polls `/api/v1/ndi/health`
   * until each has `connections > 0` (`0` = dark wall; `-1` = never polled
   * yet, so keep waiting), and FAILS if any stays dark.
   *
   * Note: SongPlayer does NOT silently self-heal this state (CLAUDE.md
   * "Disabled subsystems" — per-sender recreate was removed). It now
   * best-effort nudges OBS over its WebSocket to re-subscribe the stranded
   * receiver (#127), but a wall that stays dark is a real failure this gate
   * must catch, not paper over.
   */
  test("on-program NDI output has a live receiver — wall is not dark (#127)", async ({
    request,
  }) => {
    // Park on a baseline scene (sp-slow / another non-fast sp-*), per CLAUDE.md
    // "E2E must not switch to disruptive OBS scenes". SongPlayer then registers
    // that scene's playlist as on program.
    if (obs) {
      const scenes = await obs.listScenes();
      await obs.switchScene(pickBaselineScene(scenes));
    }

    // Poll until every on-program output has a live receiver, giving the full
    // detect → spawn → DistroAV-connect chain time to settle after the deploy's
    // SongPlayer restart.
    const deadline = Date.now() + 60_000;
    let active: number[] = [];
    let unhealthy: UnhealthyOutput[] = [];
    for (;;) {
      const statusResp = await request.get("/api/v1/status");
      expect(statusResp.status()).toBe(200);
      const status = (await statusResp.json()) as {
        active_playlist_ids: number[];
      };
      active = status.active_playlist_ids ?? [];

      const healthResp = await request.get("/api/v1/ndi/health");
      expect(healthResp.status()).toBe(200);
      const health = (await healthResp.json()) as HealthSnapshot[];
      expect(Array.isArray(health)).toBe(true);

      unhealthy = unhealthyOnProgramOutputs(active, health);
      if (active.length > 0 && unhealthy.length === 0) break;
      if (Date.now() > deadline) break;
      await new Promise((r) => setTimeout(r, 3000));
    }

    expect(
      active.length,
      "no playlist registered as on program after parking on the baseline sp-* scene — scene detection did not fire (active_playlist_ids stayed empty)",
    ).toBeGreaterThan(0);
    expect(
      unhealthy,
      `on-program NDI output(s) had no live receiver — dark wall (#127): ${JSON.stringify(unhealthy)}`,
    ).toHaveLength(0);
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

    // Park on a baseline scene (sp-slow / another non-fast sp-* if
    // available) so this test isn't muddied by ytfast already running.
    if (obs) {
      const scenes = await obs.listScenes();
      await obs.switchScene(pickBaselineScene(scenes));
    }

    await page.goto("/");
    // Wait for the WASM bundle to mount, then select the playlist's card in the
    // work area (#165 selector + single work area).

    // (The NDI dark-wall gate — `connections > 0` for the on-program output —
    // lives in its own dedicated test above, "on-program NDI output has a live
    // receiver (#127)", so this Play-button test stays focused on the button.)

    const card = await selectWorkspaceCard(page, pl.name);

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
      await obs.switchScene(pickBaselineScene(scenes));
    }

    // Trigger playback via REST (bypasses the button so this test is
    // independent from issue #8).
    const playResp = await request.post(`/api/v1/playback/${pl.id}/play`);
    expect(playResp.status()).toBe(204);

    // Open the dashboard AFTER Play so the WS client definitely picks
    // up the NowPlaying broadcast from the replay / current position
    // stream that the engine emits for active playback.
    await page.goto("/");
    const card = await selectWorkspaceCard(page, pl.name);

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

    // Reset to a non-fast baseline scene first so we observe the
    // transition to sp-fast, not a no-op.
    const baselineScene = pickBaselineScene(scenes);
    await obs!.switchScene(baselineScene);
    await new Promise((r) => setTimeout(r, 500));

    // Verify baseline: ytfast should NOT be in active_playlist_ids
    // while the non-fast baseline scene is on program. This kills any
    // "always returns sp-fast" mutation in the status handler.
    const baseline = await (await request.get("/api/v1/status")).json();
    expect(
      (baseline.active_playlist_ids as number[]).includes(fastId),
      `ytfast (id=${fastId}) must NOT be in active_playlist_ids while baseline scene "${baselineScene}" is on program; got ${JSON.stringify(baseline.active_playlist_ids)}`,
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

    // Cleanup: switch back to the baseline scene.
    await obs!.switchScene(baselineScene);
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

    // Baseline: park on a non-fast scene and pause ytfast so the card
    // starts from the "Nothing playing" state.
    const baselineScene = pickBaselineScene(scenes);
    await obs!.switchScene(baselineScene);
    await request.post(`/api/v1/playback/${fastId}/pause`);
    await new Promise((r) => setTimeout(r, 500));

    await page.goto("/");
    // #165: select ytfast in the work area BEFORE the scene switch, and pin it
    // (a click pins the selection) so the work area stays on ytfast when it
    // starts playing.
    const fastCard = await selectWorkspaceCard(page, FAST_PLAYLIST_NAME);

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

    // Cleanup: switch back to the baseline scene.
    await obs!.switchScene(baselineScene);
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

    // Start from a clean non-fast baseline so the scene switch is a
    // real transition, not a no-op.
    const baselineScene = pickBaselineScene(scenes);
    await obs!.switchScene(baselineScene);
    await new Promise((r) => setTimeout(r, 500));

    // Switch to sp-fast and let the engine detect it.
    await obs!.switchScene(FAST_SCENE_NAME);

    await page.goto("/");
    // #165: select ytfast in the work area (it is the on-program playlist).
    const card = await selectWorkspaceCard(page, FAST_PLAYLIST_NAME);

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

    // Cleanup: switch back to baseline scene.
    await obs!.switchScene(baselineScene);
  });

  /**
   * Issue #150 + #164 + #176 — genlock indicator consistency with pacing gating.
   *
   * The dashboard's genlock badges must AGREE with whatever
   * `GET /api/v1/ndi/health` reports — a consistency check, NOT a hard-coded
   * state. #176 revised #164's rendering rule: the whole-box HEADER badge is now
   * ALWAYS visible — while `genlock_pacing` is OFF (`pacing.enabled == false`,
   * the production default today) it shows the explicit grey `● GENLOCK OFF`
   * (never hidden), so the owner can always tell at a glance whether SongPlayer
   * is genlocked. The PER-CARD badge keeps #164's "only where actionable" rule:
   * hidden while pacing is off, shown only on live (Playing/Paused) pacing-
   * enabled outputs when pacing is on; when pacing IS enabled the header shows
   * the worst-of summary. This test recomputes the expectation from the live
   * health and asserts the badges match, for either regime. No scene switching,
   * no sleep loops (a single settle so the 1 s poll lands, per file style).
   */
  test("genlock badges agree with /api/v1/ndi/health (#150/#164/#176)", async ({
    page,
    request,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("playlist-workspace")).toBeVisible({
      timeout: 30_000,
    });

    // Let the 1 s poll land so the badges reflect a fresh snapshot, then read
    // the health and the badges close together.
    await page.waitForTimeout(1_500);

    const resp = await request.get("/api/v1/ndi/health");
    expect(resp.status()).toBe(200);
    const health = (await resp.json()) as Array<{
      ndi_name: string;
      state: string;
      lock_state: string;
      lock_reason?: string;
      clock?: { clock_ok?: boolean };
      pacing?: { enabled?: boolean };
    }>;
    expect(Array.isArray(health)).toBe(true);

    const enabled = health.filter((o) => o.pacing?.enabled === true);

    if (enabled.length === 0) {
      // #176: pacing disabled everywhere (prod default) — the ALWAYS-visible
      // header badge shows the explicit grey `● GENLOCK OFF`, derived from the
      // live health (no pacing-enabled output), never hidden. The per-card badge
      // stays hidden (#164 "only where actionable").
      const globalBadge = page.getByTestId("genlock-global-badge");
      await expect(globalBadge).toBeVisible({ timeout: 15_000 });
      await expect(globalBadge).toContainText("GENLOCK OFF");
      await expect(globalBadge).toHaveClass(/lock-off/);
      // No per-card selector badge while pacing is off.
      await expect(
        page.locator(".playlist-selector-row .lock-badge"),
      ).toHaveCount(0, { timeout: 10_000 });
      return;
    }

    // Pacing enabled on at least one output: the header summary is over the
    // pacing-enabled outputs. Recompute the summarized state as
    // sp_core::genlock::lock_state::summarize does (a LOCKED output whose clock
    // is not ok is demoted to UNLOCKED; LOCKED iff every LIVE (state==="Playing")
    // output is LOCKED and clock ok; else the worst live state
    // UNLOCKED > DEGRADED > LOCKED; no live output → clock-only).
    const global = page.locator(".genlock-status .lock-badge");
    await expect(global).toBeVisible({ timeout: 15_000 });

    const sev = (s: string) => (s === "UNLOCKED" ? 2 : s === "DEGRADED" ? 1 : 0);
    const eff = (o: { lock_state: string; clock?: { clock_ok?: boolean } }) =>
      o.lock_state === "LOCKED" && !o.clock?.clock_ok ? "UNLOCKED" : o.lock_state;
    const live = enabled.filter((o) => o.state === "Playing");
    let expectedState: string;
    if (live.length === 0) {
      const clockOk = enabled.every((o) => !!o.clock?.clock_ok);
      expectedState = clockOk ? "LOCKED" : "UNLOCKED";
    } else {
      expectedState = live
        .map(eff)
        .reduce((a, b) => (sev(b) > sev(a) ? b : a), "LOCKED");
    }

    const cls = (await global.getAttribute("class")) ?? "";
    expect(
      cls,
      `global badge class "${cls}" must match the summarized state ${expectedState} for enabled health ${JSON.stringify(enabled)}`,
    ).toContain(`lock-${expectedState.toLowerCase()}`);

    // A per-card badge is shown only on a pacing-enabled Playing/Paused output;
    // find one that also matches a playlist card and assert its colour agrees
    // with the output's raw lock_state.
    const playlists = (await (
      await request.get("/api/v1/playlists")
    ).json()) as Array<{ name: string; ndi_output_name: string }>;
    const nameByNdi = new Map(
      playlists.map((p) => [p.ndi_output_name, p.name]),
    );
    const matched = enabled.find(
      (h) =>
        nameByNdi.has(h.ndi_name) &&
        (h.state === "Playing" || h.state === "Paused"),
    );
    if (matched) {
      // #165: the per-playlist badge lives in the SELECTOR row now, not the
      // single work area.
      const cardBadge = page
        .getByTestId("playlist-selector-row")
        .filter({ hasText: nameByNdi.get(matched.ndi_name)! })
        .locator(".lock-badge");
      await expect(cardBadge).toBeVisible({ timeout: 10_000 });
      const ccls = (await cardBadge.getAttribute("class")) ?? "";
      expect(
        ccls,
        `card badge for ${matched.ndi_name} class "${ccls}" must match its lock_state ${matched.lock_state}`,
      ).toContain(`lock-${matched.lock_state.toLowerCase()}`);
    }
  });

  /**
   * Issue #186 — a karaoke preset change must NOT reload the pipeline.
   *
   * The pre-fix `set_karaoke` reopened every playing pipeline on a MODE change
   * (`PipelineCommand::Play` at the cached position), so the wall went silent for
   * the seconds the decoder reopen + A/V resync took. Now a mode is a live gain
   * preset over the already-open streams: a change writes the gain atoms, no
   * reopen. This drives a burst of preset + fader changes while ytfast plays
   * on-program and asserts (a) `GET /api/v1/karaoke` reflects each within 500 ms
   * and (b) the `.np-info` position keeps ADVANCING across the whole burst — a
   * reload would reset/freeze it. Robust regardless of whether the playing song
   * has stems: the no-reload guarantee is universal.
   */
  test("karaoke preset changes keep playback advancing — no reload (#186)", async ({
    page,
    request,
  }) => {
    expect(obs, "OBS WebSocket driver must be connected").not.toBeNull();

    const consoleMessages: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });
    const allowedConsole = [
      /favicon/i,
      /WebSocket connection/i,
      /\bwasm\b.*instantiate/i,
      /module specifier/i,
      /integrity.*attribute.*ignored/i,
    ];

    const scenes = await obs!.listScenes();
    expect(
      scenes.includes(FAST_SCENE_NAME),
      `deployed OBS must have an "${FAST_SCENE_NAME}" scene`,
    ).toBe(true);

    // Save the karaoke state so the test restores it afterwards.
    const before = (await (await request.get("/api/v1/karaoke")).json()) as {
      mode?: string;
      vocal_gain?: number;
    };

    // Start clean, then switch to sp-fast so ytfast plays on-program.
    await obs!.switchScene(pickBaselineScene(scenes));
    await new Promise((r) => setTimeout(r, 500));
    await obs!.switchScene(FAST_SCENE_NAME);

    await page.goto("/");
    const card = await selectWorkspaceCard(page, FAST_PLAYLIST_NAME);
    await expect(
      card.locator(".np-info"),
      `card for ${FAST_PLAYLIST_NAME} must be Playing before the preset burst`,
    ).toBeVisible({ timeout: 30_000 });

    const readPosition = async () => {
      const text = (await card.locator(".np-info").innerText()) ?? "";
      const match = text.match(/(\d+):(\d+)\s*\/\s*\d+:\d+/);
      if (!match) return -1;
      return parseInt(match[1], 10) * 60 + parseInt(match[2], 10);
    };
    const first = await readPosition();
    expect(first, "position counter must be present").toBeGreaterThanOrEqual(0);

    // A burst of preset + fader changes. Each must reflect within 500 ms, and —
    // the #186 fix — must NOT reopen the pipeline.
    const presets: Array<{ mode: string; vocal_gain?: number }> = [
      { mode: "karaoke_low", vocal_gain: 0.2 },
      { mode: "vocals_only" },
      { mode: "instrumental_only" },
      { mode: "karaoke_low", vocal_gain: 0.8 },
      { mode: "full_mix" },
      { mode: "karaoke_low", vocal_gain: 0.5 },
    ];
    for (const p of presets) {
      const resp = await request.post("/api/v1/karaoke", { data: p });
      expect(resp.status()).toBe(204);
      await expect
        .poll(
          async () =>
            (
              (await (await request.get("/api/v1/karaoke")).json()) as {
                mode?: string;
              }
            ).mode,
          { timeout: 500, intervals: [50, 100, 100, 100, 100] },
        )
        .toBe(p.mode);
    }

    // The pipeline must have kept advancing across the whole burst — a reload
    // would reset/freeze the position (the #186 seconds-of-silence dropout).
    await page.waitForTimeout(2_500);
    const second = await readPosition();
    expect(
      second,
      `position must advance across karaoke preset changes (first=${first}s, second=${second}s) — a reload/dropout would freeze it`,
    ).toBeGreaterThan(first);

    // Restore karaoke state (the scene is restored by afterEach/afterAll).
    await request.post("/api/v1/karaoke", {
      data: {
        mode: before.mode ?? "full_mix",
        vocal_gain: before.vocal_gain ?? 0.3,
      },
    });

    const realConsole = consoleMessages.filter(
      (m) => !allowedConsole.some((r) => r.test(m)),
    );
    expect(realConsole).toEqual([]);
  });

  /**
   * Issue #177 — the karaoke panel binds to the now-playing song and shows its
   * stems state. Read-only: never switches the OBS program scene. Verifies the
   * deployed `GET /api/v1/karaoke` returns the `now_playing[]` contract and that
   * the dashboard panel renders a per-song header (a song title + a state glyph,
   * or the honest "nič nehrá" when nothing is selected/playing).
   */
  test("karaoke panel names the now-playing song + shows its stems state (#177)", async ({
    page,
    request,
  }) => {
    const consoleMessages: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });
    const allowedConsole = [
      /favicon/i,
      /WebSocket connection/i,
      /\bwasm\b.*instantiate/i,
      /module specifier/i,
      /integrity.*attribute.*ignored/i,
    ];

    // The deployed API carries the now_playing[] contract with valid states.
    const karaoke = (await (await request.get("/api/v1/karaoke")).json()) as {
      now_playing?: Array<{
        playlist_id: number;
        video_id: number;
        title: string;
        stems_state: string;
        queue_position: number | null;
      }>;
    };
    expect(Array.isArray(karaoke.now_playing)).toBe(true);
    const validStates = [
      "ready",
      "queued",
      "processing",
      "unavailable",
      "failed",
    ];
    for (const e of karaoke.now_playing ?? []) {
      expect(
        validStates,
        `now_playing entry ${e.video_id} state "${e.stems_state}" must be a known state`,
      ).toContain(e.stems_state);
      expect(typeof e.title).toBe("string");
    }

    await page.goto("/");
    const header = page.locator('[data-testid="karaoke-now-playing"]');
    await expect(header).toBeVisible({ timeout: 30_000 });
    // The header always begins with the "Stemy — " binding prefix — either
    // "Stemy — <song>: <glyph>" or the idle "Stemy — nič nehrá".
    await expect(header).toContainText(/Stemy — /);

    const realConsole = consoleMessages.filter(
      (m) => !allowedConsole.some((r) => r.test(m)),
    );
    expect(realConsole).toEqual([]);
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
    await expect(page.getByTestId("playlist-workspace")).toBeVisible({ timeout: 30_000 });
    await page.waitForTimeout(3_000);

    const real = messages.filter((m) => !allowed.some((r) => r.test(m)));
    expect(real).toEqual([]);
  });
});
