import { test, expect, type Page } from "@playwright/test";

// #239: the Nastavenia "Výstupy Spout (Resolume)" fieldset — the program's
// two Spout senders, SP-program-MAX (3840×2160) and SP-program (1920×1080),
// each a checkbox, both ON by default. The FHD sender rides MAX's thread, so
// its checkbox is disabled (with a Slovak note) while MAX is unchecked. A
// real user opens the Settings page, clicks, saves and reloads. Asserts the
// visible result AND the backend effect: one PATCH /api/v1/settings per save
// carrying both keys, the values surviving a reload, and the mock's
// GET /api/v1/program `max.fhd` block following the stored settings (off
// with reason `setting_off`, or `max_off` while MAX is off). Zero console
// errors is each test's last assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const MAX_OFF_NOTE = "SP-program cez Spout beží len so zapnutým SP-program-MAX.";

let consoleMessages: string[] = [];

function realConsoleErrors(): string[] {
  return consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

// Open Nastavenia and wait until the loaded settings are IN THE FORM (the
// fixture's `gemini-2.5-flash` differs from the form default, so it proves the
// load landed — the Spout defaults equal the fixture's and cannot).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-spout"]')).toBeVisible({
    timeout: 10000,
  });
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
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/settings-reset");
});

test("both Spout senders are on by default; turning SP-program off saves false and survives a reload (#239)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  const max = page.locator('[data-testid="settings-max-enabled"]');
  const fhd = page.locator('[data-testid="settings-spout-fhd-enabled"]');
  await expect(max).toBeChecked();
  await expect(fhd).toBeChecked();
  await expect(fhd).toBeEnabled();
  await expect(page.locator('[data-testid="settings-spout-fhd-hint"]')).toHaveText("");

  let program = await (await request.get("/api/v1/program")).json();
  expect(program.max.fhd.enabled).toBe(true);
  expect(program.max.fhd.reason).toBeNull();
  expect(program.max.fhd.spout_name).toBe("SP-program");

  await fhd.click();
  await expect(fhd).not.toBeChecked();
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["program_spout_fhd_enabled"]).toBe("false");
  expect(patches[0]["program_max_enabled"]).toBe("true");

  // Backend effect: the stored setting drives the program's `max.fhd` block.
  program = await (await request.get("/api/v1/program")).json();
  expect(program.max.enabled).toBe(true);
  expect(program.max.fhd.enabled).toBe(false);
  expect(program.max.fhd.reason).toBe("setting_off");

  // A fresh load shows what was saved.
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-spout-fhd-enabled"]')).not.toBeChecked({
    timeout: 10000,
  });
  await expect(page.locator('[data-testid="settings-max-enabled"]')).toBeChecked();

  expect(realConsoleErrors()).toEqual([]);
});

test("with SP-program-MAX off the SP-program checkbox is disabled and the program says max_off (#239)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  const max = page.locator('[data-testid="settings-max-enabled"]');
  const fhd = page.locator('[data-testid="settings-spout-fhd-enabled"]');
  const hint = page.locator('[data-testid="settings-spout-fhd-hint"]');
  await max.click();
  await expect(max).not.toBeChecked();
  await expect(fhd).toBeDisabled();
  await expect(fhd).toBeChecked();
  await expect(hint).toHaveText(MAX_OFF_NOTE);
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["program_max_enabled"]).toBe("false");
  expect(patches[0]["program_spout_fhd_enabled"]).toBe("true");
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.max.enabled).toBe(false);
  expect(program.max.fhd.enabled).toBe(true);
  expect(program.max.fhd.reason).toBe("max_off");

  // A fresh load keeps it disabled; MAX back on enables it again.
  await openSettings(page);
  const maxAgain = page.locator('[data-testid="settings-max-enabled"]');
  const fhdAgain = page.locator('[data-testid="settings-spout-fhd-enabled"]');
  await expect(maxAgain).not.toBeChecked({ timeout: 10000 });
  await expect(fhdAgain).toBeDisabled();
  await maxAgain.click();
  await expect(fhdAgain).toBeEnabled();
  await expect(page.locator('[data-testid="settings-spout-fhd-hint"]')).toHaveText("");

  expect(realConsoleErrors()).toEqual([]);
});
