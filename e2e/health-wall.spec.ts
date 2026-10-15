import { test, expect } from "@playwright/test";

// #259 — the LED wall report on the shared HealthBar: `Stena: …`, green
// while Resolume draws on every display its own saved output wants, grey
// while the screens are off (or Resolume does not run, or keeps no saved
// output), red while a display
// Resolume wants is missing or it does not draw on it, with the rule, what
// Resolume wants and every problem as the tooltip. Nothing while Resolume
// drives no physical display (the mock's default: its simple output). The
// bar polls `GET /api/v1/wall` every 5 s, so a change shows up without a
// reload. #254: after the guard ended a crashed or hung Resolume, the
// segment names that end, red, until Resolume runs again. Runs against the
// mock.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

const RULE =
  "SongPlayer porovnáva obrazovky Windows s výstupom, ktorý má Resolume Arena uložený, " +
  "a len hlási — nič nenastavuje ani nemení. Jediná výnimka: spadnutý alebo zamrznutý " +
  "Resolume ukončí, aby sa mohol znova spustiť.";

const SPAN_KEY =
  "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\\0000";

/** What SNV's Resolume wants, as the server reads it. */
const READ = {
  arena_running: true,
  arena_windows: [],
  arena_output: {
    advanced: true,
    preset: "snv surround 7680",
    preset_found: true,
    displays: [
      { name: "Display 1", device_id: SPAN_KEY, width: 7680, height: 1080, fullscreen: true },
    ],
  },
  displays: [],
  nvidia: "not_read",
  connected: null,
  grids: null,
};

const SCREENS_OFF = {
  code: "screens_off",
  short_sk: "obrazovky vypnuté",
  text_sk:
    "Resolume chce obrazovku 7680×1080 (Display 1); Windows ju nemá a žiadna obrazovka " +
    "karty NVIDIA nie je pripojená — obrazovky sú vypnuté",
};
const DISPLAY_SIZE = {
  code: "display_size",
  short_sk: "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju má 2560×1080",
  text_sk:
    "Resolume chce obrazovku 7680×1080 (Display 1), Windows ju má 2560×1080 — " +
    "obrazovky sú pripojené, ale nie spojené (Surround)",
};
const NOT_DRAWING = {
  code: "arena_not_drawing",
  short_sk: "Resolume nekreslí na Display 1",
  text_sk:
    "Resolume Arena nemá na Display 1 (3840,0 – 11520,1080) viditeľné výstupné okno „Display“",
};

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/wall-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/wall-reset");
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

test("the wall segment follows the report: green, grey, red, and gone with no display output", async ({
  page,
  request,
}) => {
  // Four changes, each seen at the bar's next 5 s poll.
  test.setTimeout(90_000);
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  await expect(bar).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("health-version")).toBeVisible();
  // Resolume drives no physical display: the poll landed (the marker), and
  // there is no segment.
  await expect(bar).toHaveAttribute("data-wall", "simple_output", { timeout: 15000 });
  await expect(page.getByTestId("health-wall")).toHaveCount(0);

  await request.post("/__mock/wall", { data: { state: "ok", read: READ } });
  const segment = page.getByTestId("health-wall");
  await expect(segment).toHaveText("Stena: OK", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-ok/);
  await expect(segment).toHaveAttribute("data-state", "ok");
  await expect(bar).toHaveAttribute("data-wall", "ok");
  await expect(segment).toHaveAttribute(
    "title",
    `${RULE} Resolume (preset „snv surround 7680“) chce: Display 1 7680×1080.`,
  );

  // The screens switched off at night: calm grey, never red.
  await request.post("/__mock/wall", {
    data: { state: "screens_off", problems: [SCREENS_OFF], read: READ },
  });
  await expect(segment).toHaveText("Stena: obrazovky vypnuté", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-off/);
  await expect(segment).toHaveAttribute("data-state", "screens_off");

  // Back on, but the span did not form again: red, naming what Resolume
  // wants and what Windows has.
  await request.post("/__mock/wall", {
    data: { state: "display_missing", problems: [DISPLAY_SIZE], read: READ },
  });
  await expect(segment).toHaveText(`Stena: ${DISPLAY_SIZE.short_sk}`, { timeout: 15000 });
  await expect(segment).toHaveClass(/health-error/);
  await expect(segment).toHaveAttribute("data-state", "display_missing");
  await expect(segment).toHaveAttribute(
    "title",
    `${RULE} Resolume (preset „snv surround 7680“) chce: Display 1 7680×1080. ` +
      `Zistené: ${DISPLAY_SIZE.text_sk}.`,
  );

  // Resolume back on its simple output: the segment goes.
  await request.post("/__mock/wall-reset");
  await expect(bar).toHaveAttribute("data-wall", "simple_output", { timeout: 15000 });
  await expect(page.getByTestId("health-wall")).toHaveCount(0);
});

