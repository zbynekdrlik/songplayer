import { test, expect, APIRequestContext, Locator, Page } from "@playwright/test";

// #242 / #278: a playlist's own volume + EQ on its Dashboard card, edited
// live on a real curve (design comment 6221171333 on #278).
//
// The panel "Zvuk playlistu" under the Player loads
// `GET /api/v1/playlists/{id}/audio`; every change (a handle dragged on the
// curve, a slider, a switch, a type, a reset, the volume) goes out as
// `PUT …/audio` (204) at once — no "Použiť": at most one PUT per 50 ms, the
// latest settings winning, one request in flight, the drag's end flushed.
// The mock records every PUT with its arrival and answer times
// (`/__mock/playlist-audio`); `/__mock/playlist-audio-put-delay` slows the
// answers. Every drag is a REAL mouse drag (page.mouse, sp-ui-frontend.md
// #200). Zero console errors or warnings is the last assertion of every
// test (afterEach).

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const PID = 1;

type Band = {
  kind: string;
  freq_hz: number;
  gain_db: number;
  q: number;
  enabled: boolean;
};
type Put = { id: number; body: { gain_db: number; eq: Band[] }; at: number; done: number | null };

// The mouse drags need the whole card on screen (sp-ui-frontend.md).
test.use({ viewport: { width: 1600, height: 1000 } });

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
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

/** sp_core::audio_fx_view::freq_label: three significant digits, a comma. */
function freqLabel(hz: number): string {
  if (hz < 1000) return `${hz} Hz`;
  return `${String(hz / 1000).replace(".", ",")} kHz`;
}

/** sp_core::audio_fx_view::gain_label (for an already snapped value). */
function gainLabel(db: number): string {
  const sign = db > 0 ? "+" : db < 0 ? "−" : "";
  return `${sign}${Math.abs(db).toFixed(1).replace(".", ",")} dB`;
}

async function puts(request: APIRequestContext): Promise<Put[]> {
  const res = await request.get("/__mock/playlist-audio");
  return (await res.json()).puts as Put[];
}

async function lastPut(request: APIRequestContext): Promise<Put["body"] | null> {
  const res = await request.get("/__mock/playlist-audio");
  return (await res.json()).last;
}

async function openPanel(page: Page) {
  await page.goto(`/?playlist=${PID}`);
  const toggle = page.getByTestId("playlist-audio-toggle");
  await expect(toggle).toBeVisible({ timeout: 15000 });
  await expect(toggle).toHaveText("▶ Zvuk playlistu");
  await toggle.click();
  await expect(toggle).toHaveText("▼ Zvuk playlistu");
  await expect(page.getByTestId("playlist-audio-curve")).toBeVisible({ timeout: 10000 });
}

/** The numbers a card carries for its band (its `data-*`). */
async function cardBand(card: Locator): Promise<Band> {
  const num = async (name: string) => Number(await card.getAttribute(name));
  return {
    kind: (await card.getAttribute("data-kind")) ?? "",
    freq_hz: await num("data-freq"),
    gain_db: await num("data-gain"),
    q: await num("data-q"),
    enabled: (await card.getAttribute("data-enabled")) === "true",
  };
}

