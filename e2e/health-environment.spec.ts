import { test, expect } from "@playwright/test";

// #233 — while the box violates the production environment (an edition that
// is neither Windows LTSC nor IoT Enterprise, Defender's real-time protection
// on, a firewall profile on, or a fact the check could not read), the shared
// HealthBar shows a red "Prostredie: …" segment naming the first problem
// (and how many more), with the rule and every problem as its tooltip. It is
// absent while the box holds. The bar polls `GET /api/v1/environment` every
// 10 s, so a violation that appears or clears while the page is open shows
// up without a reload. Runs against the mock.

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

const DEFENDER = {
  code: "defender_realtime_on",
  text_sk: "ochrana Windows Defender v reálnom čase je zapnutá",
};
const FIREWALL_PUBLIC = {
  code: "firewall_on:public",
  text_sk: "brána firewall je zapnutá (verejný profil)",
};

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/environment-reset");
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/environment-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("the environment segment appears red while violated, names the problems, and goes when it holds", async ({
  page,
  request,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("health-bar")).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("health-version")).toBeVisible();
  // The box holds: no segment.
  await expect(page.getByTestId("health-environment")).toHaveCount(0);

  await request.post("/__mock/environment", {
    data: {
      note: null,
      ok: false,
      edition_id: "IoTEnterpriseS",
      ltsc: true,
      edition_allowed: true,
      defender_realtime: true,
      firewall: { domain: false, private: false, public: false },
      problems: [DEFENDER],
    },
  });
  const segment = page.getByTestId("health-environment");
  await expect(segment).toHaveText(
    "Prostredie: ochrana Windows Defender v reálnom čase je zapnutá",
    { timeout: 15000 },
  );
  await expect(segment).toHaveClass(/health-error/);
  const tip = await segment.getAttribute("title");
  expect(tip).toContain("Produkčný počítač musí bežať na Windows LTSC alebo Windows IoT Enterprise");
  expect(tip).toContain("Zistené: ochrana Windows Defender v reálnom čase je zapnutá.");

  // A second problem: the first is shown, the rest counted, all in the tooltip.
  await request.post("/__mock/environment", {
    data: { note: null, ok: false, problems: [DEFENDER, FIREWALL_PUBLIC] },
  });
  await expect(segment).toHaveText(
    "Prostredie: ochrana Windows Defender v reálnom čase je zapnutá (+1)",
    { timeout: 15000 },
  );
  await expect(segment).toHaveAttribute(
    "title",
    /Zistené: ochrana Windows Defender v reálnom čase je zapnutá; brána firewall je zapnutá \(verejný profil\)\.$/,
  );

  // Fixed on the box: the next poll drops the segment.
  await request.post("/__mock/environment-reset");
  await expect(page.getByTestId("health-environment")).toHaveCount(0, {
    timeout: 15000,
  });
});

test("the environment segment shows on every page's health bar", async ({
  page,
  request,
}) => {
  await request.post("/__mock/environment", {
    data: { note: null, ok: false, problems: [FIREWALL_PUBLIC] },
  });
  for (const path of ["/", "/settings", "/lyrics"]) {
    await page.goto(path);
    await expect(page.getByTestId("health-environment")).toHaveText(
      "Prostredie: brána firewall je zapnutá (verejný profil)",
      { timeout: 10000 },
    );
  }
});
