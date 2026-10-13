import { test, expect, Page } from "@playwright/test";

// #266: the dashboard says in words what plays. The owner at PP (13.10.2026):
// "to ktory playlist hraje je uplne nezrozumitelne" — and the list's ▶ / ⏸
// glyphs read as BUTTONS (▶ = click to play, ⏸ = click to pause), the
// opposite of what they meant there. Rule (ROZHODNUTÉ 13.10.2026): no play /
// pause glyph as a STATE marker; a state is said in words.
//
// 1. Every row of the playlist list (and the mobile `<select>` option) carries
//    its word: "NA PROGRAME" (red, the PROGRAM row's tally) for SP-program's
//    playlist, "hrá mimo programu" for one decoding off program,
//    "pozastavené" for a paused one, "nehrá" for an idle one
//    (`sp_core::player_view::playlist_status`).
// 2. A page load (a reload too) opens the work area on the playlist ON
//    PROGRAM, not on the persisted pick (URL `?playlist=` / localStorage):
//    the owner found ytfast's card paused in the work area while ytslow was
//    on the wall. The operator's click still holds for the session; with
//    nothing on program the persisted pick stays.
//
// The mock's 12-playlist fixture: playlist 1 ("Playlist 01") plays on
// program (the replay); a paused playlist off program is a `/__mock/tick`
// item `{state: "WaitingForScene", transport: "Paused", step_ms: 0}`, one
// decoding off program `{state: "WaitingForScene", transport: "Playing"}` —
// both are replayed to a new socket, so a tick enabled before `page.goto`
// is what the page is told first. Zero console errors, per
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
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  const set = await request.post("/__mock/fixture", { data: { mode: "twelve" } });
  expect(set.ok()).toBeTruthy();
});

test.afterEach(async ({ request }) => {
  // Global in-memory mock state: never leak it into a later spec.
  await request.post("/__mock/tick", { data: { enabled: false } });
  await request.post("/__mock/ws-replay", { data: {} });
  await request.post("/__mock/fixture", { data: { mode: "default" } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

/** Playlist 05 paused off program with its song loaded, Playlist 07 decoding
 *  off program (a ▶ off air): replayed to every new socket. */
async function pausedAndOffProgram(request: import("@playwright/test").APIRequestContext) {
  const tick = await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: 5,
          video_id: 1,
          song: "Paused Song",
          artist: "Paused Artist",
          position_ms: 61000,
          step_ms: 0,
          state: "WaitingForScene",
          transport: "Paused",
        },
        {
          playlist_id: 7,
          video_id: 2,
          song: "Off Program Song",
          artist: "Off Artist",
          state: "WaitingForScene",
          transport: "Playing",
        },
      ],
    },
  });
  expect(tick.ok()).toBeTruthy();
}

function row(page: Page, name: string) {
  return page.getByTestId("playlist-picker-item").filter({ hasText: name });
}

test("every row says its playlist's state in words, never a play / pause glyph (#266)", async ({
  page,
  request,
}) => {
  await pausedAndOffProgram(request);
  await page.goto("/");
  await expect(page.getByTestId("playlist-picker-item")).toHaveCount(12, {
    timeout: 15000,
  });

  const words: Array<[string, string, string]> = [
    ["Playlist 01", "NA PROGRAME", "on_program"],
    ["Playlist 05", "pozastavené", "paused"],
    ["Playlist 07", "hrá mimo programu", "off_program"],
    ["Playlist 09", "nehrá", "idle"],
  ];
  for (const [name, word, code] of words) {
    const status = row(page, name).getByTestId("playlist-picker-status");
    await expect(status, `${name}'s word`).toHaveText(word, { timeout: 10000 });
    await expect(status).toHaveAttribute("data-status", code);
  }

  // The on-program word is the PROGRAM row's red tally: the same colour as
  // the dashboard's `program-cut-on` button.
  const onProgram = row(page, "Playlist 01").getByTestId("playlist-picker-status");
  const tally = await onProgram.evaluate((el) => getComputedStyle(el).backgroundColor);
  const programRed = await page.evaluate(() => {
    const probe = document.createElement("button");
    probe.className = "program-cut program-cut-on";
    document.body.appendChild(probe);
    const c = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return c;
  });
  expect(tally, "NA PROGRAME is the PROGRAM row's red").toBe(programRed);

  // No row carries a play / pause glyph (▶ / ⏸) or the old idle dash (–).
  const texts = await page
    .getByTestId("playlist-picker-item")
    .evaluateAll((els) => els.map((e) => e.textContent ?? ""));
  for (const t of texts) {
    expect(t, "a row's text").not.toMatch(/[▶⏸–]/);
  }

  // A live change re-words the row in place (no reload): Playlist 09 starts
  // decoding off program.
  const before = await row(page, "Playlist 09").elementHandle();
  const set = await request.post("/__mock/set-playing", {
    data: { playlist_id: 9, state: "WaitingForScene", transport: "Playing" },
  });
  expect(set.ok()).toBeTruthy();
  await expect(row(page, "Playlist 09").getByTestId("playlist-picker-status")).toHaveText(
    "hrá mimo programu",
    { timeout: 10000 },
  );
  expect(await before!.evaluate((el) => el.isConnected), "the row is kept").toBe(true);
});

