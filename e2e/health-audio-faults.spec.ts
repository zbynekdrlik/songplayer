import { test, expect, type Locator, type Page } from "@playwright/test";

// #257 lane 2 — the audio fault history on the dashboard. The owner
// (12.10.2026): "song player zobrazoval pocet problemov a nejaky cas/graph
// aby bolo videt ako dlho to je bez chyby alebo ked sa problem udial", for
// every audio path. First glance (#248, every service): ONE HealthBar
// segment for the whole chain (`health-audio`, its `data-level`): green
// "Zvuk: bez chyby …" (risks counted beside it), amber on a loss in 24 h,
// red on one in the last 10 min, naming the worst path. One click: the
// panel — a row per path (counts, "bez chyby od", a loop's checked time, a
// timeline of 60 buckets), one shared time axis, the event list, over
// 1 h / 24 h / 7 d / 30 d. Runs against the mock's ledger scenarios
// (`/__mock/audio-faults {scenario, delay_ms}`), local times in
// Europe/Bratislava.

test.use({ timezoneId: "Europe/Bratislava" });

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

const ZONE = "Europe/Bratislava";
const FAULTS = "/api/v1/audio/faults";

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/audio-faults-reset");
  await request.post("/__mock/settings-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/audio-faults-reset");
  await request.post("/__mock/settings-reset");
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

async function scenario(page: Page, name: string, delayMs = 0) {
  const resp = await page.request.post("/__mock/audio-faults", {
    data: { scenario: name, delay_ms: delayMs },
  });
  expect(resp.status()).toBe(200);
}

/** An event's time as the dashboard writes it: `12.10. 14:05:07`, local. */
function eventTime(ms: number): string {
  const format = new Intl.DateTimeFormat("en-GB", {
    timeZone: ZONE,
    day: "numeric",
    month: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  });
  const p = Object.fromEntries(format.formatToParts(new Date(ms)).map((x) => [x.type, x.value]));
  return `${Number(p.day)}.${Number(p.month)}. ${p.hour}:${p.minute}:${p.second}`;
}

async function openPanel(page: Page): Promise<Locator> {
  const segment = page.getByTestId("health-audio");
  await expect(segment).toHaveAttribute("aria-expanded", "false");
  await segment.click();
  await expect(segment).toHaveAttribute("aria-expanded", "true");
  const panel = page.getByTestId("audio-faults-panel");
  await expect(panel).toBeVisible();
  return panel;
}

function row(panel: Locator, path: string): Locator {
  return panel.locator(`[data-testid="audio-path-row"][data-path="${path}"]`);
}

async function pickWindow(panel: Locator, hours: number) {
  const button = panel.locator(`[data-testid="audio-window"][data-hours="${hours}"]`);
  await button.click();
  await expect(button).toHaveClass(/selected/);
}

test("a clean chain reads green with how long it is without a loss; risks are counted beside it", async ({
  page,
}) => {
  await scenario(page, "clean");
  await page.goto("/");
  const segment = page.getByTestId("health-audio");
  await expect(segment).toHaveAttribute("data-level", "ok", { timeout: 15000 });
  await expect(segment).toHaveText("Zvuk: bez chyby 2 h 14 min");
  await expect(segment).toHaveClass(/health-ok/);
  await expect(segment).toHaveAttribute(
    "title",
    "Všetky zvukové cesty za posledných 24 h: 0 chýb, 0 rizík. Kliknutím zobrazíte podrobnosti.",
  );
  await expect(page.getByTestId("health-audio-risks")).toHaveCount(0);

  await scenario(page, "risk");
  await page.goto("/");
  await expect(segment).toHaveAttribute("data-level", "risk", { timeout: 15000 });
  await expect(segment).toHaveText("Zvuk: bez chyby 2 h 14 min · riziká: 1");
  await expect(segment).toHaveClass(/health-ok/);
  await expect(page.getByTestId("health-audio-risks")).toHaveText("· riziká: 1");
});

