import { test, expect, type Page } from "@playwright/test";

// #233: Nastavenia "Zvukové výstupy" — the program's audio outputs as ONE
// list (`audio_outputs`) and the network rate. A real user adds a VBAN
// output, fills it, saves; the save is ONE PATCH carrying exactly the two
// keys; the values survive a reload and the output shows its live state from
// GET /api/v1/program. A bad entry is refused in Slovak before anything is
// sent. Saving the OTHER settings keeps the outputs list (Review Focus 1),
// and neither section's save drops the other's unsaved edits. A stored row
// shows its OWN rate and format after a load (the selects' options are built
// after the value is set: every option carries `selected`). Zero console
// errors is each test's last assertion.

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

// Wait until the LOADED settings are in the page (`sp-ui-frontend.md`): the
// fixture's Gemini model differs from the form's default.
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-audio-outputs"]')).toBeVisible({ timeout: 10000 });
  await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue("gemini-2.5-flash", {
    timeout: 10000,
  });
}

// The two save handlers reached PAST their disabled buttons, so a test sees
// their own `loaded` guards (a click on a disabled button reaches no handler):
// - the outputs save: its button re-enabled by hand, then clicked (Leptos
//   re-applies `prop:disabled` only when `loaded` changes);
// - the form: `requestSubmit()` fires `submit` past the disabled button. It
//   runs constraint validation first, so the helper returns whether `submit`
//   really fired — a test asserts it, or an invalid default would make the
//   check pass without reaching the guard.
async function clickOutputsSavePastTheButton(page: Page) {
  await page.evaluate(() => {
    const button = document.querySelector('[data-testid="audio-outputs-save"]') as HTMLButtonElement;
    button.disabled = false;
    button.click();
  });
}

