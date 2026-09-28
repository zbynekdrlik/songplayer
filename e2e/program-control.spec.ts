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

// #215 (B5 of EPIC #174): every cut is a transition. The "Prechod" line shows
// the one the next cut uses — cg OBS's scene transition by default, the
// Nastavenia override, or the default 300 ms fade while cg OBS's is not known
// (the mock runs no cg OBS; a spec injects its transition) — and the progress
// of a running fade.

async function clickCut(page: Page, id: number) {
  const button = page.locator(`[data-testid="program-cut"][data-playlist-id="${id}"]`);
  await expect(button).toBeVisible({ timeout: 10000 });
  const box = await button.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.click(box!.x + box!.width / 2, box!.y + box!.height / 2);
}

test("the Program control shows the transition a click cut uses and counts its mixed boundaries", async ({
  page,
  request,
}) => {
  const consoleMessages = collectConsole(page);
  await page.setViewportSize({ width: 1600, height: 1000 });
  await page.goto("/");
  const line = page.getByTestId("program-transition");
  await expect(line).toHaveText("Prechod: prelínanie 300 ms (predvolené)", {
    timeout: 10000,
  });

  // Cut to Background with the real mouse: one fade of 9 mixed boundaries.
  await clickCut(page, 2);
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Background",
  );
  let program = await (await request.get("/api/v1/program")).json();
  expect(program.transition.kind).toBe("fade");
  expect(program.transition.duration_ms).toBe(300);
  expect(program.transition.n_slots).toBe(9);
  expect(program.transition.source).toBe("fallback");
  expect(program.transition.transitions_done).toBe(1);
  expect(program.transition.mixed_boundaries).toBe(9);

  // A running fade shows its progress, then the line settles again.
  await request.post("/__mock/program-transition-active", {
    data: { progress: 44 },
  });
  await expect(line).toHaveText(
    "Prechod: prelínanie 300 ms (predvolené) — prebieha 44 %",
    { timeout: 5000 },
  );
  await request.post("/__mock/program-transition-active", { data: {} });
  await expect(line).toHaveText("Prechod: prelínanie 300 ms (predvolené)", {
    timeout: 5000,
  });

  // Nastavenia makes every cut a hard cut: the next click mixes nothing.
  await request.patch("/api/v1/settings", {
    data: { program_transition: "cut" },
  });
  await expect(line).toHaveText("Prechod: strih (nastavenie)", {
    timeout: 5000,
  });
  await clickCut(page, 1);
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
  );
  const last = await (await request.get("/__mock/program-last-cut")).json();
  expect(last.body).toEqual({ source: 1 });
  program = await (await request.get("/api/v1/program")).json();
  expect(program.transition.kind).toBe("cut");
  expect(program.transition.n_slots).toBe(0);
  expect(program.transition.transitions_done).toBe(2);
  expect(program.transition.mixed_boundaries).toBe(9);
  await expect(page.getByTestId("program-error")).toHaveText("");

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});

test("the Program control follows cg OBS's own scene transition", async ({
  page,
  request,
}) => {
  const consoleMessages = collectConsole(page);
  await page.setViewportSize({ width: 1600, height: 1000 });
  await request.post("/__mock/program-obs-transition", {
    data: { name: "Fade", kind: "fade_transition", duration_ms: 500 },
  });
  await page.goto("/");
  const line = page.getByTestId("program-transition");
  await expect(line).toHaveText("Prechod: prelínanie 500 ms (podľa OBS)", {
    timeout: 10000,
  });
  await clickCut(page, 184);
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: ytlive",
  );
  let program = await (await request.get("/api/v1/program")).json();
  expect(program.transition.n_slots).toBe(15);
  expect(program.transition.mixed_boundaries).toBe(15);
  expect(program.follow.obs_transition).toEqual({
    name: "Fade",
    kind: "fade_transition",
    duration_ms: 500,
  });

  // The operator switches cg OBS to its Cut transition: SP-program follows.
  await request.post("/__mock/program-obs-transition", {
    data: { name: "Cut", kind: "cut_transition", duration_ms: null },
  });
  await expect(line).toHaveText("Prechod: strih (podľa OBS)", {
    timeout: 5000,
  });
  await clickCut(page, 1);
  await expect(page.getByTestId("program-source")).toHaveText(
    "Na programe: Worship",
  );
  program = await (await request.get("/api/v1/program")).json();
  expect(program.transition.kind).toBe("cut");
  expect(program.transition.source).toBe("obs");
  expect(program.transition.mixed_boundaries).toBe(15);

  // Zero console errors — the last assertion.
  expect(realConsoleErrors(consoleMessages)).toEqual([]);
});