/** Where a handle's dot is drawn now, in page px (its centre). */
async function dotCentre(page: Page, handle: Locator): Promise<{ x: number; y: number }> {
  await page.getByTestId("playlist-audio-curve").scrollIntoViewIfNeeded();
  const box = await handle.locator("circle").boundingBox();
  if (!box) throw new Error("the handle has no box");
  return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

/** A real mouse drag of a handle by (dx, dy) px. */
async function dragHandle(page: Page, handle: Locator, dx: number, dy: number, steps = 10) {
  const from = await dotCentre(page, handle);
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(from.x + dx, from.y + dy, { steps });
  await page.mouse.up();
}

/** A real mouse drag along a horizontal range input, by fractions of its width. */
async function dragSlider(page: Page, slider: Locator, from: number, to: number) {
  await slider.scrollIntoViewIfNeeded();
  const box = await slider.boundingBox();
  if (!box) throw new Error("the slider has no box");
  const y = box.y + box.height / 2;
  await page.mouse.move(box.x + box.width * from, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * to, y, { steps: 8 });
  await page.mouse.up();
}

/** Add a band of `kind` and wait until its PUT is in. */
async function addBand(page: Page, request: APIRequestContext, kind: string) {
  const before = (await puts(request)).length;
  await page.getByTestId(`playlist-audio-add-${kind}`).click();
  await expect.poll(async () => (await puts(request)).length, { timeout: 5000 }).toBeGreaterThan(before);
}

function ys(d: string | null): number[] {
  return [...(d ?? "").matchAll(/[ML][\d.]+ ([\d.]+)/g)].map((m) => Number(m[1]));
}

test("the panel shows the stored volume, the bands by frequency and the curve (#242, #278)", async ({
  page,
  request,
}) => {
  await request.post("/__mock/playlist-audio-set", {
    data: {
      id: PID,
      fx: {
        gain_db: -5,
        eq: [
          { kind: "peak", freq_hz: 3000, gain_db: 4, q: 1.5, enabled: false },
          { kind: "low_shelf", freq_hz: 150, gain_db: -18, q: 0.71, enabled: true },
        ],
      },
    },
  });
  await openPanel(page);

  await expect(page.getByTestId("playlist-audio-gain-value")).toHaveText("−5,0 dB");
  const cards = page.getByTestId("playlist-audio-band");
  await expect(cards).toHaveCount(2);
  // By frequency: the 150 Hz shelf (stored second) is card 1.
  const shelf = cards.nth(0);
  await expect(shelf).toHaveAttribute("data-index", "1");
  await expect(shelf.getByTestId("band-number")).toHaveText("1");
  // A stored kind that is NOT the select's first option (#233 trap).
  await expect(shelf.getByTestId("band-kind")).toHaveValue("low_shelf");
  await expect(shelf.getByTestId("band-freq-value")).toHaveText("150 Hz");
  await expect(shelf.getByTestId("band-gain-value")).toHaveText("−18,0 dB");
  await expect(shelf.getByTestId("band-q-value")).toHaveText("Q 0,71");
  await expect(shelf.getByTestId("band-on")).toBeChecked();
  const peak = cards.nth(1);
  await expect(peak).toHaveAttribute("data-index", "0");
  await expect(peak.getByTestId("band-number")).toHaveText("2");
  await expect(peak.getByTestId("band-freq-value")).toHaveText("3 kHz");
  await expect(peak.getByTestId("band-q-value")).toHaveText("Q 1,50");
  await expect(peak.getByTestId("band-on")).not.toBeChecked();

  // The summed curve is not flat: a −18 dB bass shelf + −5 dB volume.
  const path = ys(await page.getByTestId("playlist-audio-path").getAttribute("d"));
  expect(path.length).toBeGreaterThan(100);
  expect(Math.max(...path) - Math.min(...path)).toBeGreaterThan(20);
  await expect(page.getByTestId("playlist-audio-fill")).toHaveAttribute("d", /^M0\.0 .* Z$/);
  // Each band's own curve: the shelf's drawn, the switched-off peak's empty.
  const curves = page.getByTestId("playlist-audio-band-curve");
  await expect(curves).toHaveCount(2);
  await expect(curves.and(page.locator('[data-index="1"]'))).toHaveAttribute("d", /^M0\.0 /);
  await expect(curves.and(page.locator('[data-index="0"]'))).toHaveAttribute("d", "");
  // One handle per band, numbered like its card, the off one dimmed.
  const handles = page.getByTestId("playlist-audio-handle");
  await expect(handles).toHaveCount(2);
  const offHandle = handles.and(page.locator('[data-index="0"]'));
  await expect(offHandle).toHaveAttribute("class", /\boff\b/);
  await expect(offHandle).toHaveAttribute("data-kind", "peak");
  await expect(offHandle.locator("text")).toHaveText("2");
  // Nothing was saved by opening the panel, and there is no save button.
  await expect(page.getByTestId("playlist-audio-save")).toHaveCount(0);
  expect(await puts(request)).toEqual([]);
});

/// The panel offers no field before the stored sound has loaded: an edit
/// made then would be overwritten when the load lands (CI run 37975959175).
test("the panel shows its fields only once the stored sound is loaded (#242)", async ({
  page,
  request,
}) => {
  await request.post("/__mock/playlist-audio-set", {
    data: { id: PID, fx: { gain_db: -5, eq: [] } },
  });
  await request.post("/__mock/playlist-audio-delay", { data: { ms: 1500 } });
  await page.goto(`/?playlist=${PID}`);
  await page.getByTestId("playlist-audio-toggle").click({ timeout: 15000 });
  await expect(page.getByTestId("playlist-audio-loading")).toHaveText("Načítavam…");
  await expect(page.getByTestId("playlist-audio-gain")).toHaveCount(0);
  await expect(page.getByTestId("playlist-audio-add-peak")).toHaveCount(0);
  await expect(page.getByTestId("playlist-audio-gain-value")).toHaveText("−5,0 dB", {
    timeout: 10000,
  });
  await expect(page.getByTestId("playlist-audio-loading")).toHaveCount(0);
  expect(await puts(request)).toEqual([]);
});

test("a new high-pass starts at 80 Hz and is sent at once, with no button (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await expect(page.getByTestId("playlist-audio-band")).toHaveCount(0);
  await page.getByTestId("playlist-audio-add-high_pass").click();
  const card = page.getByTestId("playlist-audio-band");
  await expect(card).toHaveCount(1);
  await expect(card.getByTestId("band-kind")).toHaveValue("high_pass");
  await expect(card.getByTestId("band-freq-value")).toHaveText("80 Hz");
  await expect(card.getByTestId("band-q-value")).toHaveText("Q 0,71");
  // A high-pass has no gain to set.
  await expect(card.getByTestId("band-gain")).toBeDisabled();
  await expect(card.getByTestId("band-gain-value")).toHaveText("—");

  await expect.poll(() => lastPut(request), { timeout: 5000 }).toEqual({
    gain_db: 0,
    eq: [{ kind: "high_pass", freq_hz: 80, gain_db: 0, q: Math.SQRT1_2, enabled: true }],
  });
  await expect(page.getByTestId("playlist-audio-status")).toHaveText("Uložené");
  // Each type is born with its own frequency.
  for (const [kind, label] of [
    ["low_shelf", "120 Hz"],
    ["peak", "1 kHz"],
    ["high_shelf", "8 kHz"],
    ["low_pass", "16 kHz"],
  ]) {
    await addBand(page, request, kind);
    await expect(
      page.getByTestId("playlist-audio-band").and(page.locator(`[data-kind="${kind}"]`)).getByTestId(
        "band-freq-value",
      ),
    ).toHaveText(label);
  }
  // By frequency: 80 Hz, 120 Hz, 1 kHz, 8 kHz, 16 kHz.
  const kinds = await page
    .getByTestId("playlist-audio-band")
    .evaluateAll((cards) => cards.map((c) => c.getAttribute("data-kind")));
  expect(kinds).toEqual(["high_pass", "low_shelf", "peak", "high_shelf", "low_pass"]);
  const sent = (await lastPut(request))!;
  expect(sent.eq.map((b) => [b.kind, b.freq_hz])).toEqual([
    ["high_pass", 80],
    ["low_shelf", 120],
    ["peak", 1000],
    ["high_shelf", 8000],
    ["low_pass", 16000],
  ]);
});

