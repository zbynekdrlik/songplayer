import { test, expect } from "@playwright/test";

// E2E coverage for the Nastavenia "Dabing" fieldset (#184 round C, round H step
// 2): the "Hlas dabingu" select (default `speaker` = the speaker's own voice) and
// the "Model dabingu" field (the Live Translate model is a setting), plus the
// Dabing row's voice line. Opens the real dashboard, interacts with the UI, and
// asserts BOTH the visible result AND the backend effect (the single PATCH
// /api/v1/settings the mock records), per e2e-real-user-testing.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const DEFAULT_MODEL = "gemini-3.5-live-translate-preview";

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/dabing-reset");
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("Nastavenia defaults to the speaker's voice + the default model, and one save PATCHes both (#184)", async ({
  page,
}) => {
  // Count every settings PATCH so we can prove exactly one fires on save.
  let settingsPatches = 0;
  let lastPatchBody: Record<string, unknown> | null = null;
  page.on("request", (req) => {
    if (
      req.method() === "PATCH" &&
      /\/api\/v1\/settings$/.test(req.url())
    ) {
      settingsPatches += 1;
      try {
        lastPatchBody = req.postDataJSON();
      } catch {
        lastPatchBody = null;
      }
    }
  });

  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();

  const select = page.locator('[data-testid="settings-dub-voice"]');
  await expect(select).toBeVisible({ timeout: 10000 });
  // Loads with the speaker's own voice from the settings fixture.
  await expect(select).toHaveValue("speaker");
  // The speaker's voice first, then the six catalogue voices.
  const options = select.locator("option");
  await expect(options).toHaveCount(7);
  await expect(options.first()).toHaveText("Hlas rečníka (odporúčané)");

  // No stored dub_model → the field shows the default model.
  const model = page.locator('[data-testid="settings-dub-model"]');
  await expect(model).toHaveValue(DEFAULT_MODEL);

  // Pin a prebuilt voice, switch the model, and save the form.
  await select.selectOption("Kore");
  await model.fill("gemini-4-live-translate");
  const patchPromise = page.waitForRequest(
    (req) =>
      req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url()),
  );
  await page.locator('.settings-form button[type="submit"]').click();
  await patchPromise;

  // Exactly ONE PATCH, carrying the chosen dub_voice AND dub_model.
  expect(settingsPatches).toBe(1);
  expect(lastPatchBody).not.toBeNull();
  expect(lastPatchBody!["dub_voice"]).toBe("Kore");
  expect(lastPatchBody!["dub_model"]).toBe("gemini-4-live-translate");

  // The save status confirms and the fields keep the chosen values.
  await expect(page.locator(".save-status")).toHaveText("Uložené");
  await expect(select).toHaveValue("Kore");
  await expect(model).toHaveValue("gemini-4-live-translate");
});

test("the Dabing row shows the voice: `hlas: rečník` for the speaker, `hlas: <name>` for a pinned voice (#184)", async ({
  page,
  request,
}) => {
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 344,
      title: "Morning Prayer & Devotion",
      dub_status: "ready",
      chain_state: "ready",
      stem_status: null,
      dub_file_path: "/c/morning_dub.flac",
      dub_voice: "speaker",
    },
  });
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 345,
      title: "Evening Prayer",
      dub_status: "ready",
      chain_state: "ready",
      stem_status: null,
      dub_file_path: "/c/evening_dub.flac",
      dub_voice: "Charon",
    },
  });

  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  const voices = page.locator(
    '[data-testid="dabing-list"] [data-testid="song-row"] [data-testid="dabing-row-voice"]',
  );
  await expect(voices).toHaveCount(2, { timeout: 5000 });
  // Newest first: 345 (pinned Charon) was added last, then 344 (the speaker).
  await expect(voices.nth(0)).toHaveText("hlas: Charon");
  await expect(voices.nth(1)).toHaveText("hlas: rečník");
});
