import { test, expect } from "@playwright/test";

// #221 L4b: SongPlayer's own program (SP-program, plus what it told cg OBS)
// is the playback authority, so a manual ▶ never claims program. A playlist
// that is NOT on air plays OFF program: the server reports the scene-aware
// state `WaitingForScene` with transport `Playing`, and the Player's state
// label reads "Hrá mimo programu" (not "Čaká na scénu"). A playlist ON air
// plays on program and reads "Hrá". The mock models it: its `/play` broadcasts
// the state from its program source (`POST /__mock/program-reset` → playlist
// 1 on SP-program; ytlive 184 is off air). Zero console errors, per
// browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  await request.post("/__mock/program-reset");
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/program-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("▶ on a playlist off air plays it off program — 'Hrá mimo programu'", async ({
  page,
}) => {
  await page.goto("/live");
  const toggle = page.getByTestId("player-playpause");
  await expect(toggle).toContainText("▶ Prehrať", { timeout: 15000 });
  await expect(page.getByTestId("player-program-badge")).toContainText(
    "○ Mimo programu",
  );

  const posted = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/playback/184/play") && req.method() === "POST",
  );
  await toggle.click();
  await posted;

  await expect(page.getByTestId("player-state")).toHaveText("Hrá mimo programu", {
    timeout: 10000,
  });
  await expect(toggle).toContainText("⏸ Pauza");
  await expect(page.getByTestId("player-program-badge")).toContainText(
    "○ Mimo programu",
  );
});

test("▶ on the playlist on air plays it on program — 'Hrá'", async ({
  page,
  request,
}) => {
  await page.goto("/");
  const toggle = page.getByTestId("player-playpause");
  await expect(toggle).toBeVisible({ timeout: 15000 });
  // Playlist 1 (on SP-program) paused: the toggle offers ▶ and the label
  // reads "Čaká na scénu" (paused, not playing anywhere).
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 1, state: "WaitingForScene", transport: "Paused" },
  });
  await expect(toggle).toContainText("▶ Prehrať", { timeout: 10000 });
  await expect(page.getByTestId("player-state")).toHaveText("Čaká na scénu");

  const posted = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/playback/1/play") && req.method() === "POST",
  );
  await toggle.click();
  await posted;

  await expect(page.getByTestId("player-state")).toHaveText("Hrá", {
    timeout: 10000,
  });
  await expect(toggle).toContainText("⏸ Pauza");
});
