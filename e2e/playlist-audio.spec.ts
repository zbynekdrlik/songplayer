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
  const box = await handle.locator("circle.eq-handle-grip").boundingBox();
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

test("dragging a peak's handle moves its frequency and gain live, throttled, before the release (#278)", async ({
  page,
  request,
}) => {
  // The page's own send instants (performance.now() at each fetch of a PUT
  // …/audio): the throttle is the page's rule, so it is timed where it runs,
  // never by the mock's arrival clock (a loaded runner delays arrivals).
  await page.addInitScript(() => {
    const sends: number[] = [];
    (window as unknown as { __audioSends: number[] }).__audioSends = sends;
    const original = window.fetch.bind(window);
    window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      const method = (init?.method ?? (input instanceof Request ? input.method : "GET")).toUpperCase();
      if (method === "PUT" && url.endsWith("/audio")) sends.push(performance.now());
      return original(input, init);
    };
  });
  await openPanel(page);
  await addBand(page, request, "peak");
  const handle = page.getByTestId("playlist-audio-handle");
  const card = page.getByTestId("playlist-audio-band");
  const dPath = page.getByTestId("playlist-audio-path");
  const flat = await dPath.getAttribute("d");
  const sendsNow = () =>
    page.evaluate(() => (window as unknown as { __audioSends: number[] }).__audioSends.slice());
  const sendsBefore = (await sendsNow()).length;
  expect(sendsBefore, "the new band's own PUT went through the page's fetch").toBeGreaterThanOrEqual(1);
  const cxBefore = Number(await handle.locator("circle.eq-handle-dot").getAttribute("cx"));
  const cyBefore = Number(await handle.locator("circle.eq-handle-dot").getAttribute("cy"));
  const sentBefore = (await puts(request)).length;

  // 20 quick moves with the button still down.
  const from = await dotCentre(page, handle);
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(from.x + 150, from.y - 40, { steps: 20 });

  // Live: with the button STILL down, the server already has the latest
  // position (the throttle's timer sends it, no release needed).
  const held = await cardBand(card);
  expect(held.kind).toBe("peak");
  expect(held.freq_hz).toBeGreaterThan(1000);
  expect(held.gain_db).toBeGreaterThan(2);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({
    gain_db: 0,
    eq: [held],
  });
  // Throttled: 20 changes, fewer PUTs, the gate deciding each ≥ 50 ms after
  // the one before. The fetch is stamped a little after its decision (the
  // send runs in a spawned task, behind whatever the page had queued), so
  // 40: a broken throttle sends as fast as the moves come, far closer.
  const live = (await puts(request)).slice(sentBefore);
  expect(live.length).toBeGreaterThanOrEqual(1);
  expect(live.length, "20 moves, throttled").toBeLessThan(20);
  const sends = (await sendsNow()).slice(sendsBefore);
  expect(sends.length, "every PUT the mock saw was sent by the page").toBe(live.length);
  for (let i = 1; i < sends.length; i++) {
    expect(sends[i] - sends[i - 1], `send ${i}`).toBeGreaterThanOrEqual(40);
  }
  await page.mouse.up();

  // The release: the last PUT is still the card's band.
  const final = await cardBand(card);
  expect(final).toEqual(held);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({
    gain_db: 0,
    eq: [final],
  });
  await expect(card.getByTestId("band-freq-value")).toHaveText(freqLabel(final.freq_hz));
  await expect(card.getByTestId("band-gain-value")).toHaveText(gainLabel(final.gain_db));
  // The handle and the curve moved with it.
  expect(Number(await handle.locator("circle.eq-handle-dot").getAttribute("cx"))).toBeGreaterThan(cxBefore + 50);
  expect(Number(await handle.locator("circle.eq-handle-dot").getAttribute("cy"))).toBeLessThan(cyBefore - 10);
  await expect(dPath).not.toHaveAttribute("d", flat ?? "");
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

test("a card moved by a keyboard edit keeps its focus (#278)", async ({ page, request }) => {
  await request.post("/__mock/playlist-audio-set", {
    data: {
      id: PID,
      fx: {
        gain_db: 0,
        eq: [
          { kind: "peak", freq_hz: 50, gain_db: 3, q: 1, enabled: true },
          { kind: "low_shelf", freq_hz: 120, gain_db: -2, q: Math.SQRT1_2, enabled: true },
          { kind: "peak", freq_hz: 1000, gain_db: 0, q: 1, enabled: true },
        ],
      },
    },
  });
  await openPanel(page);
  const cards = page.getByTestId("playlist-audio-band");
  await expect(cards.nth(2)).toHaveAttribute("data-index", "2");
  const slider = cards.nth(2).getByTestId("band-freq");
  await slider.focus();
  // Home: the 1 kHz peak goes to 20 Hz, below the other two: its card
  // moves to the top, and the slider keeps the focus.
  await page.keyboard.press("Home");
  await expect(cards.nth(0)).toHaveAttribute("data-index", "2");
  await expect(cards.nth(0).getByTestId("band-freq-value")).toHaveText("20 Hz");
  await expect(cards.nth(0).getByTestId("band-freq")).toBeFocused();
  await expect
    .poll(async () => (await lastPut(request))?.eq[2]?.freq_hz, { timeout: 1000 })
    .toBe(20);
  // The keyboard goes on working on the same slider.
  await page.keyboard.press("PageUp");
  await expect(cards.nth(0).getByTestId("band-freq-value")).toHaveText("40 Hz");
  await expect(cards.nth(0).getByTestId("band-freq")).toBeFocused();
  await expect
    .poll(async () => (await lastPut(request))?.eq[2]?.freq_hz, { timeout: 1000 })
    .toBe(40);
});

test("a high-pass handle moves on its frequency only (#278)", async ({ page, request }) => {
  await openPanel(page);
  await addBand(page, request, "high_pass");
  const handle = page.getByTestId("playlist-audio-handle");
  const cyBefore = await handle.locator("circle.eq-handle-dot").getAttribute("cy");
  await dragHandle(page, handle, 120, -60);
  const final = await cardBand(page.getByTestId("playlist-audio-band"));
  expect(final.freq_hz).toBeGreaterThan(80);
  expect(final.gain_db).toBe(0);
  await expect(handle.locator("circle.eq-handle-dot")).toHaveAttribute("cy", cyBefore ?? "");
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [final] });
});

