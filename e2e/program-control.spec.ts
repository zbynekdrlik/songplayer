import { test, expect, Page } from "@playwright/test";

// #209 (B1 of EPIC #174): the dashboard "Program" control. SongPlayer is the
// master switcher — its own NDI output `SP-program` carries whichever
// playlist output is cut to it. The control shows the on-program source and
// cuts to another playlist on click (`POST /api/v1/program/cut`), and follows
// a cut made elsewhere (it polls `GET /api/v1/program`). Runs under the
// default `chromium` project against the local mock (mock playlists: 1 Worship,
// 2 Background, 184 ytlive; the mock program starts on playlist 1).

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

function collectConsole(page: Page): string[] {
  const messages: string[] = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      messages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  return messages;
}

function realConsoleErrors(messages: string[]): string[] {
  return messages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

test.beforeEach(async ({ request }) => {
  await request.post("/__mock/program-reset");
});

// The mock program state is global in-memory state: never leak a cut into a
// serially-run sibling spec.
test.afterEach(async ({ request }) => {
  await request.post("/__mock/program-reset");
});

test("the Program control shows the on-program source and cuts on click", async ({
  page,
  request,
}) => {
  const consoleMessages = collectConsole(page);
  await page.setViewportSize({ width: 1600, height: 1000 });
  await page.goto("/");

  const control = page.getByTestId("program-control");
  await expect(control).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
    { timeout: 5000 },
  );
  const cuts = control.getByTestId("program-cut");
  await expect(cuts).toHaveCount(3);
  const worship = control.locator('[data-testid="program-cut"][data-playlist-id="1"]');
  const background = control.locator('[data-testid="program-cut"][data-playlist-id="2"]');
  await expect(worship).toHaveText("Worship");
  await expect(worship).toHaveAttribute("aria-pressed", "true");
  await expect(background).toHaveAttribute("aria-pressed", "false");

  // Cut to Background with the real mouse.
  const box = await background.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.click(box!.x + box!.width / 2, box!.y + box!.height / 2);

  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Background",
  );
  await expect(background).toHaveAttribute("aria-pressed", "true");
  await expect(worship).toHaveAttribute("aria-pressed", "false");
  await expect(page.getByTestId("program-error")).toHaveText("");

  // Backend effect: exactly one cut, with the body the server expects, and the
  // program moved from Worship (1) to Background (2).
  const last = await (await request.get("/__mock/program-last-cut")).json();
  expect(last.body).toEqual({ source: 2 });
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.source).toBe(2);
  expect(program.previous).toBe(1);
  expect(program.health.cuts).toBe(1);

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});

test("the Program control follows a cut made elsewhere", async ({
  page,
  request,
}) => {
  const consoleMessages = collectConsole(page);
  await page.goto("/");
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
    { timeout: 10000 },
  );

  // Another client (Companion, the API) cuts to ytlive.
  const resp = await request.post("/api/v1/program/cut", {
    data: { source: 184 },
  });
  expect(resp.status()).toBe(200);

  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: ytlive",
    { timeout: 5000 },
  );
  await expect(
    page.locator('[data-testid="program-cut"][data-playlist-id="184"]'),
  ).toHaveAttribute("aria-pressed", "true");

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});
