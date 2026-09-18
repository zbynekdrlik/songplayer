import { test, expect, Page } from "@playwright/test";

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

// #164 genlock fixtures. `pacing.enabled` decides badge visibility: a
// pacing-disabled output shows NO badge; a pacing-enabled output shows one only
// while its state is Playing/Paused. All three names match a playlist card
// (SP-worship→Worship, SP-background→Background, SP-live→ytlive).
const GENLOCK_ENABLED_FIXTURE = [
  {
    ndi_name: "SP-worship", playlist_id: 1, state: "Playing", connections: 2,
    lock_state: "LOCKED", lock_reason: "locked",
    clock: { is_locked: true, mode: "LOCK", offset_ns: 100, clock_ok: true },
    pacing: { enabled: true }, audio: {},
  },
  {
    ndi_name: "SP-background", playlist_id: 2, state: "Playing", connections: 0,
    lock_state: "DEGRADED", lock_reason: "no receiver",
    clock: { is_locked: true, mode: "LOCK", offset_ns: 120, clock_ok: true },
    pacing: { enabled: true }, audio: {},
  },
  {
    ndi_name: "SP-live", playlist_id: 184, state: "Idle", connections: 0,
    lock_state: "UNLOCKED", lock_reason: "pacing disabled",
    clock: { is_locked: false, mode: "", offset_ns: null, clock_ok: false },
    pacing: { enabled: false }, audio: {},
  },
];

const GENLOCK_ALL_DISABLED_FIXTURE = [
  {
    ndi_name: "SP-worship", playlist_id: 1, state: "Playing", connections: 2,
    lock_state: "UNLOCKED", lock_reason: "pacing disabled",
    clock: { is_locked: false, mode: "", offset_ns: null, clock_ok: false },
    pacing: { enabled: false }, audio: {},
  },
  {
    ndi_name: "SP-background", playlist_id: 2, state: "Playing", connections: 0,
    lock_state: "UNLOCKED", lock_reason: "pacing disabled",
    clock: { is_locked: false, mode: "", offset_ns: null, clock_ok: false },
    pacing: { enabled: false }, audio: {},
  },
  {
    ndi_name: "SP-live", playlist_id: 184, state: "Idle", connections: 0,
    lock_state: "UNLOCKED", lock_reason: "pacing disabled",
    clock: { is_locked: false, mode: "", offset_ns: null, clock_ok: false },
    pacing: { enabled: false }, audio: {},
  },
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("dashboard loads and shows title", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });
});

test("dashboard shows a selector row per playlist and one work area (#165)", async ({
  page,
}) => {
  // #165: the dashboard is a selector (one row per playlist) + ONE work area
  // showing the selected playlist — not a grid of every card. The mock marks
  // playlist 1 (Worship) Playing, so it is preselected in the work area.
  await page.goto("/");
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 10000,
  });
  // A selector row exists for each of the 3 mock playlists.
  await expect(
    page.getByTestId("playlist-selector-row").filter({ hasText: "Worship" }),
  ).toBeVisible();
  await expect(
    page.getByTestId("playlist-selector-row").filter({ hasText: "Background" }),
  ).toBeVisible();
  await expect(
    page.getByTestId("playlist-selector-row").filter({ hasText: "ytlive" }),
  ).toBeVisible();
  // Exactly one work area, and it shows the playing playlist (Worship).
  await expect(page.getByTestId("playlist-workspace")).toHaveCount(1);
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
});

test("settings tab navigates", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });
  await page.click("text=Settings");
  await expect(page.locator("text=OBS WebSocket")).toBeVisible({
    timeout: 5000,
  });
});

test("status endpoint returns json", async ({ request }) => {
  const resp = await request.get("/api/v1/status");
  expect(resp.status()).toBe(200);
  const json = await resp.json();
  expect(json).toHaveProperty("obs_connected");
});

test("playlists endpoint returns data", async ({ request }) => {
  const resp = await request.get("/api/v1/playlists");
  expect(resp.status()).toBe(200);
  const json = await resp.json();
  // Mock-api ships 3 playlists — Worship, Background, and the ytlive
  // custom playlist added in v0.22.0 to exercise the /live page.
  expect(json).toHaveLength(3);
  const names = json.map((p: { name: string }) => p.name);
  expect(names).toContain("Worship");
  expect(names).toContain("Background");
  expect(names).toContain("ytlive");
});