test("the wall segment shows on every page's health bar", async ({ page, request }) => {
  await request.post("/__mock/wall", {
    data: { state: "arena_not_drawing", problems: [NOT_DRAWING], read: READ },
  });
  for (const path of ["/", "/settings", "/lyrics"]) {
    await page.goto(path);
    await expect(page.getByTestId("health-bar")).toHaveAttribute(
      "data-wall",
      "arena_not_drawing",
      { timeout: 10000 },
    );
    const segment = page.getByTestId("health-wall");
    await expect(segment).toHaveText("Stena: Resolume nekreslí na Display 1", {
      timeout: 10000,
    });
    await expect(segment).toHaveClass(/health-error/);
  }
});

test("Resolume not running or with no saved output reads grey, the read failing too", async ({
  page,
  request,
}) => {
  test.setTimeout(60_000);
  await request.post("/__mock/wall", { data: { state: "arena_off", read: { arena_running: false } } });
  await page.goto("/");
  const segment = page.getByTestId("health-wall");
  await expect(segment).toHaveText("Stena: Resolume nebeží", { timeout: 10000 });
  await expect(segment).toHaveClass(/health-off/);
  // The advanced output on with no preset saved: the operator's setup,
  // calm grey (never "nedá sa overiť").
  await request.post("/__mock/wall", {
    data: {
      state: "no_preset",
      read: {
        arena_running: true,
        arena_output: { advanced: true, preset: null, preset_found: false, displays: [] },
      },
    },
  });
  await expect(segment).toHaveText("Stena: Resolume nemá uložený výstup", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-off/);
  await expect(segment).toHaveAttribute("data-state", "no_preset");
  await expect(segment).toHaveAttribute("title", RULE);
  await request.post("/__mock/wall", {
    data: {
      state: "unknown",
      problems: [
        {
          code: "unreadable:arena_output",
          short_sk: "nedá sa overiť",
          text_sk: "uložený výstup Resolume Arena sa nepodarilo zistiť",
        },
      ],
      read_errors: [
        "C:\\Users\\Resolume\\Documents\\Resolume Arena\\Preferences\\AdvancedOutput.xml: not XML: …",
      ],
    },
  });
  await expect(segment).toHaveText("Stena: nedá sa overiť", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-off/);
  await expect(segment).toHaveAttribute("data-state", "unknown");
  await expect(segment).toHaveAttribute(
    "title",
    `${RULE} Zistené: uložený výstup Resolume Arena sa nepodarilo zistiť.`,
  );
});

/** #254: the guard's record of its end of Arena 15176 (SNV, 14.10.2026). */
function guardEnded(phase: string, error: string | null = null) {
  return {
    ends: 1,
    last: {
      at_ms: 1791608681366,
      why: "crash_report",
      pid: 15176,
      monitors: [9188],
      error,
    },
    phase,
  };
}

const CRASH_TEXT =
  "SongPlayer ukončil Resolume Arena (proces 15176), lebo Resolume Arena spadol a jeho " +
  "hlásenie chýb (BugSplat) čakalo na klik v okne „Crash Report“. Znova ho spúšťa skript " +
  "operátora; klipy spúšťa len Stream Deck.";

