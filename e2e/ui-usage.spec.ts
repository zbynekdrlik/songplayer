import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

// #256: the dashboard records how it is used — one capture-phase listener
// on `document` for click / change / submit plus a `page` event per route
// change (sp-ui `usage.rs`), sent to `POST /api/v1/ui-usage` every 10 s and
// by `navigator.sendBeacon` when the page is hidden. A real person's
// clicks, change and submit arrive as exactly those events (page, control,
// kind); a page the operator cannot see (SongPlayer's own hidden app
// window) owes its view until it can be seen; an automated session
// (`navigator.webdriver`, which Playwright sets) and a browser with the
// `sp_no_usage` localStorage key send nothing; Nastavenia's line reads the
// summary. The positive tests make the page look like a person's browser
// first (`asAPerson`). Runs against the mock, which records every batch it
// took (`/__mock/ui-usage`). Zero console errors is each test's last
// assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

/** The batch flush period (`sp_core::ui_usage::FLUSH_EVERY_MS`). */
const FLUSH_MS = 10_000;

type UsageEvent = { ts_ms: number; page: string; control: string; kind: string };
type Post = { content_type: string; events: UsageEvent[] };

let consoleMessages: string[] = [];

function watchConsole(page: Page) {
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
}

/** Playwright sets `navigator.webdriver`, which turns the usage log off:
 * make this page read as a person's browser. */
async function asAPerson(page: Page) {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "webdriver", { configurable: true, get: () => false });
  });
}

/** Every batch the mock took, in arrival order. */
async function posts(request: APIRequestContext): Promise<Post[]> {
  const resp = await request.get("/__mock/ui-usage");
  expect(resp.status()).toBe(200);
  return ((await resp.json()) as { posts: Post[] }).posts;
}

/** What each event says, without its time. */
async function recorded(request: APIRequestContext) {
  return (await posts(request))
    .flatMap((p) => p.events)
    .map((e) => ({ page: e.page, control: e.control, kind: e.kind }));
}

/** The page is being hidden (a tab switched away or closed). */
async function hide(page: Page) {
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "hidden",
    });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

