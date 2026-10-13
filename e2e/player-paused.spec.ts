import { test, expect, APIRequestContext, Page } from "@playwright/test";

// #266: the Player says the truth about a PAUSED playlist. The owner at PP
// (13.10.2026): "to ze sa nespusti lebo nie je v programe je uplne bez
// vysvetlenia!!!" — ytfast's card, paused off program while ytslow was on
// the wall, read "Čaká na scénu" and offered "▶ Živý náhľad", which could
// only stay black.
//
// 3. Its state line reads "Pozastavené (mimo programu)"
//    (`sp_core::player_view::state_label`).
// 4. Its preview is offered only while the pipeline DECODES (transport
//    `Playing`, the off-program dub included): a paused pipeline gives the
//    encoder no picture, so a preview started there stayed black. The slot
//    says why instead (`sp_core::player_view::preview_slot`); a preview the
//    operator started while it played stays mounted through a pause
//    (`preview_view`, the #184 frozen-picture owner path).
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

test.beforeEach(async ({ page, request }) => {
  // Worship (1) on SP-program; a `/play` of another playlist plays it off
  // program (the mock models #221 L4b from its program source).
  await request.post("/__mock/program-reset");
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // The tick and the program are global in-memory mock state: never leak them.
  await request.post("/__mock/tick", { data: { enabled: false } });
  await request.post("/__mock/program-reset");
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

const PREVIEW_PAUSED = "Náhľad sa zobrazí, keď playlist hrá — tento je pozastavený.";
const PREVIEW_IDLE = "Náhľad sa zobrazí, keď playlist hrá — tento nehrá.";

test("a paused playlist off program offers no live preview — the slot says why (#266)", async ({
  page,
  request,
}) => {
  await background(request, "Paused");
  await openBackground(page);
  await expect(page.getByTestId("player-state")).toHaveText("Pozastavené (mimo programu)");

  const card = page.locator(".playlist-card");
  await expect(card.getByTestId("preview-placeholder")).toHaveText(PREVIEW_PAUSED);
  await expect(card.getByTestId("preview-start")).toHaveCount(0);
  await expect(card.getByTestId("preview-video")).toHaveCount(0);

  // The operator presses ▶ Prehrať: once its pipeline decodes (off program,
  // the mock's `/play` for a playlist not on air) the preview is offered.
  await page.getByTestId("player-playpause").click();
  await expect(page.getByTestId("player-state")).toHaveText("Hrá mimo programu", {
    timeout: 10000,
  });
  await expect(card.getByTestId("preview-start")).toBeVisible();
});

test("a playlist decoding off program offers the live preview (#266)", async ({
  page,
  request,
}) => {
  await background(request, "Playing");
  await openBackground(page);
  await expect(page.getByTestId("player-state")).toHaveText("Hrá mimo programu");

  const card = page.locator(".playlist-card");
  await card.getByTestId("preview-start").click();
  await expect(card.getByTestId("preview-video")).toBeVisible({ timeout: 10000 });
});

test("an idle playlist's slot says it plays nothing (#266)", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 15000,
  });
  // Background is idle in the mock's replay (a song row, state Idle).
  await page.getByTestId("playlist-picker-item").filter({ hasText: "Background" }).click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Background");
  await expect(page.getByTestId("player-state")).toHaveText("Nehrá", { timeout: 10000 });

  const card = page.locator(".playlist-card");
  await expect(card.getByTestId("preview-placeholder")).toHaveText(PREVIEW_IDLE);
  await expect(card.getByTestId("preview-start")).toHaveCount(0);
});

test("a preview started while the song plays holds through a pause and plays on at the resume (#266)", async ({
  page,
  request,
}) => {
  // Worship plays on program (the replay); `?playlist=1` opens its card.
  await page.goto("/?playlist=1");
  const card = page.locator(".playlist-card");
  await expect(page.getByTestId("player-state")).toHaveText("Hrá", { timeout: 15000 });
  await card.getByTestId("preview-start").click();
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });
  const element = await video.elementHandle();

  // Paused (the Player's ⏸ Pauza, as the #184 owner path does on the box):
  // the SAME element stays — the encoder starves and the picture holds — and
  // no second start control appears.
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 1, state: "WaitingForScene", transport: "Paused" },
  });
  await expect(page.getByTestId("player-state")).toHaveText("Pozastavené (mimo programu)", {
    timeout: 10000,
  });
  await expect(card.getByTestId("preview-start")).toHaveCount(0);
  expect(await element!.evaluate((el) => el.isConnected), "kept through the pause").toBe(true);

  // Resumed: it plays on in the same element.
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 1, state: "Playing", transport: "Playing" },
  });
  await expect(page.getByTestId("player-state")).toHaveText("Hrá", { timeout: 10000 });
  expect(await element!.evaluate((el) => el.isConnected), "kept at the resume").toBe(true);
});