async function submitFormPastTheButton(page: Page): Promise<boolean> {
  return page.evaluate(() => {
    const form = document.querySelector("form.settings-form") as HTMLFormElement;
    let fired = false;
    form.addEventListener(
      "submit",
      () => {
        fired = true;
      },
      { once: true },
    );
    form.requestSubmit();
    return fired;
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

const TWO = JSON.stringify([
  {
    id: "out-1",
    name: "FOH",
    type: "vban",
    enabled: true,
    rate: 48000,
    delay_ms: 0,
    vban: { host: "fohabl.lan", port: 6980, stream_name: "sp-program", format: "int24" },
  },
  {
    id: "out-2",
    name: "lv1",
    type: "vban",
    enabled: true,
    rate: "network",
    delay_ms: 20,
    vban: { host: "lv1.lan", port: 6980, stream_name: "sp-program", format: "int16" },
  },
]);

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

test("an empty list: add, fill and save one VBAN output; it survives a reload with its live state (#233)", async ({
  page,
  request,
}) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await expect(page.locator('[data-testid="audio-output-row"]')).toHaveCount(0);
  await expect(page.locator('[data-testid="settings-audio-network-rate"]')).toHaveValue("48000");

  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  const row = page.locator('[data-testid="audio-output-row"]');
  await expect(row).toHaveCount(1);
  await expect(row).toHaveAttribute("data-id", "out-1");
  await expect(row.locator('[data-testid="audio-output-name"]')).toHaveValue("VBAN 1");
  await expect(row.locator('[data-testid="audio-output-rate"]')).toHaveValue("network");
  await expect(row.locator('[data-testid="audio-output-vban-port"]')).toHaveValue("6980");
  await expect(row.locator('[data-testid="audio-output-state"]')).toHaveText("neuložený");
  await row.locator('[data-testid="audio-output-vban-host"]').fill("dev1.lan");
  await row.locator('[data-testid="audio-output-rate"]').selectOption("96000");
  await page.locator('[data-testid="settings-audio-network-rate"]').selectOption("96000");
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");

  expect(patches).toHaveLength(1);
  expect(Object.keys(patches[0]).sort()).toEqual(["audio_network_rate", "audio_outputs"]);
  expect(JSON.parse(patches[0]["audio_outputs"] as string)).toEqual([
    {
      id: "out-1",
      name: "VBAN 1",
      type: "vban",
      enabled: true,
      rate: 96000,
      delay_ms: 0,
      vban: { host: "dev1.lan", port: 6980, stream_name: "sp-program", format: "int24" },
    },
  ]);
  expect(patches[0]["audio_network_rate"]).toBe("96000");

  // Backend effect: the program lists it.
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.outputs.map((o: { id: string; rate: number }) => [o.id, o.rate])).toEqual([["out-1", 96000]]);
  expect(program.audio_network_rate).toBe(96000);
  expect(program.vban).toBeUndefined();

  await page.reload();
  await openSettings(page);
  await expect(page.locator('[data-testid="audio-output-vban-host"]')).toHaveValue("dev1.lan");
  await expect(page.locator('[data-testid="audio-output-rate"]')).toHaveValue("96000");
  await expect(page.locator('[data-testid="audio-output-vban-format"]')).toHaveValue("int24");
  await expect(page.locator('[data-testid="settings-audio-network-rate"]')).toHaveValue("96000");
  await expect(page.locator('[data-testid="audio-output-state"]')).toContainText("beží", { timeout: 10000 });
  expect(realConsoleErrors()).toEqual([]);
});

test("a bad entry is refused in Slovak before anything is sent (#233)", async ({ page }) => {
  const patches = settingsPatches(page);
  await openSettings(page);
  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText(
    "Výstup 1 (out-1): pole „cieľ“ je prázdne",
  );
  expect(patches).toHaveLength(0);
  expect(realConsoleErrors()).toEqual([]);
});

test("an output is removed and another switched off; the save carries exactly the rest (#233)", async ({
  page,
  request,
}) => {
  const seeded = await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  expect(seeded.status()).toBe(204);
  const patches = settingsPatches(page);
  await openSettings(page);
  const rows = page.locator('[data-testid="audio-output-row"]');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0).locator('[data-testid="audio-output-name"]')).toHaveValue("FOH");
  // FOH's own values, neither of them its select's first option.
  await expect(rows.nth(0).locator('[data-testid="audio-output-rate"]')).toHaveValue("48000");
  await expect(rows.nth(0).locator('[data-testid="audio-output-vban-format"]')).toHaveValue("int24");
  await expect(rows.nth(1).locator('[data-testid="audio-output-rate"]')).toHaveValue("network");
  await expect(rows.nth(1).locator('[data-testid="audio-output-delay"]')).toHaveValue("20");
  await expect(rows.nth(1).locator('[data-testid="audio-output-vban-format"]')).toHaveValue("int16");
  await rows.nth(0).locator('[data-testid="audio-output-remove"]').click();
  await expect(rows).toHaveCount(1);
  await rows.nth(0).locator('[data-testid="audio-output-enabled"]').uncheck();
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");
  expect(patches).toHaveLength(1);
  const saved = JSON.parse(patches[0]["audio_outputs"] as string);
  expect(saved.map((e: { id: string; enabled: boolean }) => [e.id, e.enabled])).toEqual([["out-2", false]]);
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.outputs[0].state).toBe("disabled");
  await expect(page.locator('[data-testid="audio-output-state"]')).toContainText("vypnutý", { timeout: 10000 });
  expect(realConsoleErrors()).toEqual([]);
});

test("saving the other settings keeps the outputs list (#233)", async ({ page, request }) => {
  const seeded = await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  expect(seeded.status()).toBe(204);
  const patches = settingsPatches(page);
  await openSettings(page);
  // ONE `.save-status` on the page (the form's): the specs of the other
  // Nastavenia fields read it unscoped (Playwright strict mode).
  await expect(page.locator(".save-status")).toHaveCount(1);
  await expect(page.locator('[data-testid="audio-output-row"]')).toHaveCount(2);
  await page.locator('[data-testid="settings-gemini-model"]').fill("gemini-x");
  await page.getByRole("button", { name: "Uložiť nastavenia" }).click();
  await expect.poll(() => patches.length).toBe(1);
  expect(patches[0]).not.toHaveProperty("audio_outputs");
  expect(patches[0]).not.toHaveProperty("vban_targets");
  await expect(page.locator(".save-status").first()).toHaveText("Uložené");
  await expect(page.locator('[data-testid="audio-output-row"]')).toHaveCount(2);
  const stored = await (await request.get("/api/v1/settings")).json();
  expect(JSON.parse(stored.audio_outputs)).toHaveLength(2);
  expect(realConsoleErrors()).toEqual([]);
});