test("a handle taken off its centre moves by the pointer's travel, never jumps to it (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  const handle = page.getByTestId("playlist-audio-handle");
  const card = page.getByTestId("playlist-audio-band");
  const curve = page.getByTestId("playlist-audio-curve");
  await curve.scrollIntoViewIfNeeded();
  const drawn = await curve.boundingBox();
  const grip = await handle.locator("circle.eq-handle-grip").boundingBox();
  if (!drawn || !grip) throw new Error("the curve or the grip has no box");
  // Inside the grip, well off the dot: 0.3 of its width right, 0.2 up.
  const at = { x: grip.x + grip.width * 0.8, y: grip.y + grip.height * 0.3 };
  const sentBefore = (await puts(request)).length;
  await page.mouse.move(at.x, at.y);
  await page.mouse.down();
  await page.mouse.move(at.x + 4, at.y);
  // 4 px of travel: the frequency by 4 px's worth of the log axis (3
  // decades over the curve's width), the gain not at all. A band that
  // jumped to the pointer would land ≥ 12 % higher and ~1,6 dB up.
  await expect.poll(async () => (await cardBand(card)).freq_hz, { timeout: 2000 }).not.toBe(1000);
  const nudged = await cardBand(card);
  const travel = Math.exp((Math.log(1000) * 4) / drawn.width);
  expect(Math.abs(nudged.gain_db), "the gain stays").toBeLessThanOrEqual(0.2);
  expect(nudged.freq_hz / 1000, "moved by the travel").toBeGreaterThanOrEqual(1);
  expect(nudged.freq_hz / 1000, "moved by the travel").toBeLessThanOrEqual(travel + 0.011);
  // The rest of the drag carries it by the travel.
  await page.mouse.move(at.x + 80, at.y - 40, { steps: 5 });
  await page.mouse.up();
  const final = await cardBand(card);
  expect(final.freq_hz).toBeGreaterThan(1100);
  expect(final.gain_db).toBeGreaterThan(1);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [final] });
  expect((await puts(request)).length).toBeGreaterThan(sentBefore);
});

