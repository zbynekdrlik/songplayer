import { test, expect, type Page } from "@playwright/test";

// #261 (no infinite archive): the server's daily cache sweep also measures
// the data dir, and `GET /api/v1/status` carries `data_dir_bytes`,
// `cache_bytes` and `data_dir_measured_ms` (null until the first measure,
// 5 min after a start). A real user opens Nastavenia and reads ONE line
// about it, like #256's usage line: "Dáta na disku: 123,4 GB (cache 120,0
// GB), merané 17.10. 14:05", the time in the browser's own zone. Not on
// the HealthBar (read rarely). Runs against the mock
// (`/__mock/data-size {…}`, reset with `/__mock/data-size-reset`). Zero
// console errors is each test's last assertion.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

function watchConsole(page: Page) {
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
}

function realConsoleErrors(): string[] {
  return consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
}

const MIB = 1_048_576;
const GIB = 1_073_741_824;

/** An instant as the line names it, read by the PAGE's own clock zone. */
async function localTime(page: Page, ms: number): Promise<string> {
  return page.evaluate((at) => {
    const d = new Date(at);
    const two = (n: number) => String(n).padStart(2, "0");
    return `${d.getDate()}.${d.getMonth() + 1}. ${two(d.getHours())}:${two(d.getMinutes())}`;
  }, ms);
}

test.beforeEach(async ({ request }) => {
  consoleMessages = [];
  await request.post("/__mock/data-size-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/data-size-reset");
});

test("Nastavenia names the data dir's size, the cache's and when it was measured", async ({
  page,
  request,
}) => {
  watchConsole(page);
  const status = await (await request.get("/api/v1/status")).json();
  expect(status.data_dir_bytes).toBe(123 * GIB + Math.floor((4 * GIB) / 10));
  expect(status.cache_bytes).toBe(120 * GIB);
  await page.goto("/settings");
  const when = await localTime(page, status.data_dir_measured_ms);
  await expect(page.getByTestId("data-size-line")).toHaveText(
    `Dáta na disku: 123,4 GB (cache 120,0 GB), merané ${when}`,
    { timeout: 10000 },
  );
  // One line in Nastavenia, never a HealthBar segment.
  await expect(page.getByTestId("data-size-line")).toHaveCount(1);
  await expect(page.locator(".health-bar").getByText("Dáta na disku")).toHaveCount(0);
  expect(realConsoleErrors()).toEqual([]);
});

test("a size under a gigabyte reads in megabytes", async ({ page, request }) => {
  watchConsole(page);
  const measured = Date.now() - 5 * 60_000;
  await request.post("/__mock/data-size", {
    data: { data_dir_bytes: 850 * MIB, cache_bytes: 300 * MIB, data_dir_measured_ms: measured },
  });
  await page.goto("/settings");
  const when = await localTime(page, measured);
  await expect(page.getByTestId("data-size-line")).toHaveText(
    `Dáta na disku: 850 MB (cache 300 MB), merané ${when}`,
    { timeout: 10000 },
  );
  expect(realConsoleErrors()).toEqual([]);
});

test("before the first measure the line says it is not measured yet", async ({
  page,
  request,
}) => {
  watchConsole(page);
  await request.post("/__mock/data-size", { data: {} });
  const status = await (await request.get("/api/v1/status")).json();
  expect(status.data_dir_bytes).toBeNull();
  expect(status.cache_bytes).toBeNull();
  expect(status.data_dir_measured_ms).toBeNull();
  await page.goto("/settings");
  await expect(page.getByTestId("data-size-line")).toHaveText(
    "Dáta na disku: ešte nezmerané (meria sa pri dennom čistení cache)",
    { timeout: 10000 },
  );
  expect(realConsoleErrors()).toEqual([]);
});
