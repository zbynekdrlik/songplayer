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
