import { test, expect, APIRequestContext } from "@playwright/test";

// #201: the shared Player's play/pause label follows the pipeline's own
// TRANSPORT state, NOT the on/off-program state. A dub prepared OFF program on
// the Dabing page decodes while its scene is off OBS program (the scene-aware
// WS `state` is `WaitingForScene`) — the toggle must read `⏸ Pauza` (transport
// Playing) while it plays, and show off-program ONLY in the badge
// (`○ Mimo programu`). A genuinely paused off-program pipeline (transport
// Paused) reads `▶ Prehrať`. Driven with the mock tick's `transport` field.
//
// The mock's Dabing playlist is id 500; it is NOT in the ndiHealth fixture, so
// `on_program` is false → the badge reads `○ Mimo programu`. Zero console
// errors, per browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const DABING_PID = 500;

let consoleMessages: string[] = [];

// Drive the (off-program) Dabing output with the mock tick: `state` is the
// scene-aware WS state (WaitingForScene = off program), `transport` is the
// pipeline's own decoding state that the Player label must follow.
async function drive(
  request: APIRequestContext,
  transport: "Playing" | "Paused",
) {
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
          transport,
        },
      ],
    },
  });
}

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // Tick mode is global in-memory mock state — turn it OFF so it can't leak
  // into a later spec (a single worker runs files serially).
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test.describe("the Player label follows transport, not program (#201)", () => {
  test("an off-program DECODING dub reads ⏸ Pauza + ○ Mimo programu", async ({
    page,
    request,
  }) => {
    await page.goto("/dabing");
    const toggle = page.getByTestId("player-playpause");
    await expect(toggle).toBeVisible({ timeout: 15000 });

    // WaitingForScene (off program) + transport Playing = a dub decoding off
    // program while it is prepared for a cut-in.
    await drive(request, "Playing");

    await expect(toggle).toContainText("⏸ Pauza", { timeout: 10000 });
    await expect(page.getByTestId("player-program-badge")).toContainText(
      "○ Mimo programu",
    );
  });

  test("an off-program PAUSED dub reads ▶ Prehrať", async ({
    page,
    request,
  }) => {
    await page.goto("/dabing");
    const toggle = page.getByTestId("player-playpause");
    await expect(toggle).toBeVisible({ timeout: 15000 });

    // WaitingForScene (off program) + transport Paused = a genuinely paused
    // pipeline → the toggle offers Play.
    await drive(request, "Paused");

    await expect(toggle).toContainText("▶ Prehrať", { timeout: 10000 });
    await expect(page.getByTestId("player-program-badge")).toContainText(
      "○ Mimo programu",
    );
  });
});