test("a loss that appears while the page is open turns the segment red at the next poll", async ({ page }) => {
  await scenario(page, "clean");
  await page.goto("/");
  const segment = page.getByTestId("health-audio");
  await expect(segment).toHaveAttribute("data-level", "ok", { timeout: 15000 });
  await scenario(page, "recent");
  await expect(segment).toHaveAttribute("data-level", "recent", { timeout: 15000 });
  await expect(segment).toHaveText("Zvuk: 3 chyby za 24 h · posledná pred 3 min · Waves SoundGrid");
  await expect(segment).toHaveClass(/health-error/);
});

test("a loss 3 minutes ago: red, every path's row with its counts, timeline and the event list", async ({
  page,
}) => {
  await scenario(page, "recent");
  await page.goto("/");
  const segment = page.getByTestId("health-audio");
  await expect(segment).toHaveAttribute("data-level", "recent", { timeout: 15000 });
  await expect(segment).toHaveText("Zvuk: 3 chyby za 24 h · posledná pred 3 min · Waves SoundGrid");
  await expect(segment).toHaveClass(/health-error/);
  await expect(segment).toHaveAttribute(
    "title",
    "Všetky zvukové cesty za posledných 24 h: 3 chyby, 2 riziká. Kliknutím zobrazíte podrobnosti.",
  );

  const panel = await openPanel(page);
  await expect(panel.locator('[data-testid="audio-window"][data-hours="24"]')).toHaveClass(/selected/);
  const rows = panel.getByTestId("audio-path-row");
  await expect(rows).toHaveCount(5);
  expect(await rows.evaluateAll((els) => els.map((e) => e.getAttribute("data-path")))).toEqual([
    "program",
    "out:out-1",
    "out:out-2",
    "loop:out-2",
    "pipeline:1",
  ]);
  // name, losses, risks, the "bez chyby" span in brackets, bars.
  const expected: Array<[string, string, string, string, string, number]> = [
    ["program", "Program", "1 chyba", "0 rizík", "5 h", 1],
    ["out:out-1", "FOH", "0 chýb", "2 riziká", "2 h 14 min", 2],
    ["out:out-2", "Waves SoundGrid", "1 chyba", "0 rizík", "3 min", 1],
    ["loop:out-2", "Slučka – Waves SoundGrid", "1 chyba", "0 rizík", "40 min", 1],
    ["pipeline:1", "Worship", "0 chýb", "0 rizík", "2 h 14 min", 0],
  ];
  for (const [path, name, losses, risks, since, bars] of expected) {
    const r = row(panel, path);
    await expect(r.getByTestId("audio-path-name"), path).toHaveText(name);
    await expect(r.getByTestId("audio-path-losses"), path).toHaveText(losses);
    await expect(r.getByTestId("audio-path-risks"), path).toHaveText(risks);
    await expect(r.getByTestId("audio-path-clean"), path).toHaveText(
      new RegExp(`^bez chyby od \\d{1,2}\\.\\d{1,2}\\. \\d{2}:\\d{2} \\(${since}\\)$`),
    );
    await expect(r.getByTestId("audio-bar"), path).toHaveCount(bars);
  }
  await expect(row(panel, "out:out-2").getByTestId("audio-path-losses")).toHaveClass(/has-loss/);
  await expect(row(panel, "out:out-1").getByTestId("audio-path-risks")).toHaveClass(/has-risk/);
  await expect(row(panel, "loop:out-2").getByTestId("audio-path-loop")).toHaveText(
    "skontrolované 59 min · ticho 1 min",
  );
  await expect(row(panel, "program").getByTestId("audio-path-loop")).toHaveCount(0);
  const bar = row(panel, "out:out-2").getByTestId("audio-bar");
  await expect(bar).toHaveAttribute("data-loss", "1");
  await expect(bar).toHaveAttribute("data-risk", "0");
  const fohBars = row(panel, "out:out-1").getByTestId("audio-bar");
  expect(await fohBars.evaluateAll((els) => els.map((e) => e.getAttribute("data-risk")))).toEqual(["1", "1"]);

  // One shared axis: a tick every 3 local hours (on whole hours; a
  // daylight-saving change inside the window moves the older ones by one).
  const ticks = panel.getByTestId("audio-axis-tick");
  const labels = await ticks.allTextContents();
  expect(labels.length).toBeGreaterThanOrEqual(8);
  expect(labels.length).toBeLessThanOrEqual(9);
  for (const label of labels) expect(label).toMatch(/^\d{2}:00$/);

  // The event list: every loss and risk, newest first, in local time.
  const events = panel.getByTestId("audio-fault-event");
  await expect(events).toHaveCount(5);
  expect(await events.getByTestId("audio-event-kind").allTextContents()).toEqual([
    "podtečenie",
    "oneskorený paket",
    "výpadok",
    "oneskorený paket",
    "vyplnené tichom",
  ]);
  expect(await events.getByTestId("audio-event-path").allTextContents()).toEqual([
    "Waves SoundGrid",
    "FOH",
    "Slučka – Waves SoundGrid",
    "FOH",
    "Program",
  ]);
  expect(await events.getByTestId("audio-event-amount").allTextContents()).toEqual([
    "×2",
    "12,5 ms",
    "480 vzoriek",
    "11,0 ms",
    "×3",
  ]);
  expect(await events.evaluateAll((els) => els.map((e) => e.getAttribute("data-severity")))).toEqual([
    "loss",
    "risk",
    "loss",
    "risk",
    "loss",
  ]);
  const stamps = (await events.evaluateAll((els) => els.map((e) => e.getAttribute("data-ts")))).map(Number);
  expect([...stamps].sort((a, b) => b - a)).toEqual(stamps);
  const times = await events.getByTestId("audio-event-time").allTextContents();
  expect(times).toEqual(stamps.map(eventTime));
  await expect(panel.getByTestId("audio-faults-no-events")).toHaveCount(0);
  await expect(panel.getByTestId("audio-faults-truncated")).toHaveCount(0);

  // The panel closes with its own button, and opens again from the segment.
  await panel.getByTestId("audio-faults-close").click();
  await expect(page.getByTestId("audio-faults-panel")).toHaveCount(0);
  await expect(segment).toHaveAttribute("aria-expanded", "false");
  await openPanel(page);
});

