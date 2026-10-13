import { test, expect } from "@playwright/test";

// #259 — the LED wall report on the shared HealthBar: `Stena: …`, green
// while Resolume draws on every display its own saved output wants, grey
// while the screens are off (or Resolume does not run), red while a display
// Resolume wants is missing or it does not draw on it, with the rule, what
// Resolume wants and every problem as the tooltip. Nothing while Resolume
// drives no physical display (the mock's default: its simple output). The
// bar polls `GET /api/v1/wall` every 5 s, so a change shows up without a
// reload. Runs against the mock.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

const RULE =
  "SongPlayer porovnáva obrazovky Windows s výstupom, ktorý má Resolume Arena uložený, " +
  "a len hlási — nič nenastavuje ani nemení.";

const SPAN_KEY =
  "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{49D71F75-AF5A-11EE-BACA-806E6F6E6963}\\0000";

/** What SNV's Resolume wants, as the server reads it. */
const READ = {
  arena_running: true,
  arena_windows: [],
  arena_output: {
    advanced: true,
    preset: "snv surround 7680",
    displays: [
      { name: "Display 1", device_id: SPAN_KEY, width: 7680, height: 1080, fullscreen: true },
    ],
  },
  displays: [],
  nvidia: true,
  connected: [],
  grids: [],
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

test("Resolume not running reads grey, the read failing reads grey too", async ({
  page,
  request,
}) => {
  test.setTimeout(60_000);
  await request.post("/__mock/wall", { data: { state: "arena_off", read: { arena_running: false } } });
  await page.goto("/");
  const segment = page.getByTestId("health-wall");
  await expect(segment).toHaveText("Stena: Resolume nebeží", { timeout: 10000 });
  await expect(segment).toHaveClass(/health-off/);
  await request.post("/__mock/wall", {
    data: {
      state: "unknown",
      problems: [
        {
          code: "unreadable:displays",
          short_sk: "nedá sa overiť",
          text_sk: "zoznam obrazoviek Windows sa nepodarilo zistiť",
        },
      ],
      read_errors: ["EnumDisplaySettingsW failed for \\\\.\\DISPLAY1"],
    },
  });
  await expect(segment).toHaveText("Stena: nedá sa overiť", { timeout: 15000 });
  await expect(segment).toHaveClass(/health-off/);
  await expect(segment).toHaveAttribute("data-state", "unknown");
  await expect(segment).toHaveAttribute(
    "title",
    `${RULE} Zistené: zoznam obrazoviek Windows sa nepodarilo zistiť.`,
  );
});