test("neither section's save drops the other's unsaved edits (#233)", async ({ page, request }) => {
  const seeded = await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  expect(seeded.status()).toBe(204);
  const patches = settingsPatches(page);
  await openSettings(page);
  const rows = page.locator('[data-testid="audio-output-row"]');
  await expect(rows).toHaveCount(2);

  // An output added but not saved survives a save of the form above.
  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  await expect(rows).toHaveCount(3);
  await rows.nth(2).locator('[data-testid="audio-output-vban-host"]').fill("dev1.lan");
  await page.locator('[data-testid="settings-gemini-model"]').fill("gemini-x");
  await page.getByRole("button", { name: "Uložiť nastavenia" }).click();
  await expect.poll(() => patches.length).toBe(1);
  await expect(page.locator(".save-status").first()).toHaveText("Uložené");
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(2).locator('[data-testid="audio-output-vban-host"]')).toHaveValue("dev1.lan");

  // A field of the form edited but not saved survives a save of the outputs.
  await page.locator('[data-testid="settings-gemini-model"]').fill("gemini-unsaved");
  await page.locator('[data-testid="audio-outputs-save"]').click();
  await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Uložené");
  expect(patches).toHaveLength(2);
  expect(Object.keys(patches[1]).sort()).toEqual(["audio_network_rate", "audio_outputs"]);
  expect(JSON.parse(patches[1]["audio_outputs"] as string)).toHaveLength(3);
  await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue("gemini-unsaved");
  await expect(rows).toHaveCount(3);
  expect(realConsoleErrors()).toEqual([]);
});

test("a saved entry the server does not run reads so, with the server's reason (#233)", async ({
  page,
  request,
}) => {
  const seeded = await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  expect(seeded.status()).toBe(204);
  await request.post("/__mock/outputs-skip", { data: { ids: ["out-2"] } });
  await openSettings(page);
  const rows = page.locator('[data-testid="audio-output-row"]');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0).locator('[data-testid="audio-output-state"]')).toContainText("beží", { timeout: 10000 });
  const skipped = rows.nth(1).locator('[data-testid="audio-output-state"]');
  await expect(skipped).toHaveText("uložený, nespustený", { timeout: 10000 });
  await expect(skipped).toHaveAttribute("title", "Hlásenie servera: entry 2 (id out-2): type must be vban");
  // A row added here and not saved yet is still "neuložený".
  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  await expect(rows.nth(2).locator('[data-testid="audio-output-state"]')).toHaveText("neuložený");
  expect(realConsoleErrors()).toEqual([]);
});

test("Nastavenia whose settings did not load saves nothing (#233)", async ({ page, request }) => {
  const patches = settingsPatches(page);
  await request.post("/__mock/fail-mode", { data: { kind: "settings", enabled: true } });
  try {
    await page.goto("/");
    await page.locator('[data-testid="nav-settings"]').click();
    await expect(page.locator('[data-testid="settings-audio-outputs"]')).toBeVisible({ timeout: 10000 });
    await expect(page.locator('[data-testid="audio-outputs-load-error"]')).toHaveText(
      "Nastavenia sa nenačítali — výstupy sa nedajú uložiť",
      { timeout: 10000 },
    );
    await expect(page.locator('[data-testid="audio-outputs-save"]')).toBeDisabled();
    await expect(page.getByRole("button", { name: "Uložiť nastavenia" })).toBeDisabled();
    await expect(page.locator(".save-status")).toHaveText("Nastavenia sa nenačítali — uloženie je vypnuté");
    // An output added in this state keeps the save disabled.
    await page.locator('[data-testid="audio-outputs-add-vban"]').click();
    const rows = page.locator('[data-testid="audio-output-row"]');
    await expect(rows).toHaveCount(1);
    await expect(page.locator('[data-testid="audio-outputs-save"]')).toBeDisabled();
    // Each handler's own guard: an EMPTY list passes `validate_list`, so only
    // the guard can stop the outputs save here.
    await rows.nth(0).locator('[data-testid="audio-output-remove"]').click();
    await expect(rows).toHaveCount(0);
    await clickOutputsSavePastTheButton(page);
    await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("");
    expect(await submitFormPastTheButton(page), "the form's submit handler ran").toBe(true);
    // A round trip through the page after the attempts, then: no PATCH.
    await page.locator('[data-testid="audio-outputs-add-vban"]').click();
    await expect(rows).toHaveCount(1);
    expect(patches).toHaveLength(0);
  } finally {
    await request.post("/__mock/fail-mode", { data: { kind: "settings", enabled: false } });
  }
  // The refused GET is the point of the test: the browser logs its 500.
  expect(realConsoleErrors().filter((m) => !/Failed to load resource.*500/.test(m))).toEqual([]);
});

