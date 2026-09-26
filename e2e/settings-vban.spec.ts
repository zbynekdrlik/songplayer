import { test, expect, type Page } from "@playwright/test";

// #210 (B2 of EPIC #174): the Nastavenia "Zvuk programu cez VBAN" fieldset —
// the enable toggle, the stream name and the targets. A real user opens the
// Settings page, clicks the toggle, types the name + targets, clicks
// "Uložiť nastavenia", reloads the page, and turns it off again. Asserts the
// visible result AND the backend effect: exactly one PATCH /api/v1/settings per
// save carrying the three keys, the values surviving a reload, and the mock's
// GET /api/v1/program `vban` block following the stored settings. Zero console
// errors is each test's last assertion.

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

// Open Nastavenia and wait until the loaded settings are IN THE FORM, so the
// load can never overwrite what the test clicks/types afterwards. The Gemini
// model is the proof: the fixture's `gemini-2.5-flash` differs from the form's
// built-in default, so it shows only once the page's GET landed and the form's
// sync Effect ran (the vban_* fields' defaults equal the fixture's, so they
// cannot prove it).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-vban"]')).toBeVisible({
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

test("VBAN defaults to off, `sp-program`, no targets; one save sends all three and they survive a reload (#210)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  const enabled = page.locator('[data-testid="settings-vban-enabled"]');
  const name = page.locator('[data-testid="settings-vban-stream-name"]');
  const targets = page.locator('[data-testid="settings-vban-targets"]');
  await expect(enabled).not.toBeChecked();
  await expect(name).toHaveValue("sp-program");
  await expect(targets).toHaveValue("");
  await expect(name).toHaveAttribute("maxlength", "16");

  // Turn it on and point it at a test receiver, like the operator would.
  await enabled.click();
  await expect(enabled).toBeChecked();
  await name.click();
  await name.fill("sp-program-test");
  await targets.click();
  await targets.fill("dev1.lan:6980, lv1.lan:6980");
  const saved = page.waitForRequest(
    (req) => req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url()),
  );
  await page.locator('.settings-form button[type="submit"]').click();
  await saved;
  await expect(page.locator(".save-status")).toHaveText("Uložené");

  expect(patches).toHaveLength(1);
  expect(patches[0]["vban_enabled"]).toBe("true");
  expect(patches[0]["vban_stream_name"]).toBe("sp-program-test");
  expect(patches[0]["vban_targets"]).toBe("dev1.lan:6980, lv1.lan:6980");

  // Backend effect: the stored settings drive the program's `vban` block.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.vban.enabled).toBe(true);
  expect(program.vban.stream_name).toBe("sp-program-test");
  expect(program.vban.targets.map((t: { target: string }) => t.target)).toEqual([
    "dev1.lan:6980",
    "lv1.lan:6980",
  ]);

  // A fresh load shows what was saved (read back through GET /api/v1/settings).
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-vban-enabled"]')).toBeChecked({
    timeout: 10000,
  });
  await expect(page.locator('[data-testid="settings-vban-stream-name"]')).toHaveValue(
    "sp-program-test",
  );
  await expect(page.locator('[data-testid="settings-vban-targets"]')).toHaveValue(
    "dev1.lan:6980, lv1.lan:6980",
  );

  expect(realConsoleErrors()).toEqual([]);
});

test("turning VBAN off again saves `false` and the program reports it off (#210)", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", {
    data: { vban_enabled: "true", vban_targets: "dev1.lan:6980" },
  });
  const patches = settingsPatches(page);
  await openSettings(page);

  const enabled = page.locator('[data-testid="settings-vban-enabled"]');
  await expect(enabled).toBeChecked();
  await expect(page.locator('[data-testid="settings-vban-targets"]')).toHaveValue(
    "dev1.lan:6980",
  );
  await enabled.click();
  await expect(enabled).not.toBeChecked();
  const saved = page.waitForRequest(
    (req) => req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url()),
  );
  await page.locator('.settings-form button[type="submit"]').click();
  await saved;
  await expect(page.locator(".save-status")).toHaveText("Uložené");

  expect(patches).toHaveLength(1);
  expect(patches[0]["vban_enabled"]).toBe("false");
  expect(patches[0]["vban_targets"], "the targets are kept").toBe("dev1.lan:6980");
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.vban.enabled).toBe(false);

  expect(realConsoleErrors()).toEqual([]);
});