test("every slider moved with the mouse is sent within a second, no button (#278)", async ({
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
  // The log frequency slider: 1 kHz sits at 0.566 of it; up to ~85 %.
  await dragSlider(page, card.getByTestId("band-freq"), 0.566, 0.85);
  const higher = await cardBand(card);
  expect(higher.freq_hz).toBeGreaterThan(3000);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [higher] });
  await expect(card.getByTestId("band-freq-value")).toHaveText(freqLabel(higher.freq_hz));
  // The log Q slider: Q 1 sits in its middle; down to ~20 % (a wide bell).
  await dragSlider(page, card.getByTestId("band-q"), 0.5, 0.2);
  const wider = await cardBand(card);
  expect(wider.q).toBeLessThan(0.6);
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({ gain_db: 0, eq: [wider] });
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
  // A high-pass keeps the gain it does not use; a double-click on its
  // greyed gain row resets nothing, so a later shelf gets it back.
  await card.getByTestId("band-kind").selectOption("high_pass");
  await expect(card.getByTestId("band-gain-value")).toHaveText("—");
  await card.getByTestId("band-gain-value").dblclick();
  await card.getByTestId("band-kind").selectOption("low_shelf");
  await expect(card.getByTestId("band-freq-value")).toHaveText("120 Hz");
  await expect(card.getByTestId("band-gain-value")).toHaveText(gainLabel(moved.gain_db));
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({
    gain_db: 0,
    eq: [{ kind: "low_shelf", freq_hz: 120, gain_db: moved.gain_db, q: Math.SQRT1_2, enabled: true }],
  });
});

test("the wheel over a handle changes its Q (#278)", async ({ page, request }) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  const centre = await dotCentre(page, page.getByTestId("playlist-audio-handle"));
  const scrollBefore = await page.evaluate(() => window.scrollY);
  await page.mouse.move(centre.x, centre.y);
  await page.mouse.wheel(0, -100);
  await page.mouse.wheel(0, -100);
  const card = page.getByTestId("playlist-audio-band");
  await expect(card.getByTestId("band-q-value")).toHaveText("Q 1,21");
  await expect
    .poll(async () => (await lastPut(request))?.eq[0]?.q, { timeout: 1000 })
    .toBe(1.21);
  // A touchpad's small events add up instead of sticking: three of 4 px
  // reach the next 0,01 once (sp_core::audio_fx_view::WheelTurn).
  for (let i = 0; i < 3; i++) await page.mouse.wheel(0, -4);
  await expect(card.getByTestId("band-q-value")).toHaveText("Q 1,22");
  await expect
    .poll(async () => (await lastPut(request))?.eq[0]?.q, { timeout: 1000 })
    .toBe(1.22);
  // The page itself did not scroll (the handle's listener takes the wheel).
  expect(await page.evaluate(() => window.scrollY)).toBe(scrollBefore);
});

test("one request in flight: a slow server gets the PUTs one after another, the latest last (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await addBand(page, request, "peak");
  await request.post("/__mock/playlist-audio-put-delay", { data: { ms: 300 } });
  const sentBefore = (await puts(request)).length;
  const handle = page.getByTestId("playlist-audio-handle");
  const from = await dotCentre(page, handle);
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(from.x + 100, from.y - 25, { steps: 10 });
  // A drag PUT has arrived (and waits 300 ms for its answer) with the
  // button still down; the drag goes on, then ends.
  await expect
    .poll(async () => (await puts(request)).length - sentBefore, { timeout: 5000 })
    .toBeGreaterThanOrEqual(1);
  await page.mouse.move(from.x + 200, from.y - 50, { steps: 10 });
  await page.mouse.up();
  const final = await cardBand(page.getByTestId("playlist-audio-band"));
  await expect.poll(() => lastPut(request), { timeout: 5000 }).toEqual({ gain_db: 0, eq: [final] });
  await expect
    .poll(async () => (await puts(request)).every((p) => p.done !== null), { timeout: 5000 })
    .toBe(true);
  const sent = (await puts(request)).slice(sentBefore);
  // The moves after the first drag PUT reached the server in a later one.
  expect(sent.length).toBeGreaterThan(1);
  for (let i = 1; i < sent.length; i++) {
    expect(sent[i].at, `PUT ${i} started before PUT ${i - 1} was answered`).toBeGreaterThanOrEqual(
      sent[i - 1].done!,
    );
  }
});