test("the window selector re-reads the history over 1 h and 7 d", async ({ page }) => {
  await scenario(page, "recent");
  await page.goto("/");
  await expect(page.getByTestId("health-audio")).toHaveAttribute("data-level", "recent", { timeout: 15000 });
  const panel = await openPanel(page);

  await pickWindow(panel, 1);
  // In the last hour: SoundGrid's underrun, its loop's dropout, one FOH risk.
  const events = panel.getByTestId("audio-fault-event");
  await expect(events).toHaveCount(3);
  await expect(row(panel, "program").getByTestId("audio-path-losses")).toHaveText("0 chýb");
  await expect(row(panel, "program").getByTestId("audio-path-clean")).toHaveText("bez chyby viac ako 1 h");
  await expect(row(panel, "out:out-1").getByTestId("audio-path-risks")).toHaveText("1 riziko");
  for (const [path, bars] of [
    ["program", 0],
    ["out:out-1", 1],
    ["out:out-2", 1],
    ["loop:out-2", 1],
    ["pipeline:1", 0],
  ] as const) {
    await expect(row(panel, path).getByTestId("audio-bar"), path).toHaveCount(bars);
  }
  const hourTicks = await panel.getByTestId("audio-axis-tick").allTextContents();
  expect(hourTicks.length).toBeGreaterThanOrEqual(6);
  expect(hourTicks.length).toBeLessThanOrEqual(7);
  for (const label of hourTicks) expect(label).toMatch(/^\d{2}:\d0$/);
  // The segment keeps its own 24 h.
  await expect(page.getByTestId("health-audio")).toHaveText(
    "Zvuk: 3 chyby za 24 h · posledná pred 3 min · Waves SoundGrid",
  );

  await pickWindow(panel, 168);
  await expect(events).toHaveCount(5);
  await expect(row(panel, "program").getByTestId("audio-path-losses")).toHaveText("1 chyba");
  const dayTicks = await panel.getByTestId("audio-axis-tick").allTextContents();
  expect(dayTicks.length).toBeGreaterThanOrEqual(7);
  expect(dayTicks.length).toBeLessThanOrEqual(8);
  for (const label of dayTicks) expect(label).toMatch(/^\d{1,2}\.\d{1,2}\.$/);
});