test("settings endpoint returns data", async ({ request }) => {
  const resp = await request.get("/api/v1/settings");
  expect(resp.status()).toBe(200);
  const json = await resp.json();
  expect(json).toHaveProperty("obs_websocket_url");
  expect(json).toHaveProperty("gemini_model");
});

test("dashboard navbar shows version label matching backend (#85)", async ({
  page,
  request,
}) => {
  // Foundation per ~/devel/airuleset/modules/quality/version-on-dashboard.md:
  // every web dashboard must display the deployed version visibly on every
  // page, build-time injected from the same source as the backend.
  await page.goto("/");
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });

  const label = page.locator('[data-testid="version"]');
  await expect(label).toBeVisible();
  const text = (await label.textContent())?.trim() ?? "";
  expect(text).toMatch(/^v\d+\.\d+\.\d+(-dev\.\d+)?$/);

  const statusResp = await request.get("/api/v1/status");
  expect(statusResp.status()).toBe(200);
  const status = await statusResp.json();
  expect(status).toHaveProperty("version");
  expect(text).toBe(`v${status.version}`);
});

test("navigating away from the Dashboard does not panic a disposed signal", async ({
  page,
}) => {
  // Regression for the reactive_graph "access a reactive value that has
  // already been disposed" panic. The Dashboard's ResolumeHealthCard polls
  // /api/v1/resolume/health every 5 s from a spawn_local loop; navigating
  // away disposes the page (and the loop's page-owned signals) while the
  // loop is parked in its 5 s timer, and the loop then read a disposed
  // signal on its next wake and panicked the WASM runtime. The panic
  // surfaces via console_error_panic_hook as a console.error, which the
  // beforeEach/afterEach console collector asserts is absent.
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(page.locator("text=OBS WebSocket")).toBeVisible({
    timeout: 5000,
  });

  await page.getByRole("button", { name: "Lyrics", exact: true }).click();
  await expect(page).toHaveURL(/\/lyrics$/);

  await page.getByRole("button", { name: "Dashboard", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 5000,
  });

  // Wait LONGER than the 5 s poll interval: the disposed Dashboard's loop
  // fires its timer ~5 s after that page first mounted, so a shorter wait
  // (e.g. 2 s) would finish before the panic and pass falsely. 7 s guarantees
  // the disposed loop's timer fires inside the console-collection window.
  await page.waitForTimeout(7000);
});

// ── #164: genlock LockBadge hidden while pacing disabled ──────────────────────

test("per-card lock badge shows only on live pacing-enabled outputs (#164)", async ({
  page,
  request,
}) => {
  // SP-worship LOCKED + Playing + pacing enabled, SP-background DEGRADED +
  // Playing + pacing enabled, SP-live UNLOCKED + Idle + pacing DISABLED. The
  // badge must appear on the two live pacing-enabled cards and NOT on the
  // pacing-disabled SP-live card (#164: no '● UNLOCKED — pacing disabled' noise).
  const set = await request.post("/__mock/ndi-health", {
    data: GENLOCK_ENABLED_FIXTURE,
  });
  expect(set.ok()).toBeTruthy();

  await page.goto("/");
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 10000,
  });

  // #165: the per-playlist badge now lives in the SELECTOR rows (not the single
  // work area). Same #164 gating rule applies.
  const worshipBadge = page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Worship" })
    .locator(".lock-badge");
  await expect(worshipBadge).toBeVisible({ timeout: 5000 });
  await expect(worshipBadge).toHaveText("● LOCKED");
  await expect(worshipBadge).toHaveClass(/lock-locked/);

  const bgBadge = page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "Background" })
    .locator(".lock-badge");
  await expect(bgBadge).toBeVisible();
  await expect(bgBadge).toContainText("DEGRADED");
  await expect(bgBadge).toContainText("no receiver");
  await expect(bgBadge).toHaveClass(/lock-degraded/);

  // The pacing-disabled SP-live (ytlive) row carries NO badge at all.
  const liveBadge = page
    .getByTestId("playlist-selector-row")
    .filter({ hasText: "ytlive" })
    .locator(".lock-badge");
  await expect(liveBadge).toHaveCount(0);
});

