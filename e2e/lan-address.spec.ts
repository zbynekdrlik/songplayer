import { test, expect } from "@playwright/test";

// #51: the dashboard must show the LAN `sp.local` URL (offline-LAN
// reachability) plus the raw IP fallback, sourced from /api/v1/status's
// lan_url / lan_ip fields. Runs against the local mock-api (see
// playwright.config.ts baseURL + e2e/mock-api.mjs's /api/v1/status).

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

test("dashboard shows the LAN sp.local URL with the raw IP fallback", async ({
  page,
}) => {
  await page.goto("/");
  // Wait for the app to mount.
  await expect(page.locator("text=SongPlayer")).toBeVisible({ timeout: 10000 });

  const lan = page.getByTestId("lan-address");
  await expect(lan).toBeVisible({ timeout: 10000 });

  // Primary offline-LAN URL is a clickable link to the mDNS hostname.
  const link = lan.getByRole("link", { name: "http://sp.local:8920" });
  await expect(link).toBeVisible();
  await expect(link).toHaveAttribute("href", "http://sp.local:8920");

  // Raw IP is shown as a fallback.
  await expect(lan).toContainText("10.77.9.201");
});

test("status endpoint exposes lan_url and lan_ip", async ({ request }) => {
  const resp = await request.get("/api/v1/status");
  expect(resp.status()).toBe(200);
  const json = await resp.json();
  expect(json.lan_url).toBe("http://sp.local:8920");
  expect(json.lan_ip).toBe("10.77.9.201");
});
