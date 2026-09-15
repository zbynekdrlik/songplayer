import { test, expect } from "@playwright/test";

// E2E coverage for #14: the dashboard karaoke control drives
// GET/POST /api/v1/karaoke. This spec opens the real dashboard, changes the
// mode dropdown + the vocal-gain slider, and verifies BOTH the POST request the
// UI sent AND the value the mock recorded (the backend effect), per
// e2e-real-user-testing.md. The actual NDI audio band-drop is verified on the
// wall by the supervisor (a browser cannot observe NDI audio).

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("karaoke control loads current state + stem progress (#14)", async ({
  page,
}) => {
  await page.goto("/");
  const panel = page.locator(".karaoke-control");
  await expect(panel).toBeVisible({ timeout: 10000 });

  // Mode reflects the mock's current state (full_mix).
  await expect(page.locator('[data-testid="karaoke-mode"]')).toHaveValue(
    "full_mix",
  );
  // Stem progress line shows the mock's counts (5 done, 2 pending).
  await expect(
    page.locator('[data-testid="karaoke-stem-progress"]'),
  ).toContainText("5");
});

test("selecting Instrumental-only POSTs the mode and the mock records it (#14)", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });

  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/karaoke") && req.method() === "POST",
  );
  await page
    .locator('[data-testid="karaoke-mode"]')
    .selectOption("instrumental_only");

  const req = await postPromise;
  const body = JSON.parse(req.postData() ?? "{}");
  expect(body.mode).toBe("instrumental_only");

  // Backend effect: the mock recorded the mode the UI sent.
  const recorded = await page.request.get("/__mock/karaoke-last");
  expect((await recorded.json()).mode).toBe("instrumental_only");
});

test("KaraokeLow enables the vocal-gain slider and POSTs the gain (#14)", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });

  const slider = page.locator('[data-testid="karaoke-vocal-gain"]');
  // The slider is disabled until KaraokeLow is selected.
  await expect(slider).toBeDisabled();

  await page
    .locator('[data-testid="karaoke-mode"]')
    .selectOption("karaoke_low");
  await expect(slider).toBeEnabled();

  // Drag the slider to 20% and release; the UI POSTs the gain as 0.2.
  // The gain is an f32 in the Rust UI, so `serde_json` serializes 20/100 as the
  // f32-widened f64 0.20000000298023224 — never bit-exactly 0.2. Match with the
  // same tolerance the backend-effect assertion below uses (toBeCloseTo(0.2, 5));
  // a strict `=== 0.2` here silently never matches and the wait times out.
  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/karaoke") &&
      req.method() === "POST" &&
      Math.abs((JSON.parse(req.postData() ?? "{}").vocal_gain ?? NaN) - 0.2) <
        1e-6,
  );
  await slider.fill("20");
  await slider.dispatchEvent("change");
  await postPromise;

  // Backend effect: the mock recorded the lowered gain.
  const recorded = await page.request.get("/__mock/karaoke-last");
  const body = await recorded.json();
  expect(body.mode).toBe("karaoke_low");
  expect(body.vocal_gain).toBeCloseTo(0.2, 5);
});