test("an older loss reads amber; a window without it says so", async ({ page }) => {
  await scenario(page, "old");
  await page.goto("/");
  const segment = page.getByTestId("health-audio");
  await expect(segment).toHaveAttribute("data-level", "loss", { timeout: 15000 });
  await expect(segment).toHaveText("Zvuk: 1 chyba za 24 h · posledná pred 3 h · Program");
  await expect(segment).toHaveClass(/health-warn/);

  const panel = await openPanel(page);
  await expect(row(panel, "program").getByTestId("audio-bar")).toHaveCount(1);
  await expect(panel.getByTestId("audio-fault-event")).toHaveCount(1);
  await pickWindow(panel, 1);
  await expect(panel.getByTestId("audio-faults-no-events")).toHaveText(
    "V tomto období žiadna chyba ani riziko.",
  );
  await expect(panel.getByTestId("audio-fault-event")).toHaveCount(0);
  await expect(panel.getByTestId("audio-bar")).toHaveCount(0);
  await expect(row(panel, "program").getByTestId("audio-path-clean")).toHaveText("bez chyby viac ako 1 h");
});

test("frames read as milliseconds at their output's rate", async ({ page, request }) => {
  const outputs = [
    {
      id: "out-2",
      type: "asio",
      name: "Waves SoundGrid",
      asio: { driver: "Waves SoundGrid ASIO", channels: [0, 1] },
    },
  ];
  const saved = await request.patch("/api/v1/settings", {
    data: { audio_outputs: JSON.stringify(outputs), audio_network_rate: "96000" },
  });
  expect(saved.status()).toBe(204);
  await scenario(page, "recent");
  await page.goto("/");
  await expect(page.getByTestId("health-audio")).toHaveAttribute("data-level", "recent", { timeout: 15000 });
  const panel = await openPanel(page);
  // 480 frames at 96 kHz.
  await expect(
    panel.locator('[data-testid="audio-fault-event"][data-path="loop:out-2"] [data-testid="audio-event-amount"]'),
  ).toHaveText("5,0 ms");
});

test("the dashboard never has two history requests in flight", async ({ page }) => {
  let inFlight = 0;
  let most = 0;
  const asked: string[] = [];
  page.on("request", (r) => {
    if (!r.url().includes(FAULTS)) return;
    inFlight += 1;
    most = Math.max(most, inFlight);
    asked.push(new URL(r.url()).searchParams.get("hours") ?? "");
  });
  const done = (r: { url(): string }) => {
    if (r.url().includes(FAULTS)) inFlight -= 1;
  };
  page.on("requestfinished", done);
  page.on("requestfailed", done);

  await scenario(page, "recent", 700);
  await page.goto("/");
  await expect(page.getByTestId("health-audio")).toHaveAttribute("data-level", "recent", { timeout: 15000 });
  const panel = await openPanel(page);
  for (const hours of [1, 168, 720, 1]) {
    await panel.locator(`[data-testid="audio-window"][data-hours="${hours}"]`).click();
  }
  await expect(row(panel, "program").getByTestId("audio-path-clean")).toHaveText("bez chyby viac ako 1 h", {
    timeout: 10000,
  });
  expect(most).toBe(1);
  expect(asked[0]).toBe("24");
  expect(asked).toContain("1");
});