test("Nastavenia saves nothing while its settings are still loading (#233)", async ({ page }) => {
  const patches = settingsPatches(page);
  // Hold the page's GET /api/v1/settings until the test releases it.
  let release = () => {};
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  await page.route("**/api/v1/settings", async (route) => {
    if (route.request().method() === "GET") await held;
    await route.continue();
  });
  try {
    await page.goto("/");
    await page.locator('[data-testid="nav-settings"]').click();
    await expect(page.locator('[data-testid="settings-audio-outputs"]')).toBeVisible({ timeout: 10000 });
    const saveOutputs = page.locator('[data-testid="audio-outputs-save"]');
    const saveForm = page.getByRole("button", { name: "Uložiť nastavenia" });
    await expect(saveOutputs).toBeDisabled();
    await expect(saveForm).toBeDisabled();
    await expect(page.locator('[data-testid="audio-outputs-load-error"]')).toHaveCount(0);
    // Both handlers reached past their disabled buttons while the load runs
    // (no row yet: an empty list, so only the guard can stop the save).
    await clickOutputsSavePastTheButton(page);
    await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("");
    expect(await submitFormPastTheButton(page), "the form's submit handler ran").toBe(true);
    release();
    await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue("gemini-2.5-flash", {
      timeout: 10000,
    });
    await expect(saveOutputs).toBeEnabled();
    await expect(saveForm).toBeEnabled();
    expect(patches, "nothing was sent while the settings loaded").toHaveLength(0);
  } finally {
    release();
    await page.unroute("**/api/v1/settings");
  }
  expect(realConsoleErrors()).toEqual([]);
});

test("an output added after one was removed takes a new id (#233)", async ({ page, request }) => {
  const seeded = await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  expect(seeded.status()).toBe(204);
  await openSettings(page);
  const rows = page.locator('[data-testid="audio-output-row"]');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(1).locator('[data-testid="audio-output-state"]')).toContainText("beží", { timeout: 10000 });
  // out-2 is removed but not saved: it still runs under its id.
  await rows.nth(1).locator('[data-testid="audio-output-remove"]').click();
  await expect(rows).toHaveCount(1);
  await page.locator('[data-testid="audio-outputs-add-vban"]').click();
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(1)).toHaveAttribute("data-id", "out-3");
  await expect(rows.nth(1).locator('[data-testid="audio-output-state"]')).toHaveText("neuložený");
  expect(realConsoleErrors()).toEqual([]);
});

test("a refused save keeps the edits and stores nothing (#233)", async ({ page, request }) => {
  const seeded = await request.patch("/api/v1/settings", { data: { audio_outputs: TWO } });
  expect(seeded.status()).toBe(204);
  await openSettings(page);
  // The server refuses this PATCH (as it refuses a list it cannot take).
  await page.route("**/api/v1/settings", async (route) => {
    if (route.request().method() === "PATCH") {
      await route.fulfill({ status: 400, contentType: "text/plain", body: "entry 2 (id out-2): vban.host is empty" });
      return;
    }
    await route.continue();
  });
  try {
    const rows = page.locator('[data-testid="audio-output-row"]');
    await expect(rows).toHaveCount(2);
    await rows.nth(1).locator('[data-testid="audio-output-delay"]').fill("30");
    await page.locator('[data-testid="audio-outputs-save"]').click();
    await expect(page.locator('[data-testid="audio-outputs-message"]')).toHaveText("Chyba pri ukladaní");
    await expect(rows.nth(1).locator('[data-testid="audio-output-delay"]')).toHaveValue("30");
  } finally {
    await page.unroute("**/api/v1/settings");
  }
  // The refused PATCH is the point of the test: the browser logs its 400.
  expect(realConsoleErrors().filter((m) => !/Failed to load resource.*400/.test(m))).toEqual([]);
});
