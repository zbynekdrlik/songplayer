import { test, expect } from "@playwright/test";

// #225 (release 0.70.0 review): a mode change the server REFUSES must not
// stay on the select. The Player claims only what it was told (#225): after a
// refused `PUT /api/v1/playback/{id}/mode` the toast says so AND the select
// reads the mode the playlist still plays, never the operator's unsaved pick.
//
// The mock's Dabing playlist is id 500, driven by the mock tick so its state
// is known. The mock's `fail-mode` knob makes the mode route answer 500.
// Zero console errors, per browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
  // The refused PUT itself: Chrome logs the failed request's 500.
  /Failed to load resource.*500/,
];

const DABING_PID = 500;

let consoleMessages: string[] = [];

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // Global in-memory mock state: never leak it into a later spec.
  await request.post("/__mock/fail-mode", { data: { kind: "mode", enabled: false } });
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("a refused mode change puts the select back on the mode that plays (#225)", async ({
  page,
  request,
}) => {
  await page.goto("/dabing");
  await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: DABING_PID,
          video_id: 900,
          song: "Kázeň",
          artist: "Dabing",
          duration_ms: 200000,
          position_ms: 1000,
          state: "WaitingForScene",
          transport: "Playing",
        },
      ],
    },
  });

  const select = page.getByTestId("player-mode");
  await expect(select).toBeEnabled({ timeout: 15000 });
  await expect(select).toHaveValue("continuous");

  await request.post("/__mock/fail-mode", { data: { kind: "mode", enabled: true } });
  await select.selectOption("single");

  await expect(page.getByTestId("player-error")).toContainText(
    "Zmena režimu zlyhala",
    { timeout: 10000 },
  );
  // Nothing told the Player the mode changed: it still plays Continuous.
  await expect(select).toHaveValue("continuous", { timeout: 5000 });
});
