import { test, expect, type Page } from "@playwright/test";

// #244: Chrome filled the operator's saved login into Nastavenia. The
// settings form holds three password fields and no autofill hint, so the
// password manager read it as a sign-in form: the saved password went into
// the password fields and the saved user name into the text field before
// them (OBS "URL").
//
// The contract this spec pins, on the loaded Settings page:
// - every password field is `autocomplete="new-password"` (Chrome never
//   fills a stored credential there; `off` is ignored for passwords);
// - every other text / number field and every form is `autocomplete="off"`.
// Zero console errors is each test's last assertion.

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

test.afterEach(() => {
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

// Open Nastavenia and wait until the loaded settings are IN THE FORM (the
// fixture's `gemini-2.5-flash` differs from the form default).
async function openSettings(page: Page) {
  await page.goto("/");
  await page.locator('[data-testid="nav-settings"]').click();
  await expect(page.locator('[data-testid="settings-gemini-model"]')).toHaveValue(
    "gemini-2.5-flash",
    { timeout: 10000 },
  );
}

type Field = { type: string; autocomplete: string | null; label: string };

async function fields(page: Page): Promise<Field[]> {
  return page.locator("main.content input").evaluateAll((inputs) =>
    inputs
      .map((el) => el as HTMLInputElement)
      .filter((el) => !["checkbox", "radio", "range", "button", "submit", "hidden"].includes(el.type))
      .map((el) => ({
        type: el.type,
        autocomplete: el.getAttribute("autocomplete"),
        label:
          el.closest("label")?.textContent?.trim().slice(0, 40) ||
          el.getAttribute("data-testid") ||
          el.getAttribute("placeholder") ||
          "(no label)",
      })),
  );
}

test("no settings field invites Chrome's saved login (#244)", async ({ page }) => {
  await openSettings(page);
  const all = await fields(page);

  const passwords = all.filter((f) => f.type === "password");
  expect(passwords.map((f) => f.label)).toHaveLength(3);
  for (const f of passwords) {
    expect(f.autocomplete, `password field "${f.label}"`).toBe("new-password");
  }

  const others = all.filter((f) => f.type !== "password");
  expect(others.length).toBeGreaterThan(5);
  for (const f of others) {
    expect(f.autocomplete, `${f.type} field "${f.label}"`).toBe("off");
  }

  const forms = await page
    .locator("main.content form")
    .evaluateAll((list) => list.map((f) => f.getAttribute("autocomplete")));
  expect(forms.length).toBeGreaterThanOrEqual(2);
  for (const value of forms) {
    expect(value).toBe("off");
  }
});
