import { test, expect, APIRequestContext, Page } from "@playwright/test";

// #266: the Player says the truth about a PAUSED playlist. The owner at PP
// (13.10.2026): "to ze sa nespusti lebo nie je v programe je uplne bez
// vysvetlenia!!!" — ytfast's card, paused off program while ytslow was on
// the wall, read "Čaká na scénu" and offered "▶ Živý náhľad", which could
// only stay black.
//
// 3. Its state line reads "Pozastavené (mimo programu)"
//    (`sp_core::player_view::state_label`).
//
// The mock: Worship (1) plays on program (the replay); Background (2) is a
// `/__mock/tick` item `{state: "WaitingForScene", transport: "Paused",
// step_ms: 0}` — a paused pipeline off program with its song loaded,
// replayed to every new socket. Zero console errors, per
// browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const PAUSED_SONG = "Paused Song";

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
  // The tick is global in-memory mock state: never leak it.
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

/** Background (2) with its song loaded, at `transport` off program. */
async function background(request: APIRequestContext, transport: "Paused" | "Playing") {
  const tick = await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: 2,
          video_id: 2,
          song: PAUSED_SONG,
          artist: "Paused Artist",
          duration_ms: 180000,
          position_ms: 61000,
          step_ms: transport === "Paused" ? 0 : 500,
          state: "WaitingForScene",
          transport,
        },
      ],
    },
  });
  expect(tick.ok()).toBeTruthy();
}

/** Open Background's card in the work area (the operator's click). */
async function openBackground(page: Page) {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 15000,
  });
  await page.getByTestId("playlist-picker-item").filter({ hasText: "Background" }).click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Background");
  await expect(page.getByTestId("player-title")).toContainText(PAUSED_SONG, {
    timeout: 10000,
  });
}

test("a paused playlist off program reads 'Pozastavené (mimo programu)', never 'Čaká na scénu' (#266)", async ({
  page,
  request,
}) => {
  await background(request, "Paused");
  await openBackground(page);

  await expect(page.getByTestId("player-state")).toHaveText("Pozastavené (mimo programu)");
  await expect(page.getByTestId("player-program-badge")).toHaveText("○ Mimo programu");
  // The button offers the action (▶ = play it), the state line says the state.
  await expect(page.getByTestId("player-playpause")).toHaveText("▶ Prehrať");
  await expect(page.getByTestId("player")).not.toContainText("Čaká na scénu");
});
