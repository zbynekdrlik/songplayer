import { test, expect } from "@playwright/test";

// #230 — while a `sp-90s` press holds the background jobs (no new download,
// lyrics, stems, dub, sync, repair or peer transfer until `sp-slow` goes on
// program or 4 h pass), the shared HealthBar shows an amber segment with the
// time left; it is absent while nothing is held. The bar polls
// `GET /api/v1/background-hold` every 5 s, so a hold that arms or ends
// while the page is open shows up without a reload. Runs against the mock.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/background-hold-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/background-hold-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("the hold segment appears while held, names the time left and what waits, and goes when it ends", async ({
  page,
  request,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("health-version")).toBeVisible();
  await expect(page.getByTestId("health-background-hold")).toHaveCount(0);

  await request.post("/__mock/background-hold", {
    data: {
      held: true,
      until_utc_ms: 1791608681366,
      remaining_s: 14280,
      held_jobs: ["download", "lyrics"],
    },
  });
  const segment = page.getByTestId("health-background-hold");
  await expect(segment).toHaveText("Pozadie: pozastavené (ešte 3 h 58 min)", {
    timeout: 10000,
  });
  await expect(segment).toHaveClass(/health-warn/);
  const tip = await segment.getAttribute("title");
  expect(tip).toContain("Na programe bola scéna sp-90s");
  expect(tip).toContain("kým na program nepôjde sp-slow alebo neuplynú 4 h");
  expect(tip).toContain("Čaká: sťahovanie, texty.");

  // The next poll follows the server: less time left.
  await request.post("/__mock/background-hold", {
    data: { held: true, until_utc_ms: 1791608681366, remaining_s: 600 },
  });
  await expect(segment).toHaveText("Pozadie: pozastavené (ešte 10 min)", {
    timeout: 10000,
  });

  // Released (sp-slow or the 4 h): the segment goes.
  await request.post("/__mock/background-hold-reset");
  await expect(page.getByTestId("health-background-hold")).toHaveCount(0, {
    timeout: 10000,
  });
});

test("the hold segment shows on every page's health bar", async ({
  page,
  request,
}) => {
  await request.post("/__mock/background-hold", {
    data: { held: true, until_utc_ms: 1791608681366, remaining_s: 3600 },
  });
  for (const path of ["/", "/settings", "/lyrics"]) {
    await page.goto(path);
    await expect(page.getByTestId("health-background-hold")).toHaveText(
      "Pozadie: pozastavené (ešte 1 h)",
      { timeout: 10000 },
    );
  }
});
