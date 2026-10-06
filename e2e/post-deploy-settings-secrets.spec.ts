/**
 * Post-deploy gate: the box's settings API shows every secret masked (#229).
 *
 * #229 lane 1 acceptance: "curl …/api/v1/settings shows ******** for every
 * secret". This spec reads the REAL deployed server and is READ-ONLY: it
 * never PATCHes a setting and never presses "Uložiť nastavenia".
 *
 * - `GET /api/v1/settings`: every secret setting reads `""` (nothing stored)
 *   or the mask `********`, never a value. A secret setting is one on the
 *   list or one named like one (`_key` / `_token` / `_password` /
 *   `_secret`, so a retired credential still in the box's database counts
 *   too) — the rule mirrors `sp_core::config::is_secret_setting`. A failure
 *   names the key, never the value. (`peers`, the exchange's peer list, masks
 *   the secrets INSIDE its JSON; this gate does not read into it.)
 * - `gemini_api_key` must read the mask: CI's "Seed settings" step stores a
 *   key on the box when none is there.
 * - The Nastavenia form's Gemini "API kľúč" field shows the mask in a
 *   password input.
 * - `GET /api/v1/exchange/status` (merged into the app's router) answers 200,
 *   the box's exchange settings hold (`config_error` null), `serving` is a
 *   boolean and `peers` a list. A failure names the field, never a value.
 *
 * Zero console errors is the last assertion (the same benign filters as
 * `post-deploy-flac.spec.ts`).
 */

import { test, expect } from "@playwright/test";

const SECRET_MASK = "********";

/** `sp_core::config::SECRET_SETTINGS`. */
const SECRET_SETTINGS = [
  "gemini_api_key",
  "genius_access_token",
  "obs_websocket_password",
  "peer_api_key",
  "remote_ws_password",
];

/** `sp_core::config::SECRET_SETTING_SUFFIXES`. */
const SECRET_SETTING_SUFFIXES = ["_key", "_token", "_password", "_secret"];

/** `sp_core::config::is_secret_setting`. */
function isSecretSetting(key: string): boolean {
  return (
    SECRET_SETTINGS.includes(key) ||
    SECRET_SETTING_SUFFIXES.some((suffix) => key.endsWith(suffix))
  );
}

// No trace for this gate: a trace records every response body, so the trace
// of a FAILED run — the one CI uploads, from a public repository — would carry
// the very secret the gate caught unmasked. The failure message names the key,
// which is the evidence needed. (A trace option forces a worker, so it must be
// top-level in the file.)
test.use({ trace: "off" });

test.describe("settings secrets post-deploy verification (#229)", () => {
  let consoleErrors: string[] = [];

  test.beforeEach(({ page }) => {
    consoleErrors = [];
    page.on("console", (msg) => {
      const type = msg.type();
      if (type === "error" || type === "warning") {
        const text = msg.text();
        // Chromium emits a benign SRI warning on the preloaded WASM bundle.
        if (/integrity.*attribute.*ignored/i.test(text)) return;
        // #178: the on-demand preview <video> opens a WebSocket; a page that
        // navigates/tears down mid-handshake makes Chrome log this benign
        // "closed before established" warning. Not a product error.
        if (/WebSocket is closed before the connection is established/i.test(text))
          return;
        consoleErrors.push(`[${type}] ${text}`);
      }
    });
  });

  test("the settings API shows every secret masked and the form shows the mask", async ({
    page,
    request,
  }) => {
    const resp = await request.get("/api/v1/settings");
    expect(resp.status()).toBe(200);
    const settings = (await resp.json()) as Record<string, unknown>;

    const secretKeys = Object.keys(settings).filter(isSecretSetting).sort();
    for (const key of secretKeys) {
      const value = settings[key];
      // The message names the key only: a failure must never print a secret.
      expect(
        value === "" || value === SECRET_MASK,
        `GET /api/v1/settings shows the secret setting "${key}" unmasked`,
      ).toBe(true);
    }
    expect(
      settings["gemini_api_key"] === SECRET_MASK,
      "gemini_api_key must be stored (CI's Seed settings) and read as the mask",
    ).toBe(true);
    console.log(
      `settings secrets check: ${secretKeys.length} secret settings, all masked or empty (${secretKeys.join(", ")})`,
    );

    // The Nastavenia form shows the stored key as the mask, in a password field.
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const geminiKey = page.getByLabel("API kľúč");
    await expect(geminiKey).toHaveValue(SECRET_MASK, { timeout: 10_000 });
    await expect(geminiKey).toHaveAttribute("type", "password");

    expect(consoleErrors).toEqual([]);
  });

  test("the exchange status answers and the box's exchange settings hold", async ({
    request,
  }) => {
    const resp = await request.get("/api/v1/exchange/status");
    expect(resp.status(), "GET /api/v1/exchange/status").toBe(200);
    const status = (await resp.json()) as Record<string, unknown>;

    // The messages name the field only: a failure never prints a value.
    expect(
      status["config_error"] === null,
      "the box's exchange settings do not hold (config_error is set)",
    ).toBe(true);
    expect(typeof status["serving"] === "boolean", "serving is not a boolean").toBe(
      true,
    );
    expect(Array.isArray(status["peers"]), "peers is not a list").toBe(true);
    console.log(
      `exchange status: serving=${String(status["serving"])}, ${(status["peers"] as unknown[]).length} peer(s)`,
    );

    expect(consoleErrors).toEqual([]);
  });
});