test.beforeEach(async ({ request }) => {
  consoleMessages = [];
  await request.post("/__mock/ui-usage-reset");
  await request.post("/__mock/settings-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/ui-usage-reset");
  await request.post("/__mock/settings-reset");
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

test("a person's clicks, change and submit arrive as exactly those events, page by page", async ({
  page,
  request,
}) => {
  watchConsole(page);
  await asAPerson(page);
  const startedAt = Date.now();
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  // The tab already shown: its click is recorded, no page view.
  await page.getByTestId("nav-dashboard").click();
  await page.getByTestId("nav-settings").click();
  await expect(page.getByTestId("settings-gemini-model")).toHaveValue("gemini-2.5-flash", {
    timeout: 10000,
  });
  // A heading has no `data-testid` (nor does any ancestor): it shows up as
  // its tag, a gap in the ids.
  await page.getByRole("heading", { name: "Nastavenia", exact: true }).click();
  await page.getByTestId("settings-program-transition-kind").selectOption("cut");
  const saved = page.waitForRequest(
    (req) => req.method() === "PATCH" && /\/api\/v1\/settings$/.test(req.url()),
  );
  await page.getByRole("button", { name: "Uložiť nastavenia", exact: true }).click();
  await saved;
  await page.getByTestId("nav-dashboard").click();
  await expect(page.getByTestId("nav-dashboard")).toHaveClass(/active/);
  await hide(page);

  const expected = [
    { page: "/", control: "", kind: "page" },
    { page: "/", control: "nav-dashboard", kind: "click" },
    { page: "/", control: "nav-settings", kind: "click" },
    { page: "/settings", control: "", kind: "page" },
    { page: "/settings", control: "<h1>", kind: "click" },
    { page: "/settings", control: "settings-program-transition-kind", kind: "change" },
    { page: "/settings", control: "<button>", kind: "click" },
    { page: "/settings", control: "<form>", kind: "submit" },
    { page: "/settings", control: "nav-dashboard", kind: "click" },
    { page: "/", control: "", kind: "page" },
  ];
  await expect.poll(() => recorded(request), { timeout: 5000 }).toEqual(expected);

  // Each stamp is the browser's clock at the event, in order.
  const stamps = (await posts(request)).flatMap((p) => p.events).map((e) => e.ts_ms);
  const doneAt = Date.now();
  for (const [i, ts] of stamps.entries()) {
    expect(ts, `event ${i}`).toBeGreaterThanOrEqual(startedAt);
    expect(ts, `event ${i}`).toBeLessThanOrEqual(doneAt);
    if (i > 0) expect(ts, `event ${i}`).toBeGreaterThanOrEqual(stamps[i - 1]);
  }
});

test("what was recorded goes out every 10 s by fetch", async ({ page, request }) => {
  watchConsole(page);
  await asAPerson(page);
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  await page.getByTestId("nav-live").click();
  await expect(page.getByTestId("nav-live")).toHaveClass(/active/);

  await expect
    .poll(async () => (await posts(request)).length, { timeout: FLUSH_MS + 5000 })
    .toBe(1);
  const [post] = await posts(request);
  expect(post.content_type).toMatch(/^application\/json/);
  expect(post.events.map((e) => [e.page, e.control, e.kind])).toEqual([
    ["/", "", "page"],
    ["/", "nav-live", "click"],
    ["/live", "", "page"],
  ]);
});

test("a page being hidden hands what waits to sendBeacon", async ({ page, request }) => {
  watchConsole(page);
  await asAPerson(page);
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  await page.getByTestId("nav-lyrics").click();
  await expect(page.getByTestId("nav-lyrics")).toHaveClass(/active/);
  // Well inside the first 10 s: the timer has sent nothing yet.
  await hide(page);

  await expect.poll(async () => (await posts(request)).length, { timeout: 5000 }).toBe(1);
  const [post] = await posts(request);
  expect(post.content_type).toMatch(/^text\/plain/);
  expect(post.events.map((e) => [e.page, e.control, e.kind])).toEqual([
    ["/", "", "page"],
    ["/", "nav-lyrics", "click"],
    ["/lyrics", "", "page"],
  ]);
});

test("an automated session and a browser with sp_no_usage send nothing", async ({
  page,
  context,
  browser,
  request,
}) => {
  // `page` keeps Playwright's own navigator.webdriver === true; `marked`
  // reads as a person's browser but carries the sp_no_usage key, in a
  // context of its own (pages of one context share their localStorage);
  // `person` is the control: the same session, recorded.
  watchConsole(page);
  // A context the `context` fixture did not make is never closed by
  // Playwright: close it on every path, or its page keeps polling the mock.
  const markedContext = await browser.newContext({ baseURL: test.info().project.use.baseURL });
  try {
    const marked = await markedContext.newPage();
    watchConsole(marked);
    await asAPerson(marked);
    await marked.addInitScript(() => {
      localStorage.setItem("sp_no_usage", "1");
    });
    const person = await context.newPage();
    watchConsole(person);
    await asAPerson(person);

    for (const p of [page, marked, person]) {
      await p.goto("/");
      await expect(p.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
      await p.getByTestId("nav-settings").click();
      await expect(p.getByTestId("nav-settings")).toHaveClass(/active/);
      await p.getByTestId("nav-dashboard").click();
      await expect(p.getByTestId("nav-dashboard")).toHaveClass(/active/);
    }
    // Hiding a page sends what its log holds at once (sendBeacon): the
    // person's session arrives; the other two, had they a log, would too.
    for (const p of [page, marked, person]) await hide(p);
    const once = [
      { page: "/", control: "", kind: "page" },
      { page: "/", control: "nav-settings", kind: "click" },
      { page: "/settings", control: "", kind: "page" },
      { page: "/settings", control: "nav-dashboard", kind: "click" },
      { page: "/", control: "", kind: "page" },
    ];
    await expect.poll(() => recorded(request), { timeout: 5000 }).toEqual(once);
    await page.waitForTimeout(1000);
    expect(await recorded(request), "only the person's session").toEqual(once);
    await person.close();
  } finally {
    await markedContext.close();
  }
});

test("a page the operator cannot see records its view only once it can be seen", async ({
  page,
  request,
}) => {
  // SongPlayer's own app window loads the dashboard hidden and unfocused
  // at every start (until the tray shows it): its view must not count.
  watchConsole(page);
  await asAPerson(page);
  await page.addInitScript(() => {
    let seen = false;
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => (seen ? "visible" : "hidden"),
    });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => !seen });
    document.hasFocus = () => seen;
    (window as unknown as { spSeen: (value: boolean) => void }).spSeen = (value: boolean) => {
      seen = value;
      document.dispatchEvent(new Event("visibilitychange"));
      if (value) window.dispatchEvent(new Event("focus"));
    };
  });
  const setSeen = (value: boolean) =>
    page.evaluate((v) => (window as unknown as { spSeen: (s: boolean) => void }).spSeen(v), value);
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  // Hidden again: a view recorded at the load would go out now.
  await setSeen(false);
  await page.waitForTimeout(1000);
  expect(await posts(request), "no view while unseen").toEqual([]);
  // Shown (the tray), then hidden: the owed view, once.
  await setSeen(true);
  await setSeen(false);
  await expect.poll(async () => (await posts(request)).length, { timeout: 5000 }).toBe(1);
  expect(await recorded(request)).toEqual([{ page: "/", control: "", kind: "page" }]);
});