test("global genlock summary reports the worst live pacing-enabled output (#164)", async ({
  page,
  request,
}) => {
  // Two live pacing-enabled outputs; the worst is DEGRADED SP-background. The
  // header shows ONE summary with the state, an n/m count, and the reason — not
  // nine identical dots.
  const set = await request.post("/__mock/ndi-health", {
    data: GENLOCK_ENABLED_FIXTURE,
  });
  expect(set.ok()).toBeTruthy();

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  const global = page.locator(".genlock-status .lock-badge");
  await expect(global).toBeVisible({ timeout: 5000 });
  await expect(global).toContainText("DEGRADED");
  await expect(global).toContainText("1/2");
  await expect(global).toContainText("no receiver");
  await expect(global).toHaveClass(/lock-degraded/);
});

test("global genlock summary flips to LOCKED after an all-locked fixture (#164)", async ({
  page,
  request,
}) => {
  const start = await request.post("/__mock/ndi-health", {
    data: GENLOCK_ENABLED_FIXTURE,
  });
  expect(start.ok()).toBeTruthy();

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  const global = page.locator(".genlock-status .lock-badge");
  await expect(global).toContainText("DEGRADED", { timeout: 6000 });

  // Replace with every live output LOCKED + pacing enabled; the 1 s poll picks
  // it up and the summary flips to LOCKED n/m within a few seconds.
  const resp = await request.post("/__mock/ndi-health", {
    data: [
      {
        ndi_name: "SP-worship", playlist_id: 1, state: "Playing", connections: 2,
        lock_state: "LOCKED", lock_reason: "locked",
        clock: { is_locked: true, mode: "LOCK", offset_ns: 100, clock_ok: true },
        pacing: { enabled: true }, audio: {},
      },
      {
        ndi_name: "SP-background", playlist_id: 2, state: "Playing", connections: 3,
        lock_state: "LOCKED", lock_reason: "locked",
        clock: { is_locked: true, mode: "LOCK", offset_ns: 120, clock_ok: true },
        pacing: { enabled: true }, audio: {},
      },
    ],
  });
  expect(resp.ok()).toBeTruthy();

  await expect(global).toContainText("LOCKED", { timeout: 4000 });
  await expect(global).toContainText("2/2");
  await expect(global).toHaveClass(/lock-locked/);

  // Both live locked outputs also carry a per-row badge in the selector.
  await expect(
    page
      .getByTestId("playlist-selector-row")
      .filter({ hasText: "Worship" })
      .locator(".lock-badge"),
  ).toBeVisible();
});

test("pacing disabled everywhere shows GENLOCK OFF in the header, no per-card badge (#176)", async ({
  page,
  request,
}) => {
  // Production reality: genlock_pacing OFF on every output. #176 revises #164:
  // the header now ALWAYS shows the explicit grey '● GENLOCK OFF' so the owner
  // can tell at a glance the box is not genlocked; per-card badges stay hidden.
  const set = await request.post("/__mock/ndi-health", {
    data: GENLOCK_ALL_DISABLED_FIXTURE,
  });
  expect(set.ok()).toBeTruthy();

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Worship" })).toBeVisible({
    timeout: 10000,
  });

  const global = page.getByTestId("genlock-global-badge");
  await expect(global).toBeVisible({ timeout: 6000 });
  await expect(global).toContainText("● GENLOCK OFF");
  await expect(global).toHaveClass(/lock-off/);
  // The OFF tooltip explains the free-running state.
  await expect(global).toHaveAttribute("title", /pacing vypnuté/);

  // No per-card badge anywhere while pacing is off (the header badge is the only
  // one). Retries while the 1 s poll settles.
  await expect(
    page.locator(".playlist-selector-row .lock-badge"),
  ).toHaveCount(0, { timeout: 6000 });
});

test("a live pacing-enabled UNLOCKED output turns the header badge red UNLOCKED (#176)", async ({
  page,
  request,
}) => {
  // A single live, pacing-enabled output whose clock is not ok → the worst-of
  // global state is UNLOCKED (red), with the reason in the badge text.
  const set = await request.post("/__mock/ndi-health", {
    data: [
      {
        ndi_name: "SP-worship",
        playlist_id: 1,
        state: "Playing",
        connections: 2,
        lock_state: "UNLOCKED",
        lock_reason: "clock not ok",
        clock: { is_locked: false, mode: "", offset_ns: null, clock_ok: false },
        pacing: { enabled: true },
        audio: {},
      },
    ],
  });
  expect(set.ok()).toBeTruthy();

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  const global = page.getByTestId("genlock-global-badge");
  await expect(global).toBeVisible({ timeout: 6000 });
  await expect(global).toContainText("UNLOCKED", { timeout: 6000 });
  await expect(global).toContainText("clock not ok");
  await expect(global).toHaveClass(/lock-unlocked/);
});

