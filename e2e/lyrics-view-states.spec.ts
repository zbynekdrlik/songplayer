import { test, expect } from "@playwright/test";

// #198 item 3: the shared LyricsView must render its LOADING and ERROR states
// through the shared StateBlock (`state-loading` / `state-error`), like every
// other surface — not fold loading, a failed fetch and genuinely-no-lyrics all
// into the single `lyrics-empty`. The genuine-empty (204) case keeps the
// `lyrics-empty` testid and is covered by item 9's own spec.
//
// The Dashboard `/` renders the shared Player, which nests
// `LyricsView(playlist_id)`; playlist 1 plays on the mock, so once its
// now-playing arrives the LyricsView fetches `/api/v1/videos/{id}/lyrics`. The
// mock's `/__mock/lyrics-mode` drives that endpoint's response.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
  // The error test DELIBERATELY forces a 500 on the lyrics endpoint — the
  // browser's own "Failed to load resource … 500" for that request is the point
  // of the test, not a bug (sp-ui-frontend.md #194 r2 trap).
  /Failed to load resource.*(500|Internal Server Error)/,
  /\/api\/v1\/videos\/.*\/lyrics/,
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

test.afterEach(async ({ request }) => {
  // Reset the (global) lyrics mode so later specs see the default track.
  await request.post("/__mock/lyrics-mode", { data: { mode: "track" } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("LyricsView shows the loading state while the fetch is in flight (#198)", async ({
  page,
  request,
}) => {
  await request.post("/__mock/lyrics-mode", { data: { mode: "slow" } });
  await page.goto("/");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
  // Once now-playing arrives the LyricsView starts its fetch; the slow mock
  // holds the response ~2 s so the shared loading block is observable.
  await expect(page.getByTestId("state-loading")).toBeVisible({ timeout: 15000 });
});

test("LyricsView shows the error state when the fetch fails (#198)", async ({
  page,
  request,
}) => {
  await request.post("/__mock/lyrics-mode", { data: { mode: "error" } });
  await page.goto("/");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
  // A failed lyrics fetch renders the shared error block — NOT `lyrics-empty`
  // (which the pre-#198 code showed for every Err, hiding real failures).
  await expect(page.getByTestId("state-error")).toBeVisible({ timeout: 15000 });
  await expect(page.getByTestId("lyrics-empty")).toHaveCount(0);
});
