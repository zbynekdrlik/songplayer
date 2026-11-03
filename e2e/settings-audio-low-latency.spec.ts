import { test, expect, type Page } from "@playwright/test";

// #289: Nastavenia "Zvukové výstupy" — the switch `audio_low_latency`
// ("Nízka latencia zvuku (~15 ms)"). Off by default; a real user ticks it
// and saves the section: ONE PATCH carries `audio_low_latency: "true"`, the
// value survives a reload, and GET /api/v1/program reports it. A save that
// does not touch the switch never sends it. A value other than true / false
// / "" is refused. Zero console errors is each test's last assertion.

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

// Wait until the LOADED settings are in the page (`sp-ui-frontend.md`).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-audio-outputs"]')).toBeVisible({ timeout: 10000 });
  await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue("gemini-2.5-flash", {
    timeout: 10000,
  });
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

async function saveOutputs(page: Page) {
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");
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

test("the low-latency switch is off, ticked and saved it is sent once and survives a reload (#289)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  const box = page.locator('[data-testid="settings-audio-low-latency"]');
  await expect(box).not.toBeChecked();
  const label = page.locator('label:has([data-testid="settings-audio-low-latency"])');
  await expect(label).toContainText("Nízka latencia zvuku (~15 ms)");
  await expect(label).toHaveAttribute("title", /65 ms.*camera-box/);
  let program = await (await request.get("/api/v1/program")).json();
  expect(program.audio_low_latency).toBe(false);

  await box.check();
  await saveOutputs(page);
  expect(patches).toHaveLength(1);
  expect(patches[0]["audio_low_latency"]).toBe("true");
  program = await (await request.get("/api/v1/program")).json();
  expect(program.audio_low_latency).toBe(true);

  // "Uložené" shows only while the switch is what was saved.
  const message = page.locator('[data-testid="audio-outputs-message"]');
  await box.uncheck();
  await expect(message).not.toHaveText("Uložené");
  await box.check();
  await expect(message).toHaveText("Uložené");

  await page.reload();
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-audio-low-latency"]')).toBeChecked();

  // A save that does not touch the switch never sends it.
  await page.locator('[data-testid="settings-audio-network-rate"]').selectOption("96000");
  await saveOutputs(page);
  expect(patches).toHaveLength(2);
  expect(patches[1]).not.toHaveProperty("audio_low_latency");

  // Unticked and saved: "false", and the program reads off again.
  await page.locator('[data-testid="settings-audio-low-latency"]').uncheck();
  await saveOutputs(page);
  expect(patches).toHaveLength(3);
  expect(patches[2]["audio_low_latency"]).toBe("false");
  program = await (await request.get("/api/v1/program")).json();
  expect(program.audio_low_latency).toBe(false);

  expect(realConsoleErrors()).toEqual([]);
});

test("a save of the outputs with the switch untouched sends no switch (#289)", async ({ page }) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await page.locator('[data-testid="settings-audio-network-rate"]').selectOption("96000");
  await saveOutputs(page);
  expect(patches).toHaveLength(1);
  expect(Object.keys(patches[0]).sort()).toEqual(["audio_network_rate", "audio_outputs"]);
  expect(realConsoleErrors()).toEqual([]);
});

test("the settings API refuses a switch that is not true, false or empty (#289)", async ({ request }) => {
  const refused = await request.patch("/api/v1/settings", {
    data: { audio_low_latency: "yes", gemini_model: "m2" },
  });
  expect(refused.status()).toBe(400);
  expect(await refused.text()).toBe("audio_low_latency must be true or false");
  const settings = await (await request.get("/api/v1/settings")).json();
  expect(settings.audio_low_latency).toBeUndefined();
  expect(settings.gemini_model).not.toBe("m2");
  const ok = await request.patch("/api/v1/settings", { data: { audio_low_latency: " TRUE " } });
  expect(ok.status()).toBe(204);
  expect((await (await request.get("/api/v1/settings")).json()).audio_low_latency).toBe("true");
});
