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
  await request.post("/__mock/settings-reset");
});

// The mock program state + settings are global in-memory state: never leak a
// cut or an enabled NDI input into a serially-run sibling spec.
test.afterEach(async ({ request }) => {
  await request.post("/__mock/program-reset");
  await request.post("/__mock/settings-reset");
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

// #212 (B3 of EPIC #174): the NDI input "OBS manuál" (cg OBS's manual-scene
// mix received over NDI) is one more program source while it is enabled in
// Nastavenia — listed after the playlists, cut with `{"source": -1}`.

test("with the NDI input disabled the Program control offers no OBS manuál", async ({
  page,
}) => {
  const consoleMessages = collectConsole(page);
  await page.goto("/");
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
    { timeout: 10000 },
  );
  await expect(page.getByTestId("program-cut")).toHaveCount(3);
  await expect(
    page.locator('[data-testid="program-cut"][data-playlist-id="-1"]'),
  ).toHaveCount(0);

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});

test("an enabled NDI input with no source name is not offered and cannot be cut to", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", {
    data: { ndi_input_enabled: "true", ndi_input_source: "" },
  });
  const consoleMessages = collectConsole(page);
  await page.goto("/");
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
    { timeout: 10000 },
  );
  // Let at least one program poll land (it carries input.enabled = true).
  await page.waitForResponse((r) => r.url().endsWith("/api/v1/program"));
  await expect(page.getByTestId("program-cut")).toHaveCount(3);
  await expect(
    page.locator('[data-testid="program-cut"][data-playlist-id="-1"]'),
  ).toHaveCount(0);
  const resp = await request.post("/api/v1/program/cut", { data: { source: -1 } });
  expect(resp.status()).toBe(404);
  const program = await (await request.get("/api/v1/program")).json();
  expect(program.source).toBe(1);
  expect(program.input.enabled).toBe(true);
  expect(program.input.source).toBe("");

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});

test("the Program control lists OBS manuál after the playlists and cuts to it and back", async ({
  page,
  request,
}) => {
  await request.patch("/api/v1/settings", {
    data: { ndi_input_enabled: "true", ndi_input_source: "CG-OBS (manual)" },
  });
  const consoleMessages = collectConsole(page);
  await page.setViewportSize({ width: 1600, height: 1000 });
  await page.goto("/");

  const control = page.getByTestId("program-control");
  const input = control.locator('[data-testid="program-cut"][data-playlist-id="-1"]');
  await expect(input).toBeVisible({ timeout: 10000 });
  await expect(input).toHaveText("OBS manuál");
  await expect(input).toHaveAttribute("aria-pressed", "false");
  const cuts = control.getByTestId("program-cut");
  await expect(cuts).toHaveCount(4);
  await expect(cuts.last()).toHaveAttribute("data-playlist-id", "-1");

  // Cut to OBS manuál with the real mouse.
  let box = await input.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.click(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: OBS manuál",
  );
  await expect(input).toHaveAttribute("aria-pressed", "true");
  const worship = control.locator('[data-testid="program-cut"][data-playlist-id="1"]');
  await expect(worship).toHaveAttribute("aria-pressed", "false");
  await expect(page.getByTestId("program-error")).toHaveText("");

  // Backend effect: the cut body is the input's id and the program moved.
  let last = await (await request.get("/__mock/program-last-cut")).json();
  expect(last.body).toEqual({ source: -1 });
  let program = await (await request.get("/api/v1/program")).json();
  expect(program.source).toBe(-1);
  expect(program.previous).toBe(1);
  expect(program.input.enabled).toBe(true);
  expect(program.input.stream).toBe("manual");

  // And back to Worship.
  box = await worship.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.click(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
  );
  await expect(input).toHaveAttribute("aria-pressed", "false");
  last = await (await request.get("/__mock/program-last-cut")).json();
  expect(last.body).toEqual({ source: 1 });
  program = await (await request.get("/api/v1/program")).json();
  expect(program.source).toBe(1);
  expect(program.previous).toBe(-1);
  expect(program.health.cuts).toBe(2);

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});