test("a value past a limit is refused in Slovak and nothing is sent (#242, #278)", async ({
  page,
  request,
}) => {
  // A stored row the panel itself could never make (5 Hz).
  await request.post("/__mock/playlist-audio-set", {
    data: { id: PID, fx: { gain_db: 0, eq: [{ kind: "peak", freq_hz: 5, gain_db: 3, q: 1, enabled: true }] } },
  });
  await openPanel(page);
  const card = page.getByTestId("playlist-audio-band");
  await card.getByTestId("band-gain-value").dblclick();
  await expect(page.getByTestId("playlist-audio-status")).toHaveText(
    "Pásmo 1: frekvencia musí byť od 20 do 20 000 Hz",
  );
  expect(await puts(request)).toEqual([]);
  // Back inside the limits, the next change goes out.
  await card.getByTestId("band-freq-value").dblclick();
  await expect.poll(() => lastPut(request), { timeout: 1000 }).toEqual({
    gain_db: 0,
    eq: [{ kind: "peak", freq_hz: 1000, gain_db: 0, q: 1, enabled: true }],
  });
  await expect(page.getByTestId("playlist-audio-status")).toHaveText("Uložené");
});

test("a save the server refuses says so, and the next change goes out (#278)", async ({
  page,
  request,
}) => {
  await openPanel(page);
  await request.post("/__mock/playlist-audio-put-status", { data: { status: 500 } });
  try {
    await page.getByTestId("playlist-audio-add-peak").click();
    await expect(page.getByTestId("playlist-audio-status")).toHaveText(
      `Uloženie zlyhalo: PUT /api/v1/playlists/${PID}/audio → 500`,
    );
    await request.post("/__mock/playlist-audio-put-status", { data: { status: 0 } });
    await page.getByTestId("playlist-audio-add-low_shelf").click();
    await expect(page.getByTestId("playlist-audio-status")).toHaveText("Uložené");
    await expect.poll(async () => (await lastPut(request))?.eq.length, { timeout: 1000 }).toBe(2);
  } finally {
    // The 500 is the point of the test, not a bug (sp-ui-frontend.md).
    consoleMessages = consoleMessages.filter((m) => !/Failed to load resource.*500/.test(m));
  }
});

test("an unanswered save is given up after 10 s, and its late answer still counts (#278)", async ({
  page,
  request,
}) => {
  test.setTimeout(90_000);
  const status = page.getByTestId("playlist-audio-status");
  const storedBands = async () =>
    ((await (await request.get("/__mock/playlist-audio")).json()).stored[String(PID)]?.eq ?? []).length;
  await openPanel(page);

  // 1. The answer comes after 13 s: the panel gives up at 10 s and sends
  // nothing more; the late answer, with the settings unchanged since, is
  // the save after all.
  await request.post("/__mock/playlist-audio-put-delay", { data: { ms: 13_000 } });
  await page.getByTestId("playlist-audio-add-peak").click();
  await expect(status).toHaveText("Ukladám…");
  await expect(status).toHaveText("Uloženie zlyhalo: server neodpovedá", { timeout: 12_000 });
  expect(await puts(request), "no second request").toHaveLength(1);
  await expect(status).toHaveText("Uložené", { timeout: 8_000 });
  expect(await puts(request)).toHaveLength(1);
  expect(await storedBands()).toBe(1);

  // 2. A change made while a given-up request still waits goes out at once
  // (the gate is free); the stale request then lands OVER it, so the
  // panel sends its settings once more.
  await request.post("/__mock/playlist-audio-put-delay", { data: { ms: 16_000 } });
  await page.getByTestId("playlist-audio-add-low_shelf").click();
  await expect(status).toHaveText("Uloženie zlyhalo: server neodpovedá", { timeout: 12_000 });
  await request.post("/__mock/playlist-audio-put-delay", { data: { ms: 0 } });
  await page.getByTestId("playlist-audio-add-high_shelf").click();
  await expect(status).toHaveText("Uložené", { timeout: 5_000 });
  await expect.poll(async () => (await puts(request)).length, { timeout: 12_000 }).toBe(4);
  const sent = await puts(request);
  expect(sent.map((p) => p.body.eq.length)).toEqual([1, 2, 3, 3]);
  expect(sent[2].at, "the new change went out while the given-up one waited").toBeLessThan(
    sent[1].done!,
  );
  expect(sent[3].at, "sent again after the stale one landed").toBeGreaterThanOrEqual(sent[1].done!);
  await expect.poll(storedBands, { timeout: 5_000 }).toBe(3);
  await expect(status).toHaveText("Uložené");
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