test("dragging a peak's handle moves its frequency and gain live; the release is flushed (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  const handle = page.getByTestId("playlist-audio-handle");
  const card = page.getByTestId("playlist-audio-band");
  const dPath = page.getByTestId("playlist-audio-path");
  const flat = await dPath.getAttribute("d");
  const cxBefore = Number(await handle.locator("circle").getAttribute("cx"));
  const cyBefore = Number(await handle.locator("circle").getAttribute("cy"));
  const sentBefore = (await puts(request)).length;

  await dragHandle(page, handle, 150, -40, 12);

  // The release flushed the last position: the last PUT IS the card.
  const final = await cardBand(card);
  expect(final.kind).toBe("peak");
  expect(final.freq_hz).toBeGreaterThan(1000);
  expect(final.gain_db).toBeGreaterThan(2);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({
    gain_db: 0,
    eq: [final],
  });
  await expect(card.getByTestId("band-freq-value")).toHaveText(freqLabel(final.freq_hz));
  await expect(card.getByTestId("band-gain-value")).toHaveText(gainLabel(final.gain_db));
  // The handle and the curve moved with it.
  expect(Number(await handle.locator("circle").getAttribute("cx"))).toBeGreaterThan(cxBefore + 50);
  expect(Number(await handle.locator("circle").getAttribute("cy"))).toBeLessThan(cyBefore - 10);
  await expect(dPath).not.toHaveAttribute("d", flat ?? "");
  // Live during the drag, throttled: more than the release's one PUT, and
  // every PUT but the flushed last starts ≥ 50 ms after the one before it
  // (40 ms: the mock's clock reads arrivals).
  const sent = (await puts(request)).slice(sentBefore);
  expect(sent.length).toBeGreaterThan(1);
  for (let i = 1; i < sent.length - 1; i++) {
    expect(sent[i].at - sent[i - 1].at, `PUT ${i}`).toBeGreaterThanOrEqual(40);
  }
  await expect(page.getByTestId("playlist-audio-status")).toHaveText("Uložené");
});

