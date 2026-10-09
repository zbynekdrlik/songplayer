import { test, expect } from "@playwright/test";

// #242: a playlist's own volume + EQ, set on its Dashboard card.
//
// The panel "Zvuk playlistu" under the Player loads
// `GET /api/v1/playlists/{id}/audio`, edits the volume and the band list
// (add / remove, typ, frekvencia, zisk, Q, on/off), draws the response curve
// and saves with `PUT …/audio` (204). The mock records every PUT body
// (`/__mock/playlist-audio`). Zero console errors, per
// browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const PID = 1;

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  await request.post("/__mock/playlist-audio-reset");
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // Global in-memory mock state: never leak it into a later spec.
  await request.post("/__mock/playlist-audio-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

async function puts(request: import("@playwright/test").APIRequestContext) {
  const res = await request.get("/__mock/playlist-audio");
  return (await res.json()).puts as { id: number; body: any }[];
}

async function openPanel(page: import("@playwright/test").Page) {
  await page.goto(`/?playlist=${PID}`);
  const toggle = page.getByTestId("playlist-audio-toggle");
  await expect(toggle).toBeVisible({ timeout: 15000 });
  await expect(toggle).toHaveText("▶ Zvuk playlistu");
  await toggle.click();
  await expect(toggle).toHaveText("▼ Zvuk playlistu");
}

test("the panel shows the playlist's stored volume, bands and curve (#242)", async ({
  page,
  request,
}) => {
  await request.post("/__mock/playlist-audio-set", {
    data: {
      id: PID,
      fx: {
        gain_db: -5,
        eq: [{ kind: "low_shelf", freq_hz: 150, gain_db: -18, q: 0.71, enabled: true }],
      },
    },
  });
  await openPanel(page);

  await expect(page.getByTestId("playlist-audio-gain")).toHaveValue("-5", {
    timeout: 10000,
  });
  const band = page.getByTestId("playlist-audio-band");
  await expect(band).toHaveCount(1);
  // A stored kind that is NOT the select's first option (#233 trap).
  await expect(band.getByTestId("band-kind")).toHaveValue("low_shelf");
  await expect(band.getByTestId("band-freq")).toHaveValue("150");
  await expect(band.getByTestId("band-gain")).toHaveValue("-18");
  await expect(band.getByTestId("band-q")).toHaveValue("0.71");
  await expect(band.getByTestId("band-on")).toBeChecked();

  // The curve is not flat: a −18 dB bass shelf + −5 dB volume.
  const d = await page.getByTestId("playlist-audio-path").getAttribute("d");
  const ys = [...(d ?? "").matchAll(/[ML][\d.]+ ([\d.]+)/g)].map((m) => Number(m[1]));
  expect(ys.length).toBeGreaterThan(100);
  expect(Math.max(...ys) - Math.min(...ys)).toBeGreaterThan(20);
  // Nothing was saved by opening the panel.
  expect(await puts(request)).toEqual([]);
});

test("editing and Použiť sends the operator's own values, focus stays put (#242)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  const path = page.getByTestId("playlist-audio-path");
  await expect(page.getByTestId("playlist-audio-gain")).toHaveValue("0", {
    timeout: 10000,
  });
  await expect(page.getByTestId("playlist-audio-band")).toHaveCount(0);
  const flat = await path.getAttribute("d");

  await page.getByTestId("playlist-audio-gain").fill("-10");
  await page.getByTestId("playlist-audio-gain").press("Tab");

  // Two bands: a bass cut and a peak.
  await page.getByTestId("playlist-audio-add").click();
  await page.getByTestId("playlist-audio-add").click();
  const bands = page.getByTestId("playlist-audio-band");
  await expect(bands).toHaveCount(2);

  const first = bands.nth(0);
  await first.getByTestId("band-kind").selectOption("high_pass");
  await expect(first.getByTestId("band-gain")).toBeDisabled();
  await first.getByTestId("band-freq").fill("90");
  // A change re-renders nothing under the operator: Tab lands on the
  // next field of the same row (the Q, the gain being disabled).
  await first.getByTestId("band-freq").press("Tab");
  await expect(first.getByTestId("band-q")).toBeFocused();

  const second = bands.nth(1);
  await second.getByTestId("band-freq").fill("3000");
  await second.getByTestId("band-freq").press("Tab");
  await expect(second.getByTestId("band-gain")).toBeFocused();
  await second.getByTestId("band-gain").fill("4");
  await second.getByTestId("band-gain").press("Tab");
  await second.getByTestId("band-q").fill("1.5");
  await second.getByTestId("band-q").press("Tab");

  await expect(path).not.toHaveAttribute("d", flat ?? "");

  await page.getByTestId("playlist-audio-save").click();
  await expect(page.getByTestId("playlist-audio-status")).toHaveText("Použité", {
    timeout: 10000,
  });
  const sent = await puts(request);
  expect(sent).toHaveLength(1);
  expect(sent[0].id).toBe(PID);
  expect(sent[0].body.gain_db).toBe(-10);
  expect(sent[0].body.eq).toHaveLength(2);
  expect(sent[0].body.eq[0]).toMatchObject({ kind: "high_pass", freq_hz: 90, enabled: true });
  expect(sent[0].body.eq[1]).toMatchObject({
    kind: "peak",
    freq_hz: 3000,
    gain_db: 4,
    q: 1.5,
    enabled: true,
  });

  // A reload reads the saved settings back.
  await openPanel(page);
  await expect(page.getByTestId("playlist-audio-gain")).toHaveValue("-10", {
    timeout: 10000,
  });
  await expect(page.getByTestId("playlist-audio-band")).toHaveCount(2);
  await expect(
    page.getByTestId("playlist-audio-band").nth(1).getByTestId("band-freq"),
  ).toHaveValue("3000");
});

