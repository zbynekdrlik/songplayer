import { test, expect, type Page } from "@playwright/test";

// #213 (C of EPIC #174): the Nastavenia "Diaľkové ovládanie (Companion)"
// fieldset — the enable toggle, the port (default 4456) and the optional
// password of SongPlayer's obs-websocket 5 subset. A real user opens the
// Settings page, clicks the toggle, types the port and the password, clicks
// "Uložiť nastavenia", reloads, and turns it off again. Asserts the visible
// result AND the backend effect: exactly one PATCH /api/v1/settings per save
// carrying the three keys, the values surviving a reload, and the mock's
// GET /api/v1/program `remote` block following the stored settings. Zero
// console errors is each test's last assertion.

/** The obs-websocket 5 spec's example password (the only password in tests). */
const SPEC_PASSWORD = "supersecretpassword";

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
// load landed — the remote_ws_* defaults equal the fixture's and cannot).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-remote"]')).toBeVisible({
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

test("the remote control defaults to off on 4456 without a password; one save sends all three and they survive a reload (#213)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  const enabled = page.locator('[data-testid="settings-remote-enabled"]');
  const port = page.locator('[data-testid="settings-remote-port"]');
  const password = page.locator('[data-testid="settings-remote-password"]');
  await expect(enabled).not.toBeChecked();
  await expect(port).toHaveValue("4456");
  await expect(password).toHaveValue("");
  await expect(password).toHaveAttribute("type", "password");

  // Turn it on, move it to another port and set a password, like the operator.
  await enabled.click();
  await expect(enabled).toBeChecked();
  await port.click();
  await port.fill("4460");
  await password.click();
  await password.fill(SPEC_PASSWORD);
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["remote_ws_enabled"]).toBe("true");
  expect(patches[0]["remote_ws_port"]).toBe("4460");
  expect(patches[0]["remote_ws_password"]).toBe(SPEC_PASSWORD);

  // Backend effect: the stored settings drive the program's `remote` block.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.remote.enabled).toBe(true);
  expect(program.remote.port).toBe(4460);
  expect(program.remote.auth).toBe(true);
  expect(program.remote.clients).toBe(0);

  // A fresh load shows what was saved.
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-remote-enabled"]')).toBeChecked({
    timeout: 10000,
  });
  await expect(page.locator('[data-testid="settings-remote-port"]')).toHaveValue("4460");
  await expect(page.locator('[data-testid="settings-remote-password"]')).toHaveValue(
    SPEC_PASSWORD,
  );

  expect(realConsoleErrors()).toEqual([]);
});

test("turning the remote control off saves `false` and keeps the port (#213)", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", {
    data: { remote_ws_enabled: "true", remote_ws_port: "4461" },
  });
  const patches = settingsPatches(page);

  await openSettings(page);
  const enabled = page.locator('[data-testid="settings-remote-enabled"]');
  await expect(enabled).toBeChecked();
  await expect(page.locator('[data-testid="settings-remote-port"]')).toHaveValue("4461");
  await enabled.click();
  await expect(enabled).not.toBeChecked();
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["remote_ws_enabled"]).toBe("false");
  expect(patches[0]["remote_ws_port"], "the port is kept").toBe("4461");
  expect(patches[0]["remote_ws_password"]).toBe("");
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.remote.enabled).toBe(false);
  expect(program.remote.port).toBe(4461);
  expect(program.remote.auth).toBe(false);

  expect(realConsoleErrors()).toEqual([]);
});
