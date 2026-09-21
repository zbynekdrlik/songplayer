import { test, expect } from "@playwright/test";

// E2E coverage for #184 round C: the Nastavenia "Hlas dabingu" select and the
// Dabing row's `hlas: <voice>` line. Opens the real dashboard, interacts with the
// UI, and asserts BOTH the visible result AND the backend effect (the single
// PATCH /api/v1/settings the mock records), per e2e-real-user-testing.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

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

test("Nastavenia shows the voice select defaulting to Charon and one save PATCHes dub_voice (#184)", async ({
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
  // Loads with the default voice from the settings fixture.
  await expect(select).toHaveValue("Charon");
  // All six catalogue voices are offered.
  await expect(select.locator("option")).toHaveCount(6);

  // Change the voice and save the form.
  await select.selectOption("Kore");
  const patchPromise = page.waitForRequest(
    (req) =>
      req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url()),
  );
  await page.locator('.settings-form button[type="submit"]').click();
  await patchPromise;

  // Exactly ONE PATCH, carrying the chosen dub_voice.
  expect(settingsPatches).toBe(1);
  expect(lastPatchBody).not.toBeNull();
  expect(lastPatchBody!["dub_voice"]).toBe("Kore");

  // The save status confirms and the select keeps the chosen value.
  await expect(page.locator(".save-status")).toHaveText("Uložené");
  await expect(select).toHaveValue("Kore");
});

test("the Dabing row shows the pinned voice as `hlas: <voice>` (#184)", async ({
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
      dub_voice: "Charon",
    },
  });

  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  const row = page
    .locator('[data-testid="dabing-list"] [data-testid="song-row"]')
    .first();
  await expect(row).toBeVisible({ timeout: 5000 });
  await expect(row.locator('[data-testid="dabing-row-voice"]')).toHaveText(
    "hlas: Charon",
  );
});
