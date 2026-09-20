import { test, expect, Page } from "@playwright/test";

// #194 ROUND 3b — the ONE shared HealthBar status strip renders identically on
// EVERY page (Dashboard, Live, Lyrics, Dabing, Settings), with the SAME testids
// (`health-ws` / `health-obs` / `health-genlock` / `health-resolume` /
// `health-tools` / `health-lan` / `health-version`) and Slovak labels. Also
// proves the shared `StateBlock` renderer (`state-empty` on the empty Dabing
// list). Runs under the default `chromium` project against the local mock.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/live-reset");
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

// Every segment the shared HealthBar exposes, with the Slovak wording each
// must carry. genlock is checked via the preserved `genlock-global-badge`.
async function assertHealthBar(page: Page) {
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });

  await expect(page.getByTestId("health-ws")).toContainText("WS");
  await expect(page.getByTestId("health-obs")).toContainText("OBS:");
  // The mock seeds ObsStatus{connected, scene:"sp-alex"} over WS — the scene
  // name must flow through into the OBS segment.
  await expect(page.getByTestId("health-obs")).toContainText("pripojené");
  await expect(page.getByTestId("health-obs")).toContainText("sp-alex");

  const genlock = page.getByTestId("health-genlock");
  await expect(genlock).toBeVisible();
  await expect(genlock.getByTestId("genlock-global-badge")).toBeVisible();

  await expect(page.getByTestId("health-resolume")).toContainText("Resolume:");
  await expect(page.getByTestId("health-tools")).toContainText("Nástroje:");
  // The mock seeds ToolsStatus with everything present.
  await expect(page.getByTestId("health-tools")).toContainText("OK");
  await expect(page.getByTestId("health-lan")).toContainText("LAN:");

  const version = page.getByTestId("health-version");
  await expect(version).toBeVisible();
  await expect(version.getByTestId("version")).toContainText(/^v\d/);
}

const PAGES: Array<[string, string]> = [
  ["Dashboard", "/"],
  ["Live", "/live"],
  ["Lyrics", "/lyrics"],
  ["Dabing", "/dabing"],
  ["Settings", "/settings"],
];

for (const [name, path] of PAGES) {
  test(`HealthBar renders the same strip on ${name}`, async ({ page }) => {
    await page.goto(path);
    await expect(page.locator("text=SongPlayer")).toBeVisible({
      timeout: 10000,
    });
    await assertHealthBar(page);
  });
}

test("StateBlock renders the shared empty state on the empty Dabing list", async ({
  page,
}) => {
  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toHaveText("Dabing");
  const empty = page.getByTestId("state-empty");
  await expect(empty).toBeVisible({ timeout: 10000 });
  await expect(empty).toContainText("Zatiaľ žiadne dabingové videá");
});
