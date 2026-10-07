import { test, expect, APIRequestContext } from "@playwright/test";

// #229: a playlist whose videos cannot be opened (a box with no VP9/AV1
// decoder, an offline cache disk) says so in the shared Player, from its
// `GET /api/v1/ndi/health` row's `open_failures`: "Videá sa nedajú otvoriť
// (N×): <chyba> — ďalší pokus o X s", the reason a black program has. The
// mock's health rows are set through `/__mock/ndi-health` and put back with
// `/__mock/ndi-health-reset`. Zero console errors, per
// browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

/// What Media Foundation said at PP (6.10.2026) for every cached video.
const ERROR = "No video: SetCurrentMediaType failed: No suitable transform";

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  await request.post("/__mock/ndi-health-reset");
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // Global in-memory mock state: never leak it into a later spec.
  await request.post("/__mock/ndi-health-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

/// Give `playlistId`'s health row these `open_failures` (null = none failed).
async function setOpenFailures(
  request: APIRequestContext,
  playlistId: number,
  openFailures: unknown,
) {
  const rows = await (await request.get("/api/v1/ndi/health")).json();
  const row = rows.find(
    (r: { playlist_id: number }) => r.playlist_id === playlistId,
  );
  expect(row, `the mock has a health row for playlist ${playlistId}`).toBeTruthy();
  row.open_failures = openFailures;
  const set = await request.post("/__mock/ndi-health", { data: rows });
  expect(set.ok()).toBeTruthy();
}

/// Tell the dashboards `playlistId`'s state and transport, as the engine's
/// `PlaybackStateChanged` does (`/__mock/set-playing`).
async function setState(
  request: APIRequestContext,
  playlistId: number,
  state: string,
  transport: string,
) {
  const sent = await request.post("/__mock/set-playing", {
    data: { playlist_id: playlistId, state, transport },
  });
  expect(sent.ok()).toBeTruthy();
  expect((await sent.json()).clients).toBeGreaterThan(0);
}

/// The seconds the line counts down to the next attempt (NaN without one).
function secondsLeft(text: string | null): number {
  const m = text?.match(/ďalší pokus o (\d+) s$/);
  return m ? Number(m[1]) : Number.NaN;
}

test("the dashboard Player says why the program is black and counts down to the next attempt (#229)", async ({
  page,
  request,
}) => {
  await page.goto("/?playlist=1");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
  const line = page.getByTestId("player-open-failures");
  await expect(line).toHaveCount(0);

  await setOpenFailures(request, 1, {
    count: 4,
    last_error: ERROR,
    retry_at_ms: Date.now() + 30_000,
  });

  await expect(line).toBeVisible({ timeout: 5000 });
  await expect(line).toHaveText(
    /^Videá sa nedajú otvoriť \(4×\): No video: SetCurrentMediaType failed: No suitable transform — ďalší pokus o \d+ s$/,
  );
  const first = secondsLeft(await line.textContent());
  expect(first).toBeGreaterThan(20);
  expect(first).toBeLessThanOrEqual(30);
  // The 1 Hz health poll re-renders the text: the countdown follows the
  // clock, not the first read.
  await expect
    .poll(async () => secondsLeft(await line.textContent()), { timeout: 6000 })
    .toBeLessThan(first);

  // A song started: the run is over, and the line goes.
  await setOpenFailures(request, 1, null);
  await expect(line).toHaveCount(0, { timeout: 5000 });
});

test("with no retry pending the line names the failures only (#229)", async ({
  page,
  request,
}) => {
  await page.goto("/?playlist=1");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });

  await setOpenFailures(request, 1, {
    count: 2,
    last_error: ERROR,
    retry_at_ms: null,
  });

  await expect(page.getByTestId("player-open-failures")).toHaveText(
    `Videá sa nedajú otvoriť (2×): ${ERROR}`,
    { timeout: 5000 },
  );
});

test("the Live page's Player shows the same line, and a due retry reads 0 s (#229)", async ({
  page,
  request,
}) => {
  await page.goto("/live");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });

  await setOpenFailures(request, 184, {
    count: 3,
    last_error: ERROR,
    retry_at_ms: Date.now() + 3_000,
  });

  const line = page.getByTestId("player-open-failures");
  await expect(line).toHaveText(
    /^Videá sa nedajú otvoriť \(3×\): No video: SetCurrentMediaType failed: No suitable transform — ďalší pokus o [0-3] s$/,
    { timeout: 5000 },
  );
  // While the retry waits the state label says so (not "Čaká na scénu"),
  // and so does the badge: a retry waits only on program. One Player
  // component, so the Live page reads what the dashboard does.
  await expect(page.getByTestId("player-state")).toHaveText(
    "Čaká na ďalší pokus",
  );
  await expect(page.getByTestId("player-program-badge")).toHaveText(
    "● Na programe — čaká na ďalší pokus",
  );
  // Once the retry is due the countdown stops at 0, never below.
  await expect(line).toHaveText(/ďalší pokus o 0 s$/, { timeout: 8000 });
});

// #229 follow-up (design record 6029071745): while a playlist waits out the
// retry of its failed opens, the engine reports `WaitingForScene` (nothing
// decodes), so the badge read "○ Mimo programu" for the playlist that IS
// SP-program's source, its program black. A retry waits only on program
// (`failure_retry.rs`: a cut off program ends it), so the badge says "on
// program, waiting". A playlist told it decodes keeps its badge: the retry's
// Play went out before the 1 Hz health row moved.
test("on program and waiting out the retry, the badge says so (#229)", async ({
  page,
  request,
}) => {
  await page.goto("/?playlist=1");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
  const badge = page.getByTestId("player-program-badge");
  const label = page.getByTestId("player-state");
  // The replay: playlist 1 plays on program.
  await expect(badge).toHaveText("● Na programe", { timeout: 15000 });

  // The pause after failed opens: nothing decodes, WaitingForScene.
  await setState(request, 1, "WaitingForScene", "Paused");
  await expect(label).toHaveText("Čaká na scénu");
  await expect(badge).toHaveText("○ Mimo programu");

  await setOpenFailures(request, 1, {
    count: 3,
    last_error: ERROR,
    retry_at_ms: Date.now() + 30_000,
  });
  await expect(label).toHaveText("Čaká na ďalší pokus", { timeout: 5000 });
  await expect(badge).toHaveText("● Na programe — čaká na ďalší pokus");
  await expect(badge).toHaveClass(/\bon\b/);

  // The retry's Play went out: the playlist is told it decodes on program
  // before the health row says the retry is over.
  await setState(request, 1, "Playing", "Playing");
  await expect(label).toHaveText("Hrá");
  await expect(badge).toHaveText("● Na programe");

  // Its open failed again and it waits again. Then it is cut off program:
  // the retry ends (the run is kept), and a playlist that waits with no
  // retry pending is off program.
  await setState(request, 1, "WaitingForScene", "Paused");
  await expect(badge).toHaveText("● Na programe — čaká na ďalší pokus");
  await setOpenFailures(request, 1, {
    count: 4,
    last_error: ERROR,
    retry_at_ms: null,
  });
  await expect(badge).toHaveText("○ Mimo programu", { timeout: 5000 });
  await expect(badge).not.toHaveClass(/\bon\b/);
  await expect(label).toHaveText("Čaká na scénu");
});
