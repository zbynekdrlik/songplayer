import { test, expect, type Page } from "@playwright/test";

// #229 item C (the owner's ruling, 8.10.2026): Nastavenia "Platené AI" —
// the node's paid-AI switch `paid_ai_enabled`. A real user opens the
// Settings page, sees the switch on (nothing stored = on), clicks it off,
// clicks "Uložiť nastavenia" and reloads: the health bar on every page then
// names the node and says "Platené AI: vypnuté", with what waits for a
// peer's copy as its tooltip. Asserts the visible result AND the backend
// effect: one PATCH carrying `paid_ai_enabled: "false"`, the mock's
// GET /api/v1/status following it. Zero console errors is each test's last
// assertion.

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
// fixture's `gemini-2.5-flash` differs from the form default).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-paid-ai"]')).toBeVisible({
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

test("paid AI is on by default; switched off and saved, the health bar names the node and says it is off (#229 item C)", async ({
  page,
  request,
}) => {
  // The node's name, as the exchange settings would carry it.
  const named = await request.patch("/api/v1/settings", { data: { node_name: "pp" } });
  expect(named.status()).toBe(204);
  const patches = settingsPatches(page);
  await openSettings(page);

  const enabled = page.locator('[data-testid="settings-paid-ai-enabled"]');
  await expect(enabled).toBeChecked();
  await expect(page.locator('[data-testid="health-node"]')).toHaveText("Uzol: pp");
  await expect(page.locator('[data-testid="health-paid-ai"]')).toHaveCount(0);

  await enabled.click();
  await expect(enabled).not.toBeChecked();
  await save(page);
  expect(patches).toHaveLength(1);
  expect(patches[0]["paid_ai_enabled"]).toBe("false");

  // Backend effect: the status says paid AI is off.
  const status = await (await request.get("/api/v1/status")).json();
  expect(status.paid_ai_enabled).toBe(false);

  // A fresh load: the health bar reads the status again.
  await openSettings(page);
  const chip = page.locator('[data-testid="health-paid-ai"]');
  await expect(chip).toHaveText("Platené AI: vypnuté", { timeout: 10000 });
  await expect(chip).toHaveAttribute(
    "title",
    "Čaká, kým sa platené AI zapne: texty, metadáta (texty a metadáta berie medzitým od susedného uzla)",
  );
  await expect(page.locator('[data-testid="settings-paid-ai-enabled"]')).not.toBeChecked();

  // On again: the warning is gone after the next load.
  await page.locator('[data-testid="settings-paid-ai-enabled"]').click();
  await save(page);
  expect(patches[1]["paid_ai_enabled"]).toBe("true");
  await openSettings(page);
  await expect(page.locator('[data-testid="health-paid-ai"]')).toHaveCount(0);

  expect(realConsoleErrors()).toEqual([]);
});

test("a Nastavenia tab opened before paid AI went off never turns it on again (#229 item C)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-paid-ai-enabled"]')).toBeChecked();

  // Switched off elsewhere (the main session's API) while this tab is open.
  const off = await request.patch("/api/v1/settings", { data: { paid_ai_enabled: "false" } });
  expect(off.status()).toBe(204);

  // An unrelated edit saved in the stale tab.
  const model = page.locator('[data-testid="settings-gemini-model"]');
  await model.click();
  await model.fill("gemini-x");
  await save(page);
  expect(patches).toHaveLength(1);
  expect(patches[0]["gemini_model"]).toBe("gemini-x");
  expect(patches[0]).not.toHaveProperty("paid_ai_enabled");

  const status = await (await request.get("/api/v1/status")).json();
  expect(status.paid_ai_enabled).toBe(false);

  expect(realConsoleErrors()).toEqual([]);
});

test("a mangled paid-AI switch is refused whole, as the server does (#229 item C)", async ({
  request,
}) => {
  const refused = await request.patch("/api/v1/settings", {
    data: { paid_ai_enabled: "nope", gemini_model: "m2" },
  });
  expect(refused.status()).toBe(400);
  expect(await refused.text()).toBe("paid_ai_enabled must be true or false");
  const settings = await (await request.get("/api/v1/settings")).json();
  expect(settings.gemini_model).toBe("gemini-2.5-flash");
  expect(settings.paid_ai_enabled).toBeUndefined();
});
