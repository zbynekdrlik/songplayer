import { test, expect } from "@playwright/test";

// #262 — the Resolume segment of the shared HealthBar. Only a host that does
// not answer reads "Resolume: neodpovedá"; a SongPlayer token the operator's
// composition has no clip for (PP shows no English line) is a note in the
// tooltip, never an error. The bar polls `GET /api/v1/resolume/health` every
// 5 s, so a change shows up without a reload. Runs against the mock.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

/** PP's live row of 13.10.2026: Arena answers, no English-line clips. */
const PP_ROW = {
  host: "127.0.0.1",
  last_refresh_ts: "2026-10-13T12:47:20.953732800Z",
  last_refresh_ok: true,
  consecutive_failures: 0,
  circuit_breaker_open: false,
  product_latency_ms: 0,
  last_full_refresh_ts: "2026-10-13T12:42:49.731260600Z",
  clips_by_token: { "#sp-subs": 0, "#sp-subs-next": 0, "#sp-subssk": 3, "#sp-title": 4 },
};

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/resolume-health", { data: {} });
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/resolume-health", { data: {} });
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

test("a composition without some SongPlayer clips reads OK, with a note — only a host that does not answer is amber", async ({
  page,
  request,
}) => {
  // Three changes, each seen at the bar's next 5 s poll.
  test.setTimeout(90_000);
  await request.post("/__mock/resolume-health", { data: { hosts: [PP_ROW] } });
  await page.goto("/");
  const segment = page.getByTestId("health-resolume");
  await expect(segment).toHaveText("Resolume: OK", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-ok/);
  await expect(segment).toHaveAttribute(
    "title",
    "127.0.0.1: v kompozícii nie sú klipy #sp-subs, #sp-subs-next (nastavenie Resolume, nie chyba)",
  );

  // Arena stops answering: amber, the reason in the tooltip.
  await request.post("/__mock/resolume-health", {
    data: { hosts: [{ ...PP_ROW, last_refresh_ok: false, consecutive_failures: 3 }] },
  });
  await expect(segment).toHaveText("Resolume: neodpovedá", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-warn/);
  await expect(segment).toHaveAttribute("title", "127.0.0.1: obnova zlyháva (3 po sebe)");

  // A composition with no SongPlayer clip at all: grey, never amber.
  await request.post("/__mock/resolume-health", {
    data: {
      hosts: [
        {
          ...PP_ROW,
          clips_by_token: { "#sp-subs": 0, "#sp-subs-next": 0, "#sp-subssk": 0, "#sp-title": 0 },
        },
      ],
    },
  });
  await expect(segment).toHaveText("Resolume: bez klipov SongPlayera", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-off/);
  await expect(segment).toHaveAttribute(
    "title",
    "127.0.0.1: v kompozícii nie je žiadny klip SongPlayera",
  );

  // No host configured: the grey dash.
  await request.post("/__mock/resolume-health", { data: {} });
  await expect(segment).toHaveText("Resolume: —", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-off/);
});
