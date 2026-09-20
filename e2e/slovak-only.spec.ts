import { test, expect, Page } from "@playwright/test";

// #194 ROUND 3c (item H) — the operator UI is Slovak on EVERY page. This asserts
// that NONE of the audit's English UI-chrome strings appears as visible text
// inside the page content (`main.content`) on Dashboard, Live, Lyrics, Dabing
// and Settings. The navbar tabs and the shared HealthBar sit OUTSIDE
// `main.content` and carry only product/technical names (OBS / NDI / Resolume /
// SongPlayer / WS / LAN / genlock LOCKED·DEGRADED·UNLOCKED·GENLOCK OFF), which
// are the allowed exceptions — so scoping the scan to `main.content` is what
// keeps those exceptions out of scope.
//
// The check uses EXACT-text matching so a data value (a song title, a playlist
// name) that merely CONTAINS an English word never trips it — only a chrome
// element whose whole text IS one of these banned strings fails.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

// The audit's English UI strings (headings / buttons / labels / th / options /
// placeholders) that ROUND 3c translated to Slovak — every one must now be gone.
const BANNED: string[] = [
  "Song",
  "Artist",
  "Cached",
  "Normalized",
  "Play",
  "Save",
  "Cancel",
  "Details",
  "Reprocess",
  "Add",
  "Delete",
  "Import",
  "Importing…",
  "Manual",
  "New",
  "Stale",
  "Currently processing",
  "Lyrics Pipeline",
  "Nothing playing",
  "No lyrics loaded",
  "Set list",
  "Songs",
  "Save Settings",
  "Password",
  "API Key",
  "Directory",
  "Reprocess all stale",
  "Clear manual queue",
  "Add Host",
  "Downloads",
  "Loading...",
  "Source:",
  "Raw audit log",
  "+ Add",
  "Enabled",
];

// The navigation tabs (outside `main.content`) — translated in the round-3c
// review follow-up: Prehľad · Naživo · Texty · Dabing · Nastavenia.
const BANNED_NAV: string[] = ["Dashboard", "Live", "Lyrics", "Settings"];

// Banned English placeholder texts (the two import boxes + the resolume form).
const BANNED_PLACEHOLDERS: string[] = [
  "Paste YouTube URL (e.g. https://youtu.be/…)",
  "Name",
  "IP address",
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/fixture", { data: { mode: "default" } });
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

async function assertSlovakOnly(page: Page) {
  const main = page.locator("main.content");
  await expect(main).toBeVisible({ timeout: 10000 });
  for (const term of BANNED) {
    await expect(
      main.getByText(term, { exact: true }),
      `English UI string "${term}" must not appear`,
    ).toHaveCount(0);
  }
  // The nav tabs are Slovak too (exact match on each tab's own text).
  const navLabels = await page
    .locator("nav.navbar button")
    .evaluateAll((els) => els.map((e) => (e.textContent || "").trim()));
  expect(navLabels.length, "the five nav tabs render").toBe(5);
  for (const banned of BANNED_NAV) {
    expect(navLabels, `English nav tab "${banned}" must not appear`).not.toContain(banned);
  }
  // No English placeholders on any input within the page content.
  const placeholders = await main.locator("[placeholder]").evaluateAll(
    (els) => els.map((e) => (e as HTMLInputElement).placeholder),
  );
  for (const banned of BANNED_PLACEHOLDERS) {
    expect(placeholders, `English placeholder "${banned}" must not appear`).not.toContain(
      banned,
    );
  }
}

test.describe("#194 H: the operator UI is Slovak on every page", () => {
  test("Dashboard has no English UI chrome", async ({ page }) => {
    await page.goto("/");
    await assertSlovakOnly(page);
  });

  test("Live has no English UI chrome", async ({ page }) => {
    await page.goto("/live");
    // Open the collapsible add panel so the ImportBox + catalog are scanned too.
    await page.locator(".live-add-toggle").click();
    await assertSlovakOnly(page);
  });

  test("Lyrics has no English UI chrome", async ({ page }) => {
    await page.goto("/lyrics");
    await assertSlovakOnly(page);
  });

  test("Dabing has no English UI chrome", async ({ page }) => {
    await page.goto("/dabing");
    await assertSlovakOnly(page);
  });

  test("Settings has no English UI chrome", async ({ page }) => {
    await page.goto("/settings");
    await assertSlovakOnly(page);
  });
});
