import { test, expect, Page } from "@playwright/test";

// #225: the shared Player never claims a state it was not told.
//
// 1. On load it shows a neutral "Načítavam…" until the WebSocket replay tells
//    it the playlist's state, and never "Nič nehrá" / "Mixér — nič nehrá" for
//    a playlist that plays, even when that playlist's first NowPlaying arrives
//    late (`/__mock/ws-replay` delays it). The server's replay sends a playing
//    playlist's song with its state (`playback/dashboard_replay.rs`).
// 2. The program badge reads the same live WS state as the state label, so a
//    program cut flips both in the same render, well inside 1 s. Before the
//    fix the badge read the 1 Hz poll of the server's 5 s health sample (the
//    mock's health row for ytlive 184 never says Playing, so it never flipped).
//
// A MutationObserver records the Player from the FIRST DOM change, so a false
// "nič nehrá" that lasted a single render is caught too, not only the state
// the test happens to poll. Zero console errors is the last assertion, per
// browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const SONG = "Never Gonna Give You Up";

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  await request.post("/__mock/ws-replay", { data: {} });
  await request.post("/__mock/program-reset");
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // The replay knobs and the program are global in-memory mock state; reset
  // them so they cannot leak into a later spec (a single worker runs the files
  // serially).
  await request.post("/__mock/ws-replay", { data: {} });
  await request.post("/__mock/program-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

type PlayerRead = {
  t: number;
  title: string | null;
  state: string | null;
  badge: string | null;
  mixerIdle: boolean;
  mixerPending: boolean;
};

// Record every distinct Player read-out from the first DOM change on.
async function recordPlayer(page: Page) {
  await page.addInitScript(() => {
    const w = window as unknown as { __player: unknown[] };
    w.__player = [];
    let last = "";
    const text = (id: string) =>
      document.querySelector(`[data-testid="${id}"]`)?.textContent?.trim() ??
      null;
    const has = (id: string) =>
      document.querySelector(`[data-testid="${id}"]`) !== null;
    const read = () => {
      const r = {
        title: text("player-title"),
        state: text("player-state"),
        badge: text("player-program-badge"),
        mixerIdle: has("player-mixer-idle"),
        mixerPending: has("player-mixer-pending"),
      };
      const key = JSON.stringify(r);
      if (key !== last) {
        last = key;
        w.__player.push({ t: performance.now(), ...r });
      }
    };
    new MutationObserver(read).observe(document, {
      subtree: true,
      childList: true,
      characterData: true,
      attributes: true,
    });
  });
}

async function recorded(page: Page): Promise<PlayerRead[]> {
  return page.evaluate(
    () => (window as unknown as { __player: PlayerRead[] }).__player,
  );
}

// Every read-out that claims nothing plays.
function claimsNothingPlays(seen: PlayerRead[]): PlayerRead[] {
  return seen.filter((r) => r.title === "Nič nehrá" || r.mixerIdle);
}

test.describe("the Player on page load (#225)", () => {
  test("a playing playlist whose first NowPlaying arrives late never reads 'nič nehrá'", async ({
    page,
    request,
  }) => {
    // The replay tells the state first and the song 3 s later (the knob
    // holds the mock's 2 s NowPlaying interval back too).
    await request.post("/__mock/ws-replay", {
      data: { now_playing_delay_ms: 3000 },
    });
    await recordPlayer(page);
    // `?playlist=1` pins the work area to playlist 1 (Worship), which plays.
    await page.goto("/?playlist=1");

    const title = page.getByTestId("player-title");
    // The state is known: it plays, on program …
    await expect(page.getByTestId("player-state")).toHaveText("Hrá", {
      timeout: 15000,
    });
    await expect(page.getByTestId("player-program-badge")).toHaveText(
      "● Na programe",
    );
    // … but its song is not, so the Player says it is loading, not that
    // nothing plays.
    await expect(title).toHaveText("Načítavam…");
    await expect(page.getByTestId("player-mixer-pending")).toHaveText(
      "Mixér — načítavam…",
    );

    // Then the song arrives.
    await expect(title).toContainText(SONG, { timeout: 10000 });
    await expect(page.getByTestId("player-mixer-idle")).toHaveCount(0);
    await expect(page.getByTestId("player-mixer-pending")).toHaveCount(0);

    const seen = await recorded(page);
    expect(
      claimsNothingPlays(seen),
      `the Player claimed nothing plays: ${JSON.stringify(seen)}`,
    ).toEqual([]);
    // The recorded read-outs hold the window whatever the polling saw: told
    // it plays on program, its song not here yet.
    expect(
      seen.some(
        (r) =>
          r.state === "Hrá" &&
          r.badge === "● Na programe" &&
          r.title === "Načítavam…" &&
          r.mixerPending,
      ),
      `no read-out of a playing playlist waiting for its song: ${JSON.stringify(seen)}`,
    ).toBe(true);
  });

  test("before the replay arrives the Player shows a neutral placeholder, then the playing song", async ({
    page,
    request,
  }) => {
    // Hold the whole replay (and the mock's 2 s song interval) back for 3 s
    // after the socket opens.
    await request.post("/__mock/ws-replay", { data: { delay_ms: 3000 } });
    await recordPlayer(page);
    await page.goto("/?playlist=1");

    // Nothing is known yet: no claim about the song, the state or the program.
    const title = page.getByTestId("player-title");
    await expect(title).toHaveText("Načítavam…", { timeout: 15000 });
    await expect(page.getByTestId("player-state")).toHaveText("—");
    await expect(page.getByTestId("player-program-badge")).toHaveText("◌ —");
    await expect(page.getByTestId("player-mixer-pending")).toBeVisible();

    // The replay: the song, its state, its program.
    await expect(title).toContainText(SONG, { timeout: 10000 });
    await expect(page.getByTestId("player-state")).toHaveText("Hrá");
    await expect(page.getByTestId("player-program-badge")).toHaveText(
      "● Na programe",
    );

    const seen = await recorded(page);
    expect(
      claimsNothingPlays(seen),
      `the Player claimed nothing plays: ${JSON.stringify(seen)}`,
    ).toEqual([]);
    // It never claimed the opposite program state or "Nehrá" either.
    const falseClaims = seen.filter(
      (r) => r.state === "Nehrá" || r.badge === "○ Mimo programu",
    );
    expect(
      falseClaims,
      `the Player claimed a state it was not told: ${JSON.stringify(seen)}`,
    ).toEqual([]);
    // The recorded read-outs hold the "nothing told yet" state whatever the
    // polling saw.
    expect(
      seen.some(
        (r) =>
          r.title === "Načítavam…" &&
          r.state === "—" &&
          r.badge === "◌ —" &&
          r.mixerPending,
      ),
      `no read-out of the Player before the replay: ${JSON.stringify(seen)}`,
    ).toBe(true);
  });
});

// The mock broadcasts `/play`'s state over the dashboard WebSocket, so a click
// must come after the socket is open: the health bar's OBS segment shows the
// mock's `ObsStatus` (sp-alex) only once the socket delivered it.
async function socketOpen(page: Page) {
  await expect(page.getByTestId("health-obs")).toContainText("sp-alex", {
    timeout: 15000,
  });
}

// From now on, record when the state label and the badge first read `label`
// and `badge` (in-page `performance.now()`).
async function watchFlip(page: Page, label: string, badge: string) {
  await page.evaluate(
    ([label, badge]) => {
      const w = window as unknown as {
        __flip: { label: number | null; badge: number | null };
      };
      const flip = { label: null as number | null, badge: null as number | null };
      w.__flip = flip;
      const text = (id: string) =>
        document.querySelector(`[data-testid="${id}"]`)?.textContent?.trim();
      const check = () => {
        const now = performance.now();
        if (flip.label === null && text("player-state") === label) flip.label = now;
        if (flip.badge === null && text("player-program-badge") === badge)
          flip.badge = now;
      };
      new MutationObserver(check).observe(document, {
        subtree: true,
        childList: true,
        characterData: true,
      });
    },
    [label, badge],
  );
}

async function flipTimes(page: Page) {
  return page.evaluate(
    () =>
      (window as unknown as { __flip: { label: number | null; badge: number | null } })
        .__flip,
  );
}

test("a program cut flips the badge together with the state label, well inside 1 s (#225)", async ({
  page,
  request,
}) => {
  await page.goto("/live");
  await socketOpen(page);
  const label = page.getByTestId("player-state");
  const badge = page.getByTestId("player-program-badge");
  const toggle = page.getByTestId("player-playpause");
  await expect(toggle).toContainText("▶ Prehrať", { timeout: 15000 });
  await expect(badge).toHaveText("○ Mimo programu");

  // ▶ on ytlive (off air): it plays off program.
  await toggle.click();
  await expect(label).toHaveText("Hrá mimo programu", { timeout: 10000 });
  await expect(badge).toHaveText("○ Mimo programu");

  // The operator cuts it to program.
  await watchFlip(page, "Hrá", "● Na programe");
  const cut = await request.post("/api/v1/program/cut", {
    data: { source: 184 },
  });
  expect(cut.ok()).toBe(true);
  await expect(badge).toHaveText("● Na programe", { timeout: 1000 });
  await expect(badge).toHaveClass(/\bon\b/);
  await expect(label).toHaveText("Hrá", { timeout: 1000 });
  let at = await flipTimes(page);
  expect(at.label, "the state label flipped").not.toBeNull();
  expect(at.badge, "the badge flipped").not.toBeNull();
  expect(
    Math.abs((at.badge as number) - (at.label as number)),
    "the badge flips with the state label, in the same render",
  ).toBeLessThan(100);

  // It leaves program (paused off air): both flip back together.
  await watchFlip(page, "Čaká na scénu", "○ Mimo programu");
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 184, state: "WaitingForScene", transport: "Paused" },
  });
  await expect(badge).toHaveText("○ Mimo programu", { timeout: 1000 });
  await expect(badge).not.toHaveClass(/\bon\b/);
  await expect(label).toHaveText("Čaká na scénu", { timeout: 1000 });
  at = await flipTimes(page);
  expect(at.label, "the state label flipped back").not.toBeNull();
  expect(at.badge, "the badge flipped back").not.toBeNull();
  expect(
    Math.abs((at.badge as number) - (at.label as number)),
    "the badge flips back with the state label, in the same render",
  ).toBeLessThan(100);
});
