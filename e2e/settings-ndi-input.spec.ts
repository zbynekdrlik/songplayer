import { test, expect, type Page } from "@playwright/test";

// #212 (B3 of EPIC #174): the Nastavenia "Vstup NDI „OBS manuál“" fieldset —
// the enable toggle and the full NDI source name ("MACHINE (stream)"). A real
// user opens the Settings page, clicks the toggle, types the source, clicks
// "Uložiť nastavenia", reloads, and turns it off again. Asserts the visible
// result AND the backend effect: exactly one PATCH /api/v1/settings per save
// carrying both keys, the values surviving a reload, and the mock's
// GET /api/v1/program `input` block following the stored settings. Zero
// console errors is each test's last assertion.

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
// load landed — the ndi_input_* defaults equal the fixture's and cannot).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-ndi-input"]')).toBeVisible({
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
  await request.post("/__mock/program-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/settings-reset");
  await request.post("/__mock/program-reset");
});

test("the NDI input defaults to off with no source; one save sends both and they survive a reload (#212)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  const enabled = page.locator('[data-testid="settings-ndi-input-enabled"]');
  const source = page.locator('[data-testid="settings-ndi-input-source"]');
  await expect(enabled).not.toBeChecked();
  await expect(source).toHaveValue("");
  await expect(source).toHaveAttribute("placeholder", "CG-OBS (manual)");

  // Turn it on and name the cg OBS manual-mix NDI output, like the operator.
  await enabled.click();
  await expect(enabled).toBeChecked();
  await source.click();
  await source.fill("CG-OBS (manual)");
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["ndi_input_enabled"]).toBe("true");
  expect(patches[0]["ndi_input_source"]).toBe("CG-OBS (manual)");

  // Backend effect: the stored settings drive the program's `input` block.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.input.enabled).toBe(true);
  expect(program.input.source).toBe("CG-OBS (manual)");
  expect(program.input.stream).toBe("manual");
  expect(program.input.label).toBe("OBS manuál");

  // A fresh load shows what was saved.
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-ndi-input-enabled"]')).toBeChecked({
    timeout: 10000,
  });
  await expect(page.locator('[data-testid="settings-ndi-input-source"]')).toHaveValue(
    "CG-OBS (manual)",
  );

  expect(realConsoleErrors()).toEqual([]);
});

test("turning the NDI input off saves `false`, keeps the source and drops OBS manuál from the Program control (#212)", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", {
    data: { ndi_input_enabled: "true", ndi_input_source: "CG-OBS (manual)" },
  });
  const patches = settingsPatches(page);

  // Enabled: the dashboard Program control lists OBS manuál.
  await page.goto("/");
  const inputCut = page.locator('[data-testid="program-cut"][data-playlist-id="-1"]');
  await expect(inputCut).toBeVisible({ timeout: 10000 });

  await openSettings(page);
  const enabled = page.locator('[data-testid="settings-ndi-input-enabled"]');
  await expect(enabled).toBeChecked();
  await expect(page.locator('[data-testid="settings-ndi-input-source"]')).toHaveValue(
    "CG-OBS (manual)",
  );
  await enabled.click();
  await expect(enabled).not.toBeChecked();
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["ndi_input_enabled"]).toBe("false");
  expect(patches[0]["ndi_input_source"], "the source is kept").toBe("CG-OBS (manual)");
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.input.enabled).toBe(false);

  // Disabled: the Program control no longer offers it.
  await page.goto("/");
  await expect(page.getByTestId("program-source")).toHaveText("Na programe: Worship", {
    timeout: 10000,
  });
  await expect(page.getByTestId("program-cut")).toHaveCount(3);
  await expect(inputCut).toHaveCount(0);

  expect(realConsoleErrors()).toEqual([]);
});
