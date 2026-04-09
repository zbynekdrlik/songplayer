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
  await expect(page.locator("text=Worship")).toBeVisible({ timeout: 10000 });
  await expect(page.locator("text=Background")).toBeVisible();
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
  expect(json).toHaveLength(2);
  expect(json[0]).toHaveProperty("name", "Worship");
  expect(json[1]).toHaveProperty("name", "Background");
});

test("settings endpoint returns data", async ({ request }) => {
  const resp = await request.get("/api/v1/settings");
  expect(resp.status()).toBe(200);
  const json = await resp.json();
  expect(json).toHaveProperty("obs_websocket_url");
  expect(json).toHaveProperty("gemini_model");
});
