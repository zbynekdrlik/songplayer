import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// #229 (lane 1 of the node exchange): the settings API never shows a secret.
// `GET /api/v1/settings` reads every stored secret setting as the mask
// `********` (`sp_core::config::{SECRET_MASK, is_secret_setting}`), a PATCH
// that sends the mask back keeps the stored secret, any other value replaces
// it and `""` clears it; the PATCH answers 204 with no body. The mock mirrors
// that contract (`e2e/mock-api.mjs` `shownSettings`). A real user opens
// Nastavenia, sees the masks in the password fields, saves a plain change,
// types a new key and clears a password, and reloads. Asserts the visible
// result AND the backend effect: the PATCH body the form sent, the mock's
// stored secrets (through `GET /api/v1/program` `remote.auth`, which reads
// the stored remote password) and a fresh GET. Zero console errors is each
// test's last assertion.

const MASK = "********";

// The seeded secrets. Every value carries `example`: they are test fixtures,
// never credentials (the staging secret scan treats them as placeholders).
const SEEDED = {
  gemini_api_key: "example-gemini-one,example-gemini-two",
  obs_websocket_password: "example-obs-pass",
  remote_ws_password: "example-remote-pass",
  genius_access_token: "example-genius-token",
};

/** Every clear text the seed stored: none of them may ever be shown. */
const CLEAR_VALUES = [
  ...Object.values(SEEDED),
  ...SEEDED.gemini_api_key.split(","),
];

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
// load landed — a masked secret alone could not tell a load from a default).
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

/** The three secret fields of the form, located the way an operator reads them. */
function secretFields(page: Page) {
  return {
    obsPassword: page
      .getByRole("group", { name: "OBS WebSocket" })
      .getByLabel("Heslo", { exact: true }),
    geminiKey: page.getByLabel("API kľúč"),
    remotePassword: page.locator('[data-testid="settings-remote-password"]'),
  };
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

/** `GET /api/v1/settings`: its raw text and the parsed map. */
async function readSettings(
  request: APIRequestContext,
): Promise<{ text: string; json: Record<string, string> }> {
  const resp = await request.get("/api/v1/settings");
  expect(resp.status()).toBe(200);
  const text = await resp.text();
  return { text, json: JSON.parse(text) as Record<string, string> };
}

/** No stored clear value appears anywhere in a GET's text. */
function expectNoClearValue(text: string) {
  for (const [i, value] of CLEAR_VALUES.entries()) {
    expect(text.includes(value), `clear value ${i} shown by GET /api/v1/settings`).toBe(
      false,
    );
  }
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
  // The server's PATCH contract: 204, no body.
  const seed = await request.patch("/api/v1/settings", { data: SEEDED });
  expect(seed.status()).toBe(204);
  expect(await seed.text()).toBe("");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/settings-reset");
  await request.post("/__mock/program-reset");
});

test("every stored secret reads as the mask, in the API and in the Nastavenia form (#229)", async ({
  page,
  request,
}) => {
  const { text, json } = await readSettings(request);
  expect(json["gemini_api_key"]).toBe(MASK);
  expect(json["obs_websocket_password"]).toBe(MASK);
  expect(json["remote_ws_password"]).toBe(MASK);
  expect(json["genius_access_token"]).toBe(MASK);
  expect(json["gemini_model"], "a plain setting reads as stored").toBe(
    "gemini-2.5-flash",
  );
  expectNoClearValue(text);

  await openSettings(page);
  const fields = secretFields(page);
  for (const field of [fields.obsPassword, fields.geminiKey, fields.remotePassword]) {
    await expect(field).toHaveValue(MASK);
    await expect(field).toHaveAttribute("type", "password");
  }

  expect(realConsoleErrors()).toEqual([]);
});

test("a save that changes only a plain setting sends the masks back and keeps the secrets (#229)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  const fields = secretFields(page);
  await expect(fields.geminiKey).toHaveValue(MASK);

  const model = page.locator('[data-testid="settings-gemini-model"]');
  await model.click();
  await model.fill("gemini-2.5-pro");
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["gemini_api_key"]).toBe(MASK);
  expect(patches[0]["obs_websocket_password"]).toBe(MASK);
  expect(patches[0]["remote_ws_password"]).toBe(MASK);
  expect(patches[0]["gemini_model"]).toBe("gemini-2.5-pro");

  // Backend effect: the stored remote password was kept (the mask wrote
  // nothing), so the remote control still asks for a password.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.remote.auth).toBe(true);

  const { text, json } = await readSettings(request);
  expect(json["gemini_api_key"]).toBe(MASK);
  expect(json["obs_websocket_password"]).toBe(MASK);
  expect(json["remote_ws_password"]).toBe(MASK);
  expect(json["genius_access_token"]).toBe(MASK);
  expect(json["gemini_model"]).toBe("gemini-2.5-pro");
  expectNoClearValue(text);

  expect(realConsoleErrors()).toEqual([]);
});

test("typing a new secret replaces it and a cleared one is removed; a reload shows the mask again (#229)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  const fields = secretFields(page);
  await expect(fields.geminiKey).toHaveValue(MASK);
  await expect(fields.remotePassword).toHaveValue(MASK);

  await fields.geminiKey.click();
  await fields.geminiKey.fill("example-gemini-three");
  await fields.remotePassword.click();
  await fields.remotePassword.fill("");
  await save(page);

  expect(patches).toHaveLength(1);
  expect(patches[0]["gemini_api_key"]).toBe("example-gemini-three");
  expect(patches[0]["remote_ws_password"]).toBe("");
  expect(patches[0]["obs_websocket_password"], "an untouched secret goes back masked").toBe(
    MASK,
  );

  // Backend effect: the cleared remote password is gone.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.remote.auth).toBe(false);

  const { text, json } = await readSettings(request);
  expect(json["gemini_api_key"]).toBe(MASK);
  expect(json["remote_ws_password"], "a cleared secret reads as stored: empty").toBe("");
  expect(text.includes("example-gemini-three"), "the new key shown by GET").toBe(false);

  // A fresh load shows the new key masked and the cleared password empty.
  await openSettings(page);
  const reloaded = secretFields(page);
  await expect(reloaded.geminiKey).toHaveValue(MASK);
  await expect(reloaded.remotePassword).toHaveValue("");

  expect(realConsoleErrors()).toEqual([]);
});
