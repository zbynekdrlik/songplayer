import { test, expect, type Page } from "@playwright/test";

// #215 (B5 of EPIC #174): the Nastavenia "Prechody programu (SP-program)"
// fieldset — whether SP-program follows cg OBS's program scene natively (off by
// default; it replaces the event-night watcher script) and the transition every
// program cut uses: cg OBS's own scene transition (default), always a fade of
// N ms, or always a hard cut. A real user opens the Settings page, clicks the
// checkbox, picks the transition, types the fade length, clicks "Uložiť
// nastavenia", reloads, and then sees the result on the dashboard's Program
// control. Asserts the visible result AND the backend effect: exactly one
// PATCH /api/v1/settings per save carrying the three keys, the values
// surviving a reload, and the mock's GET /api/v1/program `follow` +
// `transition` blocks following the stored settings. Zero console errors is
// each test's last assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

function realConsoleErrors(): string[] {
  return consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

// Open Nastavenia and wait until the loaded settings are IN THE FORM (the
// fixture's `gemini-2.5-flash` differs from the form default, so it proves the
// load landed — the program_* defaults equal the form's and cannot).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(
    page.locator('[data-testid="settings-program-transition"]'),
  ).toBeVisible({ timeout: 10000 });
  await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue(
    "gemini-2.5-flash",
    { timeout: 10000 },
  );
}

function settingsPatches(page: Page): Record<string, unknown>[] {
  const bodies: Record<string, unknown>[] = [];
  page.on("request", (req) => {
    if (req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url())) {
      bodies.push(req.postDataJSON());
    }
  });
  return bodies;
}

async function save(page: Page) {
  const saved = page.waitForRequest(
    (req) => req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url()),
  );
  await page.locator('.settings-form button[type="submit"]').click();
  await saved;
  await expect(page.locator(".save-status")).toHaveText("Uložené");
}

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/settings-reset");
  await request.post("/__mock/program-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/settings-reset");
  await request.post("/__mock/program-reset");
});

test("the transition defaults to cg OBS's with no follow; one save sends all three and the dashboard shows it (#215)", async ({
  page,
  request,
}) => {
  await page.setViewportSize({ width: 1600, height: 1000 });
  const patches = settingsPatches(page);
  await openSettings(page);

  const follow = page.locator('[data-testid="settings-program-follow-obs"]');
  const kind = page.locator('[data-testid="settings-program-transition-kind"]');
  const ms = page.locator('[data-testid="settings-program-transition-ms"]');
  await expect(follow).not.toBeChecked();
  await expect(kind).toHaveValue("obs");
  await expect(kind.locator("option")).toHaveText([
    "Podľa OBS (odporúčané)",
    "Vždy prelínanie",
    "Vždy strih",
  ]);
  await expect(ms).toHaveValue("300");

  // Follow cg OBS, always fade, 500 ms — like the operator.
  await follow.click();
  await expect(follow).toBeChecked();
  await kind.selectOption("fade");
  await ms.click();
  await ms.fill("500");
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["program_follow_obs"]).toBe("true");
  expect(patches[0]["program_transition"]).toBe("fade");
  expect(patches[0]["program_transition_ms"]).toBe("500");

  // Backend effect: the stored settings drive `follow` and the next cut's spec.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.follow.enabled).toBe(true);
  expect(program.follow.mode).toBe("fade");
  expect(program.follow.ms).toBe(500);
  expect(program.transition.kind).toBe("fade");
  expect(program.transition.duration_ms).toBe(500);
  expect(program.transition.n_slots).toBe(15);
  expect(program.transition.source).toBe("setting");

  // A fresh load shows what was saved.
  await openSettings(page);
  await expect(
    page.locator('[data-testid="settings-program-follow-obs"]'),
  ).toBeChecked({ timeout: 10000 });
  await expect(
    page.locator('[data-testid="settings-program-transition-kind"]'),
  ).toHaveValue("fade");
  await expect(
    page.locator('[data-testid="settings-program-transition-ms"]'),
  ).toHaveValue("500");

  // The dashboard's Program control shows the same transition.
  await page.locator('[data-testid="nav-dashboard"]').click();
  await expect(page.getByTestId("program-transition")).toHaveText(
    "Prechod: prelínanie 500 ms (nastavenie)",
    { timeout: 10000 },
  );

  expect(realConsoleErrors()).toEqual([]);
});

test("turning the follow off and picking a hard cut saves both and keeps the fade length (#215)", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", {
    data: {
      program_follow_obs: "true",
      program_transition: "fade",
      program_transition_ms: "700",
    },
  });
  const patches = settingsPatches(page);
  await openSettings(page);

  const follow = page.locator('[data-testid="settings-program-follow-obs"]');
  const kind = page.locator('[data-testid="settings-program-transition-kind"]');
  await expect(follow).toBeChecked();
  await expect(kind).toHaveValue("fade");
  await expect(
    page.locator('[data-testid="settings-program-transition-ms"]'),
  ).toHaveValue("700");
  await follow.click();
  await expect(follow).not.toBeChecked();
  await kind.selectOption("cut");
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["program_follow_obs"]).toBe("false");
  expect(patches[0]["program_transition"]).toBe("cut");
  expect(patches[0]["program_transition_ms"], "the length is kept").toBe("700");
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.follow.enabled).toBe(false);
  expect(program.follow.mode).toBe("cut");
  expect(program.transition.kind).toBe("cut");
  expect(program.transition.n_slots).toBe(0);
  expect(program.transition.source).toBe("setting");

  expect(realConsoleErrors()).toEqual([]);
});
