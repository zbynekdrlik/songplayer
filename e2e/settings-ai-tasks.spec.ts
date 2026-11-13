import { test, expect, type Page } from "@playwright/test";

// #321: Nastavenia "Textové AI (OpenRouter)" — per text AI task its
// OpenRouter model (blank = the newest Gemini); every task goes to
// OpenRouter (the Claude proxy is deleted, no provider choice is offered).
// A real user opens the Settings page, sets the translation's model, saves
// and reloads: the page shows it again, and ONE PATCH carried exactly the
// changed setting (never an untouched task's value). Zero console errors is
// each test's last assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const TASKS = ["translate", "register", "metadata", "description", "cleanup", "spotify"];

let consoleMessages: string[] = [];

function realConsoleErrors(): string[] {
  return consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

// Open Nastavenia and wait until the loaded settings are IN THE FORM (the
// fixture's `gemini-2.5-flash` differs from the form default).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-ai-tasks"]')).toBeVisible({
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

function aiKeys(patch: Record<string, unknown>): string[] {
  return Object.keys(patch)
    .filter((k) => k.startsWith("ai_provider_") || k.startsWith("ai_model_"))
    .sort();
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

test("every task shows its OpenRouter model; the translation's model is saved and shown again (#321)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  // No provider choice is left: OpenRouter is the only path.
  await expect(page.locator('[data-testid^="settings-ai-provider-"]')).toHaveCount(0);
  for (const task of TASKS) {
    const model = page.locator(`[data-testid="settings-ai-model-${task}"]`);
    await expect(model).toHaveValue("");
    await expect(model).toHaveAttribute("placeholder", "google/gemini-4-hydrogen");
    await expect(model).toBeEnabled();
  }

  const model = page.locator('[data-testid="settings-ai-model-translate"]');
  await model.click();
  await model.fill(" google/gemini-4-carbon ");
  await save(page);

  // Backend effect: one PATCH with exactly the changed setting, trimmed.
  expect(patches).toHaveLength(1);
  expect(aiKeys(patches[0])).toEqual(["ai_model_translate"]);
  expect(patches[0]["ai_model_translate"]).toBe("google/gemini-4-carbon");
  const stored = await (await request.get("/api/v1/settings")).json();
  expect(stored.ai_model_translate).toBe("google/gemini-4-carbon");
  expect(stored.ai_model_register).toBeUndefined();

  // A fresh load shows it again.
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-ai-model-translate"]')).toHaveValue(
    "google/gemini-4-carbon",
  );
  await expect(page.locator('[data-testid="settings-ai-model-register"]')).toHaveValue("");

  // Cleared back to blank (= the default): sent as blank.
  await page.locator('[data-testid="settings-ai-model-translate"]').fill("");
  await save(page);
  expect(patches).toHaveLength(2);
  expect(aiKeys(patches[1])).toEqual(["ai_model_translate"]);
  expect(patches[1]["ai_model_translate"]).toBe("");

  expect(realConsoleErrors()).toEqual([]);
});

test("a Nastavenia tab opened before a model was changed elsewhere never changes it back (#321)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  // Changed elsewhere (the API) while this tab is open.
  const changed = await request.patch("/api/v1/settings", {
    data: { ai_model_register: "google/gemini-4-carbon" },
  });
  expect(changed.status()).toBe(204);

  // An unrelated edit saved in the stale tab.
  const gemini = page.locator('[data-testid="settings-gemini-model"]');
  await gemini.click();
  await gemini.fill("gemini-x");
  await save(page);
  expect(patches).toHaveLength(1);
  expect(aiKeys(patches[0])).toEqual([]);

  const stored = await (await request.get("/api/v1/settings")).json();
  expect(stored.ai_model_register).toBe("google/gemini-4-carbon");

  expect(realConsoleErrors()).toEqual([]);
});