test("a Resolume the guard ended reads red until it runs again", async ({ page, request }) => {
  // Three changes, each seen at the bar's next 5 s poll.
  test.setTimeout(90_000);
  await request.post("/__mock/wall", {
    data: {
      state: "arena_off",
      read: { arena_running: false },
      guard: guardEnded("relaunching"),
    },
  });
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  await expect(bar).toHaveAttribute("data-wall", "arena_off", { timeout: 10000 });
  const segment = page.getByTestId("health-wall");
  await expect(segment).toHaveText(
    "Stena: Resolume spadol — ukončený, čaká na nové spustenie",
    { timeout: 10000 },
  );
  await expect(segment).toHaveClass(/health-error/);
  await expect(segment).toHaveAttribute("data-state", "arena_off");
  await expect(segment).toHaveAttribute("data-guard", "relaunching");
  await expect(segment).toHaveAttribute("title", `${RULE} ${CRASH_TEXT}`);

  // Not back 60 s after the end: still red, saying so.
  await request.post("/__mock/wall", {
    data: {
      state: "arena_off",
      read: { arena_running: false },
      guard: guardEnded("not_back"),
    },
  });
  await expect(segment).toHaveText("Stena: Resolume spadol — ukončený, ale znova sa nespustil", {
    timeout: 15000,
  });
  await expect(segment).toHaveClass(/health-error/);
  await expect(segment).toHaveAttribute("data-guard", "not_back");
  await expect(segment).toHaveAttribute(
    "title",
    `${RULE} ${CRASH_TEXT} Do 60 s sa znova nespustil.`,
  );

  // Its end failed: the segment says so, whatever the wall reads.
  await request.post("/__mock/wall", {
    data: {
      state: "ok",
      read: READ,
      guard: guardEnded("relaunching", "TerminateProcess(15176, Arena.exe) failed (error 5)"),
    },
  });
  await expect(segment).toHaveText("Stena: Resolume spadol — nepodarilo sa ho ukončiť", {
    timeout: 15000,
  });
  await expect(segment).toHaveClass(/health-error/);
  await expect(segment).toHaveAttribute("data-state", "ok");

  // A new Resolume runs: the wall's own state again, the end kept on record.
  await request.post("/__mock/wall", {
    data: { state: "ok", read: READ, guard: guardEnded("idle") },
  });
  await expect(segment).toHaveText("Stena: OK", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-ok/);
  await expect(segment).toHaveAttribute("data-guard", "idle");
  await expect(segment).toHaveAttribute(
    "title",
    `${RULE} Resolume (preset „snv surround 7680“) chce: Display 1 7680×1080.`,
  );
});

// #275: whether SongPlayer's clip runs in Resolume (the report's
// `songplayer_clip`, read by SongPlayer's Resolume driver). SNV, 14.10.2026:
// after an Arena relaunch no clip ran until the operator pressed a Stream
// Deck scene, the wall was black for 5.5 h and the segment read "Stena: OK".
// While a song plays on program (the mock's replay: playlist 1 on program
// with a song) and no SongPlayer clip runs, the segment says so in amber,
// with how long; never red, never an action (clips run only from the
// operator's Stream Deck).

const YT_MUSIC = { layer: "L# Fill", clip: "Yt music", source: "SP-program-MAX" };
const SNV_BG = { layer: "L # BG", clip: "SNV BG", source: "NDI Source" };

/** No SongPlayer clip runs, since 12 min 5 s ago (the mock stamps `now_ms`
 * at each answer: the text holds "12 min" for 55 s). */
function noClipFor12Min() {
  return {
    state: "none",
    clips: [],
    ndi_clips: [SNV_BG],
    since_ms: Date.now() - (12 * 60 + 5) * 1000,
  };
}

const WANTS = `${RULE} Resolume (preset „snv surround 7680“) chce: Display 1 7680×1080.`;
const NONE_TIP =
  "Na stene už 12 min nebeží žiadny Spout klip SongPlayeru (SP-program-MAX ani SP-program). " +
  "Klipy spúšťa len operátor zo Stream Decku, SongPlayer žiadny nespúšťa. Klip so zdrojom NDI " +
  "SongPlayer nerozozná (Resolume nepovie, aký NDI zdroj má); viditeľné NDI klipy: L # BG — SNV BG.";
const NOTE = "Stena: nebeží Spout klip SongPlayeru (12 min)";