test("a visible page with no focus owes its view until the window is focused", async ({
  page,
  request,
}) => {
  // WebView2 may keep a hidden app window's page "visible": there only the
  // window's `focus` (the tray's show + set_focus) can bring the view in.
  watchConsole(page);
  await asAPerson(page);
  await page.addInitScript(() => {
    let focused = false;
    document.hasFocus = () => focused;
    (window as unknown as { spFocus: () => void }).spFocus = () => {
      focused = true;
      window.dispatchEvent(new Event("focus"));
    };
  });
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  // Never focused: hiding it sends nothing (a view recorded at the load
  // would go out now).
  await hide(page);
  await page.waitForTimeout(1000);
  expect(await posts(request), "no view while unfocused").toEqual([]);
  // Visible again, still unfocused: nothing; then the window's focus.
  await page.evaluate(() => {
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    Object.defineProperty(document, "hidden", { configurable: true, get: () => false });
    document.dispatchEvent(new Event("visibilitychange"));
    (window as unknown as { spFocus: () => void }).spFocus();
  });
  await hide(page);
  await expect.poll(async () => (await posts(request)).length, { timeout: 5000 }).toBe(1);
  expect(await recorded(request)).toEqual([{ page: "/", control: "", kind: "page" }]);
});

test("Nastavenia says how many events the log keeps, over how many days, and links the summary", async ({
  page,
  request,
}) => {
  watchConsole(page);
  const now = Date.now();
  const HOUR = 3_600_000;
  const seeded = await request.post("/api/v1/ui-usage", {
    data: {
      events: [
        { ts_ms: now - 25 * HOUR, page: "/", control: "nav-live", kind: "click" },
        { ts_ms: now - 2 * HOUR, page: "/live", control: "", kind: "page" },
        { ts_ms: now - HOUR, page: "/live", control: "player-skip", kind: "click" },
      ],
    },
  });
  expect(seeded.status()).toBe(204);

  await page.goto("/settings");
  await expect(page.getByTestId("ui-usage-line")).toHaveText(
    "Záznam používania: 3 udalosti za 2 dni",
    { timeout: 10000 },
  );
  const link = page.getByTestId("ui-usage-summary-link");
  await expect(link).toHaveAttribute("href", "/api/v1/ui-usage/summary?days=400");
  await expect(link).toHaveText("súhrn (JSON)");
  // The link's target answers the summary those three events make.
  const summary = await request.get("/api/v1/ui-usage/summary?days=400");
  expect(summary.status()).toBe(200);
  const body = await summary.json();
  expect(body.events).toBe(3);
  expect(body.first_ms).toBe(now - 25 * HOUR);
});