test("a value past a limit is refused in Slovak and nothing is sent (#242)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await expect(page.getByTestId("playlist-audio-gain")).toHaveValue("0", {
    timeout: 10000,
  });
  await page.getByTestId("playlist-audio-add").click();
  const band = page.getByTestId("playlist-audio-band");
  await band.getByTestId("band-freq").fill("5");
  await band.getByTestId("band-freq").press("Tab");
  await page.getByTestId("playlist-audio-save").click();
  await expect(page.getByTestId("playlist-audio-status")).toHaveText(
    "Pásmo 1: frekvencia musí byť od 20 do 20 000 Hz",
  );

  await band.getByTestId("band-freq").fill("100");
  await band.getByTestId("band-freq").press("Tab");
  await page.getByTestId("playlist-audio-gain").fill("-40");
  await page.getByTestId("playlist-audio-gain").press("Tab");
  await page.getByTestId("playlist-audio-save").click();
  await expect(page.getByTestId("playlist-audio-status")).toHaveText(
    "Hlasitosť musí byť od −30 do +12 dB",
  );
  expect(await puts(request)).toEqual([]);
});

test("a band switched off or removed leaves the curve and the save (#242)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await expect(page.getByTestId("playlist-audio-gain")).toHaveValue("0", {
    timeout: 10000,
  });
  const path = page.getByTestId("playlist-audio-path");
  const flat = await path.getAttribute("d");

  for (let i = 0; i < 8; i++) await page.getByTestId("playlist-audio-add").click();
  const bands = page.getByTestId("playlist-audio-band");
  await expect(bands).toHaveCount(8);
  await expect(page.getByTestId("playlist-audio-add")).toBeDisabled();

  // Remove all but the last; give it a boost, then switch it off.
  for (let i = 0; i < 7; i++) await bands.first().getByTestId("band-remove").click();
  await expect(bands).toHaveCount(1);
  await expect(page.getByTestId("playlist-audio-add")).toBeEnabled();
  await bands.first().getByTestId("band-gain").fill("6");
  await bands.first().getByTestId("band-gain").press("Tab");
  await expect(path).not.toHaveAttribute("d", flat ?? "");
  await bands.first().getByTestId("band-on").uncheck();
  await expect(path).toHaveAttribute("d", flat ?? "");

  await page.getByTestId("playlist-audio-save").click();
  await expect(page.getByTestId("playlist-audio-status")).toHaveText("Použité", {
    timeout: 10000,
  });
  const sent = await puts(request);
  expect(sent).toHaveLength(1);
  expect(sent[0].body.eq).toHaveLength(1);
  expect(sent[0].body.eq[0]).toMatchObject({ kind: "peak", gain_db: 6, enabled: false });
});