test("a song on program with no SongPlayer clip running reads an amber note until the clip runs", async ({
  page,
  request,
}) => {
  test.setTimeout(60_000);
  await request.post("/__mock/wall", {
    data: { state: "ok", read: READ, songplayer_clip: noClipFor12Min() },
  });
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  const segment = page.getByTestId("health-wall");
  await expect(bar).toHaveAttribute("data-clip", "none", { timeout: 15000 });
  await expect(segment).toHaveText(NOTE, { timeout: 15000 });
  await expect(segment).toHaveClass(/health-warn/);
  await expect(segment).toHaveAttribute("data-state", "ok");
  await expect(segment).toHaveAttribute("data-clip-missing", "true");
  await expect(segment).toHaveAttribute("title", `${WANTS} ${NONE_TIP}`);

  // The operator presses his Stream Deck scene: the wall's own words again,
  // the running clip named in the tooltip.
  await request.post("/__mock/wall", {
    data: {
      state: "ok",
      read: READ,
      songplayer_clip: {
        state: "running",
        clips: [YT_MUSIC],
        ndi_clips: [SNV_BG],
        since_ms: Date.now(),
      },
    },
  });
  await expect(segment).toHaveText("Stena: OK", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-ok/);
  await expect(segment).toHaveAttribute("data-clip-missing", "false");
  await expect(bar).toHaveAttribute("data-clip", "running");
  await expect(segment).toHaveAttribute(
    "title",
    `${WANTS} Na stene beží klip SongPlayeru: L# Fill — Yt music (SP-program-MAX).`,
  );
});

test("the note follows the program: no song on program, no note, the fact in the tooltip", async ({
  page,
  request,
}) => {
  test.setTimeout(60_000);
  await request.post("/__mock/wall", {
    data: { state: "ok", read: READ, songplayer_clip: noClipFor12Min() },
  });
  await page.goto("/");
  const segment = page.getByTestId("health-wall");
  await expect(segment).toHaveText(NOTE, { timeout: 15000 });

  // The song leaves the program (a cut to "OBS manuál" for the sermon): the
  // operator may show anything then.
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 1, state: "WaitingForScene", transport: "Paused" },
  });
  await expect(segment).toHaveText("Stena: OK", { timeout: 10000 });
  await expect(segment).toHaveClass(/health-ok/);
  await expect(segment).toHaveAttribute("data-clip-missing", "false");
  await expect(segment).toHaveAttribute("title", `${WANTS} ${NONE_TIP}`);

  // Back on program: the note again.
  await request.post("/__mock/set-playing", {
    data: { playlist_id: 1, state: "Playing", transport: "Playing" },
  });
  await expect(segment).toHaveText(NOTE, { timeout: 10000 });
  await expect(segment).toHaveAttribute("data-clip-missing", "true");
});

test("a red wall or a clip not read keeps its own words: the note never hides them", async ({
  page,
  request,
}) => {
  test.setTimeout(60_000);
  await request.post("/__mock/wall", {
    data: {
      state: "display_missing",
      problems: [DISPLAY_SIZE],
      read: READ,
      songplayer_clip: noClipFor12Min(),
    },
  });
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  const segment = page.getByTestId("health-wall");
  await expect(bar).toHaveAttribute("data-clip", "none", { timeout: 15000 });
  await expect(segment).toHaveText(`Stena: ${DISPLAY_SIZE.short_sk}`, { timeout: 15000 });
  await expect(segment).toHaveClass(/health-error/);
  await expect(segment).toHaveAttribute("data-clip-missing", "false");
  await expect(segment).toHaveAttribute(
    "title",
    `${WANTS} Zistené: ${DISPLAY_SIZE.text_sk}. ${NONE_TIP}`,
  );

  // Resolume not read (it does not answer): no claim, no sentence.
  await request.post("/__mock/wall", {
    data: { state: "ok", read: READ, songplayer_clip: { state: "unknown" } },
  });
  await expect(segment).toHaveText("Stena: OK", { timeout: 15000 });
  await expect(bar).toHaveAttribute("data-clip", "unknown");
  await expect(segment).toHaveAttribute("data-clip-missing", "false");
  await expect(segment).toHaveAttribute("title", WANTS);
});
