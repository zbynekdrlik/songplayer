import { test, expect, type Page } from "@playwright/test";

// #223 S13: the Nastavenia "Video: sťahovanie a 4K" fieldset — the download
// cap select ("" = Automaticky), GPU decoding and the in-place 4K upgrade,
// saved by the form's own Save, each ONLY when changed (#229's rule: a tab
// opened before a change elsewhere never sends the old value back). Its
// status line reads GET /api/v1/video-upgrade. A real user opens the
// Settings page, picks, clicks, saves and reloads; the spec asserts the
// visible result AND the PATCH bodies. Zero console errors is each test's
// last assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const VIDEO_KEYS = ["max_resolution", "video_hw_decode", "video_upgrade_enabled"];

let consoleMessages: string[] = [];

function realConsoleErrors(): string[] {
  return consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

// Open Nastavenia and wait until the loaded settings are IN THE FORM (the
// fixture's `gemini-2.5-flash` differs from the form default).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-video"]')).toBeVisible({ timeout: 10000 });
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

/** The video keys a PATCH body carries. */
function videoKeys(body: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(body).filter(([k]) => VIDEO_KEYS.includes(k)));
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

test("the defaults show automatic, both switches off, and the upgrade's status (#223 S13)", async ({
  page,
}) => {
  await openSettings(page);
  const select = page.locator('[data-testid="settings-max-resolution"]');
  await expect(select).toHaveValue("");
  await expect(select.locator("option:checked")).toHaveText(
    "Automaticky (4K s dekódovaním na GPU, inak 1440)",
  );
  await expect(select.locator("option")).toHaveText([
    "Automaticky (4K s dekódovaním na GPU, inak 1440)",
    "2160 (4K)",
    "1440",
    "1080",
    "720",
  ]);
  await expect(page.locator('[data-testid="settings-video-hw-decode"]')).not.toBeChecked();
  await expect(page.locator('[data-testid="settings-video-upgrade"]')).not.toBeChecked();
  await expect(page.locator('[data-testid="settings-video-upgrade-status"]')).toHaveText(
    "vylepšené 3 · bez vyššej kvality 4 · čaká 339 — vypnuté",
  );
  expect(realConsoleErrors()).toEqual([]);
});

test("a picked cap saves only the cap and survives a reload; a save with nothing changed sends no video key (#223 S13)", async ({
  page,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await save(page);
  expect(videoKeys(patches[0])).toEqual({});

  await page.locator('[data-testid="settings-max-resolution"]').selectOption("1080");
  await save(page);
  expect(videoKeys(patches[1])).toEqual({ max_resolution: "1080" });

  await page.reload();
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-max-resolution"]')).toHaveValue("1080");
  await page.locator('[data-testid="settings-max-resolution"]').selectOption("");
  await save(page);
  expect(videoKeys(patches[patches.length - 1])).toEqual({ max_resolution: "" });
  expect(realConsoleErrors()).toEqual([]);
});

test("GPU decoding and the 4K upgrade save only when clicked; the status follows the switch (#223 S13)", async ({
  page,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await page.locator('[data-testid="settings-video-hw-decode"]').check();
  await page.locator('[data-testid="settings-video-upgrade"]').check();
  await save(page);
  expect(videoKeys(patches[0])).toEqual({
    video_hw_decode: "true",
    video_upgrade_enabled: "true",
  });

  await page.reload();
  await openSettings(page);
  await expect(page.locator('[data-testid="settings-video-hw-decode"]')).toBeChecked();
  await expect(page.locator('[data-testid="settings-video-upgrade"]')).toBeChecked();
  await expect(page.locator('[data-testid="settings-video-upgrade-status"]')).toHaveText(
    "vylepšené 3 · bez vyššej kvality 4 · čaká 339",
  );
  await page.locator('[data-testid="settings-video-upgrade"]').uncheck();
  await save(page);
  expect(videoKeys(patches[patches.length - 1])).toEqual({ video_upgrade_enabled: "false" });
  expect(realConsoleErrors()).toEqual([]);
});

/** A cap set through the API that is no choice shows as itself, and a save
 * that does not touch it never sends it back. */
test("a cap set through the API shows as itself and is never overwritten by a save (#223 S13)", async ({
  page,
  request,
}) => {
  const set = await request.patch("/api/v1/settings", { data: { max_resolution: "1800" } });
  expect(set.status()).toBe(204);
  const patches = settingsPatches(page);
  await openSettings(page);
  const select = page.locator('[data-testid="settings-max-resolution"]');
  await expect(select).toHaveValue("1800");
  await expect(select.locator("option:checked")).toHaveText("1800 (vlastné)");
  await save(page);
  expect(videoKeys(patches[0])).toEqual({});
  expect(realConsoleErrors()).toEqual([]);
});
