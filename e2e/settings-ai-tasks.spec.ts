import { test, expect, type Page } from "@playwright/test";

// #321: Nastavenia "Textové AI (OpenRouter)" — per text AI task where its
// calls go (Claude through the proxy by default, or OpenRouter) and the
// OpenRouter model (blank = the newest Gemini). A real user opens the
// Settings page, sees every task on the proxy, switches the translation to
// OpenRouter with a model, saves and reloads: the page shows the choice
// again, and ONE PATCH carried exactly the two changed settings (never an
// untouched task's value). Zero console errors is each test's last
// assertion.

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

test("every task starts on the proxy; the translation switched to OpenRouter is saved and shown again (#321)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  for (const task of TASKS) {
    await expect(page.locator(`[data-testid="settings-ai-provider-${task}"]`)).toHaveValue(
      "claude_proxy",
    );
    const model = page.locator(`[data-testid="settings-ai-model-${task}"]`);
    await expect(model).toHaveValue("");
    await expect(model).toHaveAttribute("placeholder", "google/gemini-4-hydrogen");
    // The model applies to OpenRouter only: off while the task is on the proxy.
    await expect(model).toBeDisabled();
  }

  await page
    .locator('[data-testid="settings-ai-provider-translate"]')
    .selectOption("openrouter");
  const model = page.locator('[data-testid="settings-ai-model-translate"]');
  await expect(model).toBeEnabled();
  await model.click();
  await model.fill("google/gemini-4-carbon");
  await save(page);

  // Backend effect: one PATCH with exactly the two changed settings.
  expect(patches).toHaveLength(1);
  expect(aiKeys(patches[0])).toEqual(["ai_model_translate", "ai_provider_translate"]);
  expect(patches[0]["ai_provider_translate"]).toBe("openrouter");
  expect(patches[0]["ai_model_translate"]).toBe("google/gemini-4-carbon");
  const stored = await (await request.get("/api/v1/settings")).json();
  expect(stored.ai_provider_translate).toBe("openrouter");
  expect(stored.ai_model_translate).toBe("google/gemini-4-carbon");
  expect(stored.ai_provider_register).toBeUndefined();

  // A fresh load shows the choice (`openrouter` is not the first option, so
  // a select that lost its value would read `claude_proxy`).
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-ai-provider-translate"]')).toHaveValue(
    "openrouter",
  );
  await expect(page.locator('[data-testid="settings-ai-model-translate"]')).toHaveValue(
    "google/gemini-4-carbon",
  );
  await expect(page.locator('[data-testid="settings-ai-provider-register"]')).toHaveValue(
    "claude_proxy",
  );

  // Back to the proxy: only the provider is sent, the model stays stored.
  await page
    .locator('[data-testid="settings-ai-provider-translate"]')
    .selectOption("claude_proxy");
  await save(page);
  expect(patches).toHaveLength(2);
  expect(aiKeys(patches[1])).toEqual(["ai_provider_translate"]);
  expect(patches[1]["ai_provider_translate"]).toBe("claude_proxy");

  expect(realConsoleErrors()).toEqual([]);
});

test("a Nastavenia tab opened before a task was switched elsewhere never switches it back (#321)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);

  // Switched elsewhere (the API) while this tab is open.
  const switched = await request.patch("/api/v1/settings", {
    data: { ai_provider_register: "openrouter" },
  });
  expect(switched.status()).toBe(204);

  // An unrelated edit saved in the stale tab.
  const gemini = page.locator('[data-testid="settings-gemini-model"]');
  await gemini.click();
  await gemini.fill("gemini-x");
  await save(page);
  expect(patches).toHaveLength(1);
  expect(aiKeys(patches[0])).toEqual([]);

  const stored = await (await request.get("/api/v1/settings")).json();
  expect(stored.ai_provider_register).toBe("openrouter");

  expect(realConsoleErrors()).toEqual([]);
});

test("an unknown provider is refused whole, as the server does (#321)", async ({ request }) => {
  const refused = await request.patch("/api/v1/settings", {
    data: { ai_provider_metadata: "gemini", gemini_model: "m2" },
  });
  expect(refused.status()).toBe(400);
  expect(await refused.text()).toBe("ai_provider_metadata must be claude_proxy or openrouter");
  const settings = await (await request.get("/api/v1/settings")).json();
  expect(settings.gemini_model).toBe("gemini-2.5-flash");
  expect(settings.ai_provider_metadata).toBeUndefined();
});