test("after a drag the cards take the frequency order, moved and never re-created (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "low_shelf");
  await addBand(page, request, "peak");
  const cards = page.getByTestId("playlist-audio-band");
  await expect(cards.nth(1)).toHaveAttribute("data-kind", "peak");
  const peakCard = await cards.nth(1).elementHandle();
  const peakHandle = page.getByTestId("playlist-audio-handle").and(page.locator('[data-kind="peak"]'));
  await expect(peakHandle.locator("text")).toHaveText("2");
  // Drag the 1 kHz peak far left (to ~70 Hz), below the 120 Hz shelf.
  const box = await page.getByTestId("playlist-audio-curve").boundingBox();
  await dragHandle(page, peakHandle, -(box!.width * 0.38), 0);
  const peak = await cardBand(cards.nth(0));
  expect(peak.kind).toBe("peak");
  expect(peak.freq_hz).toBeLessThan(120);
  // The same element, moved to the top: nothing re-created under the operator.
  expect(await peakCard!.evaluate((el) => el.isConnected)).toBe(true);
  expect(await cards.nth(0).evaluate((el, old) => el === old, peakCard)).toBe(true);
  await expect(cards.nth(0).getByTestId("band-number")).toHaveText("1");
  await expect(peakHandle.locator("text")).toHaveText("1");
});

test("a high-pass handle moves on its frequency only (#278)", async ({ page, request }) => {
  await openPanel(page);
  await addBand(page, request, "high_pass");
  const handle = page.getByTestId("playlist-audio-handle");
  const cyBefore = await handle.locator("circle").getAttribute("cy");
  await dragHandle(page, handle, 120, -60);
  const final = await cardBand(page.getByTestId("playlist-audio-band"));
  expect(final.freq_hz).toBeGreaterThan(80);
  expect(final.gain_db).toBe(0);
  await expect(handle.locator("circle")).toHaveAttribute("cy", cyBefore ?? "");
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [final] });
});

test("a gain slider moved with the mouse is sent within a second (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  const card = page.getByTestId("playlist-audio-band");
  // 0 dB sits at 30/42 of the −30 … +12 dB slider; move up to ~90 %.
  await dragSlider(page, card.getByTestId("band-gain"), 30 / 42, 0.9);
  const final = await cardBand(card);
  expect(final.gain_db).toBeGreaterThan(3);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [final] });
  await expect(card.getByTestId("band-gain-value")).toHaveText(gainLabel(final.gain_db));
  // The volume slider too.
  await dragSlider(page, page.getByTestId("playlist-audio-gain"), 30 / 42, 0.3);
  await expect
    .poll(async () => (await lastPut(request))?.gain_db, { timeout: 1000 })
    .toBeLessThan(-5);
});

test("a double-click on a value resets it to the type's default (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "low_shelf");
  const card = page.getByTestId("playlist-audio-band");
  await dragHandle(page, page.getByTestId("playlist-audio-handle"), 100, -30);
  const moved = await cardBand(card);
  expect(moved.freq_hz).toBeGreaterThan(120);
  expect(moved.gain_db).toBeGreaterThan(0);

  await card.getByTestId("band-freq-value").dblclick();
  await expect(card.getByTestId("band-freq-value")).toHaveText("120 Hz");
  await expect
    .poll(() => lastPut(request), { timeout: 1000 })
    .toEqual({ gain_db: 0, eq: [{ ...moved, freq_hz: 120 }] });

  await card.getByTestId("band-gain-value").dblclick();
  await expect(card.getByTestId("band-gain-value")).toHaveText("0,0 dB");
  await expect
    .poll(() => lastPut(request), { timeout: 1000 })
    .toEqual({ gain_db: 0, eq: [{ ...moved, freq_hz: 120, gain_db: 0 }] });

  // The volume back to 0 dB.
  await dragSlider(page, page.getByTestId("playlist-audio-gain"), 30 / 42, 0.2);
  await expect.poll(async () => (await lastPut(request))?.gain_db, { timeout: 1000 }).toBeLessThan(-5);
  await page.getByTestId("playlist-audio-gain-value").dblclick();
  await expect(page.getByTestId("playlist-audio-gain-value")).toHaveText("0,0 dB");
  await expect.poll(async () => (await lastPut(request))?.gain_db, { timeout: 1000 }).toBe(0);
});