test("the mobile <select> carries the same words (#266)", async ({ page, request }) => {
  await pausedAndOffProgram(request);
  await page.setViewportSize({ width: 400, height: 900 });
  await page.goto("/");
  const select = page.getByTestId("playlist-picker-select");
  await expect(select).toBeVisible({ timeout: 15000 });

  const options: Array<[string, string]> = [
    ["1", "Playlist 01 (SP-01) — NA PROGRAME"],
    ["5", "Playlist 05 (SP-05) — pozastavené"],
    ["7", "Playlist 07 (SP-07) — hrá mimo programu"],
    ["9", "Playlist 09 (SP-09) — nehrá"],
  ];
  for (const [value, text] of options) {
    await expect(select.locator(`option[value="${value}"]`)).toHaveText(text, {
      timeout: 10000,
    });
  }
  const texts = await select
    .locator("option")
    .evaluateAll((els) => els.map((e) => e.textContent ?? ""));
  expect(texts).toHaveLength(12);
  for (const t of texts) {
    expect(t, "an option's text").not.toMatch(/[▶⏸–]/);
  }
});

test("a page load opens on the playlist on program, not on the persisted pick of a paused one (#266)", async ({
  page,
  request,
}) => {
  await pausedAndOffProgram(request);
  const workspace = page.getByTestId("workspace-title");

  // The owner's case: the URL (and localStorage) still name the paused
  // Playlist 05 from an earlier session.
  await page.goto("/?playlist=5");
  await expect(workspace).toHaveText("Playlist 01", { timeout: 15000 });
  await expect(row(page, "Playlist 01")).toHaveClass(/selected/);
  await expect(row(page, "Playlist 05")).not.toHaveClass(/selected/);

  // The operator opens the paused card: the click holds...
  await row(page, "Playlist 05").click();
  await expect(workspace).toHaveText("Playlist 05");
  await expect(page).toHaveURL(/[?&]playlist=5\b/);

  // ...for this session only: a reload is a page load like any other and
  // opens on the playlist on program again.
  await page.reload();
  await expect(workspace).toHaveText("Playlist 01", { timeout: 15000 });
  await expect(row(page, "Playlist 01")).toHaveClass(/selected/);
});

test("the operator's click holds for the session: the work area does not follow a program change (#266)", async ({
  page,
  request,
}) => {
  await page.goto("/");
  const workspace = page.getByTestId("workspace-title");
  await expect(workspace).toHaveText("Playlist 01", { timeout: 15000 });

  await row(page, "Playlist 05").click();
  await expect(workspace).toHaveText("Playlist 05");

  // The program moves to Playlist 09: its row says so, the work area stays
  // on the operator's pick.
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 1, state: "WaitingForScene", transport: "Paused" },
  });
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 9, state: "Playing", transport: "Playing" },
  });
  await expect(row(page, "Playlist 09").getByTestId("playlist-picker-status")).toHaveText(
    "NA PROGRAME",
    { timeout: 10000 },
  );
  await expect(row(page, "Playlist 01").getByTestId("playlist-picker-status")).toHaveText(
    "pozastavené",
  );
  await expect(workspace).toHaveText("Playlist 05");
});

test("with nothing on program a load keeps the persisted pick, then follows the playlist that comes on program (#266)", async ({
  page,
  request,
}) => {
  // Nothing on program: playlist 1 waits paused with no song.
  await request.post("/__mock/ws-replay", {
    data: {
      no_song: [{ playlist_id: 1, state: "WaitingForScene", transport: "Paused" }],
    },
  });
  await page.goto("/?playlist=9");
  const workspace = page.getByTestId("workspace-title");
  await expect(row(page, "Playlist 01").getByTestId("playlist-picker-status")).toHaveText(
    "pozastavené",
    { timeout: 15000 },
  );
  // Every state is known and none is on program: the persisted pick stays
  // (never the first by name).
  await expect(workspace).toHaveText("Playlist 09");

  // Nobody clicked: a playlist that comes on program takes the work area.
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 7, state: "Playing", transport: "Playing" },
  });
  await expect(workspace).toHaveText("Playlist 07", { timeout: 10000 });
  await expect(row(page, "Playlist 07")).toHaveClass(/selected/);
});
