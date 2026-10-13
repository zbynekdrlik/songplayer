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
  // Three changes, each seen at the bar's next 10 s poll.
  test.setTimeout(90_000);
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  await expect(bar).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("health-version")).toBeVisible();
  // The box holds: the poll landed and said so, and there is no segment
  // (asserted only once the marker proves the poll's answer rendered).
  await expect(bar).toHaveAttribute("data-environment", "ok", { timeout: 15000 });
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
  await expect(bar).toHaveAttribute("data-environment", "violated");
  const tip = await segment.getAttribute("title");
  expect(tip).toContain(
    "Produkčný počítač musí bežať na Windows LTSC alebo Windows 11 IoT Enterprise, s vypnutou " +
      "ochranou Windows Defender v reálnom čase (ani v pasívnom režime), bez iného antivírusu",
  );
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

  // Fixed on the box: the next poll says so and drops the segment.
  await request.post("/__mock/environment-reset");
  await expect(bar).toHaveAttribute("data-environment", "ok", { timeout: 15000 });
  await expect(page.getByTestId("health-environment")).toHaveCount(0);
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
    await expect(page.getByTestId("health-bar")).toHaveAttribute("data-environment", "violated", {
      timeout: 10000,
    });
    await expect(page.getByTestId("health-environment")).toHaveText(
      "Prostredie: brána firewall je zapnutá (verejný profil)",
      { timeout: 10000 },
    );
  }
});

// #255: the production PC's audio profile. Its problems (the server's codes
// and Slovak texts, `sp_core::prod_env::EnvProblem`) show on the same red
// segment; its report-only facts are a grey "Profil PC" segment that never
// makes the box fail.
const SG_NETADAPTERCX = {
  code: "soundgrid_nic_netadaptercx",
  text_sk:
    "sieťová karta SoundGrid „soundgrid-pcie“ beží na ovládači NetAdapterCx (musí bežať na ovládači NDIS)",
};
const SG_EEE = {
  code: "soundgrid_nic_setting:eee",
  text_sk:
    "sieťová karta SoundGrid „soundgrid-pcie“: nastavenie „Energy Efficient Ethernet“ nie je vypnuté (hodnota 1)",
};
const SG_PORT_UNSET = {
  code: "soundgrid_port_unset",
  text_sk: "SoundGrid nemá zvolený sieťový port — nastav ho v SoundGrid Studio / ovládači",
};
const SG_PORT_MISSING = {
  code: "soundgrid_port_missing",
  text_sk: "sieťová karta s MAC 6c:24:08:e5:9c:91, ktorú má ovládač SoundGrid zvolenú ako port, sa nenašla",
};
const AUTO_UPDATE = {
  code: "windows_auto_update_on",
  text_sk: "automatické aktualizácie Windows nie sú vypnuté (NoAutoUpdate)",
};
const MMCSS = {
  code: "mmcss:system_responsiveness",
  text_sk: "MMCSS: SystemResponsiveness je 20 (má byť 0)",
};
const DVS_NOTE =
  "sieťová karta Dante (DVS) „Ethernet 4“ beží na ovládači NetAdapterCx (vplyv sa ešte meria)";
const WU_NOTE =
  "Windows Update nemá vylúčené ovládače (ExcludeWUDriversInQualityUpdate; nastaví ho príprava PC)";

test("#255: an audio-profile problem is named on the red segment", async ({ page, request }) => {
  await request.post("/__mock/environment", {
    data: { note: null, ok: false, problems: [SG_NETADAPTERCX, SG_EEE, AUTO_UPDATE, MMCSS] },
  });
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  await expect(bar).toHaveAttribute("data-environment", "violated", { timeout: 10000 });
  const segment = page.getByTestId("health-environment");
  await expect(segment).toHaveText(`Prostredie: ${SG_NETADAPTERCX.text_sk} (+3)`, {
    timeout: 10000,
  });
  await expect(segment).toHaveClass(/health-error/);
  const tip = await segment.getAttribute("title");
  expect(tip).toContain(
    "Sieťová karta, ktorú má ovládač SoundGrid zvolenú ako port, beží na ovládači NDIS s vypnutým " +
      "moderovaním prerušení, riadením toku a úsporou energie, automatické aktualizácie Windows sú " +
      "vypnuté a MMCSS je nastavené pre Pro Audio.",
  );
  expect(tip).toContain(
    `Zistené: ${SG_NETADAPTERCX.text_sk}; ${SG_EEE.text_sk}; ${AUTO_UPDATE.text_sk}; ${MMCSS.text_sk}.`,
  );
});

test("#255: SoundGrid with no port chosen, or a port no adapter has, is named", async ({
  page,
  request,
}) => {
  test.setTimeout(60_000);
  await request.post("/__mock/environment", {
    data: { note: null, ok: false, problems: [SG_PORT_UNSET] },
  });
  await page.goto("/");
  const segment = page.getByTestId("health-environment");
  await expect(segment).toHaveText(`Prostredie: ${SG_PORT_UNSET.text_sk}`, { timeout: 10000 });
  await expect(segment).toHaveClass(/health-error/);
  // The port chosen, but on no adapter: the next poll names its MAC.
  await request.post("/__mock/environment", {
    data: { note: null, ok: false, problems: [SG_PORT_MISSING] },
  });
  await expect(segment).toHaveText(`Prostredie: ${SG_PORT_MISSING.text_sk}`, { timeout: 15000 });
  await expect(page.getByTestId("health-bar")).toHaveAttribute("data-environment", "violated");
});

test("#255: report-only notes are a grey segment and the box still holds", async ({ page, request }) => {
  test.setTimeout(60_000);
  await request.post("/__mock/environment", {
    data: {
      note: null,
      ok: true,
      problems: [],
      audio: {
        soundgrid: "not_installed",
        notes: [DVS_NOTE, WU_NOTE],
        power: {
          scheme_guid: "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c",
          scheme_name: "High performance",
          core_parking_min_cores_pct: 100,
        },
      },
    },
  });
  await page.goto("/");
  const bar = page.getByTestId("health-bar");
  // The marker proves the poll's answer rendered; the red segment stays away.
  await expect(bar).toHaveAttribute("data-environment", "ok", { timeout: 10000 });
  await expect(page.getByTestId("health-environment")).toHaveCount(0);
  const notes = page.getByTestId("health-environment-notes");
  await expect(notes).toHaveText("Profil PC: 2 poznámky", { timeout: 10000 });
  await expect(notes).toHaveClass(/health-off/);
  await expect(notes).toHaveAttribute(
    "title",
    `Len hlásené, nie je to chyba (#255): ${DVS_NOTE}; ${WU_NOTE}. Plán napájania: High performance ` +
      "(8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c).",
  );
  // One note left: the count follows at the next 10 s poll.
  await request.post("/__mock/environment", {
    data: { note: null, ok: true, problems: [], audio: { notes: [DVS_NOTE] } },
  });
  await expect(notes).toHaveText("Profil PC: 1 poznámka", { timeout: 15000 });
  await expect(notes).toHaveAttribute("title", `Len hlásené, nie je to chyba (#255): ${DVS_NOTE}.`);
  // None: the segment goes.
  await request.post("/__mock/environment-reset");
  await expect(page.getByTestId("health-environment-notes")).toHaveCount(0, { timeout: 15000 });
  await expect(bar).toHaveAttribute("data-environment", "ok");
});