// ── #152: per-song SK translation gender toggle ───────────────────────────────

test("lyrics song row gender toggle cycles auto→♂→♀ and PATCHes (#152)", async ({
  page,
}) => {
  // The translator prompt frames first-person lines as spoken by a specific
  // grandparent so gendered Slovak forms come out right; the per-song toggle
  // overrides that gender. Navigate via the Dashboard nav — a direct /lyrics
  // deep link renders no sections (the page iterates store.playlists, seeded
  // by the Dashboard's own fetch — see .claude/rules/sp-ui-frontend.md).
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  await page.getByRole("button", { name: "Lyrics", exact: true }).click();
  await expect(page).toHaveURL(/\/lyrics$/);

  const toggle = page.locator(".translation-gender-btn").first();
  await expect(toggle).toBeVisible({ timeout: 5000 });
  // Auto (no override yet) shows both glyphs.
  await expect(toggle).toHaveText("♂♀");

  // First click → masculine; a PATCH must fire with gender "m".
  const [maleReq] = await Promise.all([
    page.waitForRequest(
      (r) => r.url().includes("/translation-gender") && r.method() === "PATCH",
    ),
    toggle.click(),
  ]);
  expect(JSON.parse(maleReq.postData() || "{}").gender).toBe("m");
  await expect(toggle).toHaveText("♂");

  // Second click → feminine.
  const [femaleReq] = await Promise.all([
    page.waitForRequest(
      (r) => r.url().includes("/translation-gender") && r.method() === "PATCH",
    ),
    toggle.click(),
  ]);
  expect(JSON.parse(femaleReq.postData() || "{}").gender).toBe("f");
  await expect(toggle).toHaveText("♀");
});

// ── #163: subtitles block must not resize the card ────────────────────────────

test("karaoke panel keeps the card height stable across lyrics on/off (#163)", async ({
  page,
  request,
}) => {
  // The owner's report: "okno stále skáče hore dole" — the subtitles block under
  // the player/preview appears only when there is a lyric line, so the card (and
  // everything below it) jumps whenever lyrics pause. The panel must ALWAYS be in
  // the DOM with reserved height; only the text inside it swaps.
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Worship" })).toBeVisible({
    timeout: 10000,
  });

  const card = page.locator(".playlist-card", {
    has: page.getByRole("heading", { name: "Worship" }),
  });
  // Wait for the WS-driven now-playing block to arrive (playlist 1 is marked
  // Playing by the mock). This also proves the WebSocket is connected, so the
  // /__mock/lyrics-update broadcast below actually reaches a client.
  await expect(card.locator(".np-song")).toBeVisible({ timeout: 10000 });

  const panel = card.locator(".karaoke-panel");

  // 1) A lyric line WITH text.
  const withText = await request.post("/__mock/lyrics-update", {
    data: {
      playlist_id: 1,
      line_en: "Amazing grace how sweet",
      line_sk: "Úžasná milosť aká sladká",
      prev_line_en: "was blind but now I see",
      next_line_en: "that saved a wretch like me",
      active_word_index: 2,
      word_count: 4,
    },
  });
  expect(withText.ok()).toBeTruthy();

  await expect(panel).toBeVisible({ timeout: 5000 });
  await expect(panel.locator(".karaoke-current")).toContainText("Amazing");

  const cardBox1 = await card.boundingBox();
  const panelBox1 = await panel.boundingBox();
  expect(cardBox1?.height ?? 0).toBeGreaterThan(0);
  expect(panelBox1?.height ?? 0).toBeGreaterThan(0);

  // 2) A pause between lines — nothing to show. The panel must stay in the DOM
  // at the SAME height; only its text clears.
  const noText = await request.post("/__mock/lyrics-update", {
    data: { playlist_id: 1 },
  });
  expect(noText.ok()).toBeTruthy();

  await expect(panel).toBeVisible();
  await expect(panel.locator(".karaoke-current")).not.toContainText("Amazing");

  const cardBox2 = await card.boundingBox();
  const panelBox2 = await panel.boundingBox();

  // Equal within 1px: the block reserves its space whether or not there is a
  // lyric line, so nothing below it jumps.
  expect(
    Math.abs((panelBox1?.height ?? 0) - (panelBox2?.height ?? 0)),
  ).toBeLessThanOrEqual(1);
  expect(
    Math.abs((cardBox1?.height ?? 0) - (cardBox2?.height ?? 0)),
  ).toBeLessThanOrEqual(1);
});