test("a type change takes the type's frequency and Q and keeps the gain (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  const card = page.getByTestId("playlist-audio-band");
  await dragHandle(page, page.getByTestId("playlist-audio-handle"), 0, -30);
  const moved = await cardBand(card);
  expect(moved.gain_db).toBeGreaterThan(0);
  await card.getByTestId("band-kind").selectOption("high_shelf");
  await expect(card.getByTestId("band-freq-value")).toHaveText("8 kHz");
  await expect(card.getByTestId("band-q-value")).toHaveText("Q 0,71");
  await expect(card.getByTestId("band-gain-value")).toHaveText(gainLabel(moved.gain_db));
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({
    gain_db: 0,
    eq: [{ kind: "high_shelf", freq_hz: 8000, gain_db: moved.gain_db, q: Math.SQRT1_2, enabled: true }],
  });
});

test("the wheel over a handle changes its Q (#278)", async ({ page, request }) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  const centre = await dotCentre(page, page.getByTestId("playlist-audio-handle"));
  await page.mouse.move(centre.x, centre.y);
  await page.mouse.wheel(0, -100);
  await page.mouse.wheel(0, -100);
  const card = page.getByTestId("playlist-audio-band");
  await expect(card.getByTestId("band-q-value")).toHaveText("Q 1,21");
  await expect
    .poll(async () => (await lastPut(request))?.eq[0]?.q, { timeout: 1000 })
    .toBe(1.21);
});

test("one request in flight: a slow server gets the PUTs one after another, the latest last (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  await request.post("/__mock/playlist-audio-put-delay", { data: { ms: 300 } });
  const sentBefore = (await puts(request)).length;
  await dragHandle(page, page.getByTestId("playlist-audio-handle"), 200, -50, 40);
  const final = await cardBand(page.getByTestId("playlist-audio-band"));
  await expect.poll(() => lastPut(request), { timeout: 5000 }).toEqual({ gain_db: 0, eq: [final] });
  await expect
    .poll(async () => (await puts(request)).every((p) => p.done !== null), { timeout: 5000 })
    .toBe(true);
  const sent = (await puts(request)).slice(sentBefore);
  expect(sent.length).toBeGreaterThan(1);
  for (let i = 1; i < sent.length; i++) {
    expect(sent[i].at, `PUT ${i} started before PUT ${i - 1} was answered`).toBeGreaterThanOrEqual(
      sent[i - 1].done!,
    );
  }
});

test("a band switched off or removed leaves the curve and is sent live (#242, #278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  const path = page.getByTestId("playlist-audio-path");
  const flat = await path.getAttribute("d");

  for (let i = 0; i < 8; i++) await page.getByTestId("playlist-audio-add-peak").click();
  const cards = page.getByTestId("playlist-audio-band");
  await expect(cards).toHaveCount(8);
  for (const kind of ["high_pass", "low_shelf", "peak", "high_shelf", "low_pass"]) {
    await expect(page.getByTestId(`playlist-audio-add-${kind}`)).toBeDisabled();
  }
  await expect.poll(async () => (await lastPut(request))?.eq.length, { timeout: 5000 }).toBe(8);

  // Remove all but one; boost it, then switch it off.
  for (let i = 0; i < 7; i++) await cards.first().getByTestId("band-remove").click();
  await expect(cards).toHaveCount(1);
  await expect(page.getByTestId("playlist-audio-add-peak")).toBeEnabled();
  await dragHandle(page, page.getByTestId("playlist-audio-handle"), 0, -40);
  await expect(path).not.toHaveAttribute("d", flat ?? "");
  await cards.first().getByTestId("band-on").uncheck();
  await expect(path).toHaveAttribute("d", flat ?? "");
  await expect(page.getByTestId("playlist-audio-band-curve")).toHaveAttribute("d", "");
  const off = await cardBand(cards.first());
  expect(off.enabled).toBe(false);
  expect(off.gain_db).toBeGreaterThan(0);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [off] });

  await cards.first().getByTestId("band-remove").click();
  await expect(cards).toHaveCount(0);
  await expect(page.getByTestId("playlist-audio-handle")).toHaveCount(0);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [] });
});
