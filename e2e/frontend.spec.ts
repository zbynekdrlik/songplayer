import { test, expect, Page } from "@playwright/test";

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
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

test("dashboard shows playlist cards", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Worship" })).toBeVisible({ timeout: 10000 });
  await expect(page.getByRole("heading", { name: "Background" })).toBeVisible();
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

// ── #150: LIVE-LOCKED genlock indicator ──────────────────────────────────────

test("NDI lock badges render each output state with its reason (#150)", async ({
  page,
}) => {
  // Default fixture (e2e/mock-api.mjs): SP-worship LOCKED, SP-background
  // DEGRADED "no receiver", SP-live UNLOCKED "pacing disabled". Each
  // playlist card shows a `.lock-badge` next to its NDI output name.
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Worship" })).toBeVisible({
    timeout: 10000,
  });

  const worshipBadge = page
    .locator(".playlist-card", { hasText: "Worship" })
    .locator(".lock-badge");
  await expect(worshipBadge).toBeVisible({ timeout: 5000 });
  await expect(worshipBadge).toHaveText("● LOCKED");
  await expect(worshipBadge).toHaveClass(/lock-locked/);

  const bgBadge = page
    .locator(".playlist-card", { hasText: "Background" })
    .locator(".lock-badge");
  await expect(bgBadge).toContainText("DEGRADED");
  await expect(bgBadge).toContainText("no receiver");
  await expect(bgBadge).toHaveClass(/lock-degraded/);

  const liveBadge = page
    .locator(".playlist-card", { hasText: "ytlive" })
    .locator(".lock-badge");
  await expect(liveBadge).toContainText("UNLOCKED");
  await expect(liveBadge).toContainText("pacing disabled");
  await expect(liveBadge).toHaveClass(/lock-unlocked/);
});

test("global genlock badge names the worst live output for the default fixture (#150)", async ({
  page,
}) => {
  // Global summary counts only LIVE (state==="Playing") outputs. In the
  // default fixture SP-live is Idle, so the worst LIVE output is the
  // DEGRADED SP-background — the header badge must say so.
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  const global = page.locator(".genlock-status .lock-badge");
  await expect(global).toBeVisible({ timeout: 5000 });
  await expect(global).toContainText("DEGRADED");
  await expect(global).toContainText("SP-background");
  await expect(global).toHaveClass(/lock-degraded/);
});

test("global genlock badge flips to LOCKED after an all-locked fixture (#150)", async ({
  page,
  request,
}) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Playlists" })).toBeVisible({
    timeout: 10000,
  });

  const global = page.locator(".genlock-status .lock-badge");
  await expect(global).toContainText("DEGRADED", { timeout: 5000 });

  // Replace the fixture with every output LOCKED + live; the 1 s poll picks
  // it up and the global badge flips within a few seconds.
  const resp = await request.post("/__mock/ndi-health", {
    data: [
      {
        ndi_name: "SP-worship",
        playlist_id: 1,
        state: "Playing",
        connections: 2,
        lock_state: "LOCKED",
        lock_reason: "locked",
        clock: { is_locked: true, mode: "LOCK", offset_ns: 100, clock_ok: true },
        pacing: { enabled: true },
        audio: {},
      },
      {
        ndi_name: "SP-background",
        playlist_id: 2,
        state: "Playing",
        connections: 3,
        lock_state: "LOCKED",
        lock_reason: "locked",
        clock: { is_locked: true, mode: "LOCK", offset_ns: 120, clock_ok: true },
        pacing: { enabled: true },
        audio: {},
      },
    ],
  });
  expect(resp.ok()).toBeTruthy();

  await expect(global).toHaveText("● LOCKED", { timeout: 3500 });
  await expect(global).toHaveClass(/lock-locked/);
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
