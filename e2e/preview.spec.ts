import { test, expect, type Page } from "@playwright/test";
// #184 round G: the shim's pure transport-lag decisions, imported node-side
// (like `audio-helpers.mjs` in frontend.spec.ts) so the table runs without a
// browser. The module's class only touches browser globals inside methods.
import {
  shouldReconnect,
  previewLagS,
  RTT_RECONNECT_MS,
  NO_PONG_RECONNECT_MS,
  RECONNECT_BACKOFF_BASE_MS,
  RECONNECT_BACKOFF_MAX_MS,
  reconnectGapMs,
  socketLost,
  INIT_TIMEOUT_MS,
} from "../sp-ui/preview_player.js";

// #178: the dashboard playlist card's live A/V preview is a real MSE `<video>`
// fed fragmented MP4 over the `preview.ws` WebSocket (the #15 JPEG `<img>` is
// gone from the card). Round 3: the preview is CLICK-to-start — a Playing card
// shows a `preview-start` control and only mounts the `<video>` (opening the WS)
// after the operator clicks it, so nothing runs an encoder child unwatched. The
// mock (`mock-api.mjs`) streams a canned H.264/AAC fMP4 fixture over that WS.
// H.264/AAC are ABSENT from Playwright's bundled Chromium, so this spec runs
// ONLY in the `chrome` project (channel: 'chrome') — see playwright.config.ts.
// The mock marks playlist 1 (Worship) Playing and preselects it in the single
// work area; playlist 2 (Background) stays Idle so its card shows the
// placeholder, never a start control or a `<video>`.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

// Open the playing Worship card and click its start control, returning the
// card + the now-mounted <video> locator.
async function startPreview(page: Page) {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  // Round 3: no <video> until the operator clicks start (on-demand only).
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
  await card.getByTestId("preview-start").click();
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });
  return { card, video };
}

test("clicking start streams live A/V into an MSE <video> that decodes and advances", async ({
  page,
  request,
}) => {
  const { video } = await startPreview(page);

  // #194: run the decode assertion WITH the 500 ms now-playing position tick
  // flowing — a live position tick must never disturb the preview stream.
  await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: 1,
          video_id: 1,
          song: "Never Gonna Give You Up",
          duration_ms: 213000,
          position_ms: 0,
          state: "Playing",
        },
      ],
    },
  });
  try {
    // readyState >= 3 (HAVE_FUTURE_DATA) proves the browser actually DECODED the
    // streamed H.264+AAC — a broken stream or a codec-less browser never gets here.
    await expect
      .poll(
        async () => video.evaluate((el: HTMLVideoElement) => el.readyState),
        { timeout: 15000 },
      )
      .toBeGreaterThanOrEqual(3);

    // Real decoded geometry (the fixture is 640x360).
    const width = await video.evaluate((el: HTMLVideoElement) => el.videoWidth);
    expect(width).toBeGreaterThan(0);

    // currentTime advances — the media is actually playing, not just buffered.
    const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
    await expect
      .poll(
        async () => video.evaluate((el: HTMLVideoElement) => el.currentTime),
        { timeout: 10000 },
      )
      .toBeGreaterThan(t0 + 0.05);
  } finally {
    await request.post("/__mock/tick", { data: { enabled: false } });
  }
});

test("the Dabing Player preview survives position ticks — WS opened once, never closed (#194)", async ({
  page,
  request,
}) => {
  // The #194 box regression: the Dabing page re-set an unchanged playlist id
  // every 2 s, re-creating the whole Player → the preview <video> unmounted and
  // its WebSocket was torn down right after opening ("WebSocket is closed before
  // the connection is established"). Here the preview WS actually opens (branded
  // Chrome has the codecs), so we can prove it opens ONCE and never closes while
  // the 500 ms position tick runs.
  const previewSockets: { closed: boolean }[] = [];
  page.on("websocket", (ws) => {
    if (ws.url().includes("/preview.ws")) {
      const rec = { closed: false };
      ws.on("close", () => {
        rec.closed = true;
      });
      previewSockets.push(rec);
    }
  });

  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 344,
      title: "Morning Prayer",
      dub_status: "ready",
      stem_status: null,
      dub_mix_ratio: 1.0,
    },
  });
  await page.goto("/dabing");
  await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
  await request.post("/__mock/tick", {
    data: {
      enabled: true,
      items: [
        {
          playlist_id: 500,
          video_id: 344,
          song: "Morning Prayer",
          duration_ms: 200000,
          position_ms: 0,
          state: "Playing",
        },
      ],
    },
  });

  try {
    await page.getByTestId("preview-start").click();
    const video = page.getByTestId("preview-video");
    await expect(video).toBeVisible({ timeout: 10000 });
    await expect
      .poll(
        async () => video.evaluate((el: HTMLVideoElement) => el.readyState),
        { timeout: 15000 },
      )
      .toBeGreaterThanOrEqual(3);

    const playerHandle = await page.getByTestId("player").elementHandle();
    const videoHandle = await video.elementHandle();

    // Let >= 8 s of 500 ms position ticks flow.
    await page.waitForTimeout(8000);

    // The regression proof: the SAME Player + <video> elements are still
    // connected (the Player was NOT re-created by the ticks) and the preview WS
    // opened exactly once and NEVER closed across the 8 s of ticks. (We do not
    // re-assert readyState here — the mock streams a FINITE canned fMP4 fragment
    // list, so after ~2 s the short clip has played out and readyState drops;
    // the initial readyState>=3 above already proved it decoded, and a torn-down
    // preview would have shown up as a closed/duplicate WS, which it did not.)
    expect(await playerHandle!.evaluate((el) => el.isConnected)).toBe(true);
    expect(await videoHandle!.evaluate((el) => el.isConnected)).toBe(true);
    expect(previewSockets.length).toBe(1);
    expect(previewSockets[0].closed).toBe(false);
  } finally {
    await request.post("/__mock/tick", { data: { enabled: false } });
    await request.post("/__mock/dabing-reset");
  }
});

test("the click-started <video> ends up audible (unmuted directly or via the unmute button)", async ({
  page,
}) => {
  const { card, video } = await startPreview(page);
  // The start click is the user gesture, so the player tries to start UNMUTED.
  // Wait until it has decoded (the initial play() has resolved either way).
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);
  // If the browser rejected the unmuted autoplay and the shim fell back to
  // muted, one click of the unmute button re-enables audio.
  if (await video.evaluate((el: HTMLVideoElement) => el.muted)) {
    await card.getByTestId("preview-unmute").click();
  }
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.muted), {
      timeout: 5000,
    })
    .toBe(false);
});

test("the stop control tears the <video> down", async ({ page }) => {
  const { card, video } = await startPreview(page);
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);
  await card.getByTestId("preview-stop").click();
  // The <video> is unmounted (WS closed → the encoder child dies) and the start
  // control comes back in the placeholder.
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
  await expect(card.getByTestId("preview-start")).toBeVisible();
});

test("with the lag knob the started preview shows the 'náhľad mešká N s' readout (#184)", async ({
  page,
}) => {
  // #184 round F: a page-level `?lag_ms=30000` flag is forwarded to the preview
  // WS, where the mock inflates the produced_ms beacon by 30 s. The shim then
  // reports a big picture lag and the Player shows the readout (≥ 3 s).
  await page.goto("/?lag_ms=30000");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  await card.getByTestId("preview-start").click();
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 10000 });

  const lag = card.getByTestId("preview-lag");
  await expect(lag).toBeVisible({ timeout: 15000 });
  await expect(lag).toContainText("mešká");
  const n = Number((await lag.textContent())?.match(/\d+/)?.[0] ?? "0");
  expect(n, "the readout must show a lag of at least 3 s").toBeGreaterThanOrEqual(3);
});

test("without the lag knob the live preview shows no lag readout (#184)", async ({
  page,
}) => {
  // No page flag → the mock beacon's produced_ms tracks the streamed media, so
  // the picture is live (< 3 s behind) and the readout stays absent.
  const { card, video } = await startPreview(page);
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);
  // Let a couple of 1 Hz beacon cycles flow, then confirm the readout is absent.
  await page.waitForTimeout(2500);
  await expect(card.getByTestId("preview-lag")).toHaveCount(0);
});

// ── #184 round G: transport lag via ping/pong + backlog drop ─────────────────

test("shouldReconnect: the round-G reconnect decision table (#184)", () => {
  // The constants the design fixed — a change here is a design change.
  expect(RTT_RECONNECT_MS).toBe(3000);
  expect(NO_PONG_RECONNECT_MS).toBe(5000);
  // #184 round G2: the fixed 10 s gap became an exponential backoff whose
  // FIRST step is 12 s (see the backoff table test below).
  expect(RECONNECT_BACKOFF_BASE_MS).toBe(12000);
  expect(RECONNECT_BACKOFF_MAX_MS).toBe(60000);
  const never = null; // no reconnect has happened yet on this player
  const rows: {
    name: string;
    rtts: number[];
    msAwaitingPong: number;
    msSinceLastReconnect: number | null;
    want: boolean;
  }[] = [
    { name: "healthy link", rtts: [40, 60, 55], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "no data yet", rtts: [], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "2x rtt > 3 s", rtts: [100, 3500, 3600], msAwaitingPong: 0, msSinceLastReconnect: never, want: true },
    { name: "only the last rtt > 3 s", rtts: [100, 3500], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "a slow rtt that recovered", rtts: [3500, 100], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "an older pair, not the last two", rtts: [3500, 3600, 100], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "exactly 3 s is not over", rtts: [3000, 3000], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "a single sample", rtts: [9000], msAwaitingPong: 0, msSinceLastReconnect: never, want: false },
    { name: "no pong for > 5 s", rtts: [], msAwaitingPong: 5001, msSinceLastReconnect: never, want: true },
    { name: "no pong for exactly 5 s", rtts: [], msAwaitingPong: 5000, msSinceLastReconnect: never, want: false },
    { name: "no pong after good rtts", rtts: [40, 50], msAwaitingPong: 7000, msSinceLastReconnect: never, want: true },
    { name: "2x slow but within 12 s of the last reconnect", rtts: [4000, 4000], msAwaitingPong: 0, msSinceLastReconnect: 11999, want: false },
    { name: "no pong but within 12 s of the last reconnect", rtts: [], msAwaitingPong: 6000, msSinceLastReconnect: 5000, want: false },
    { name: "2x slow, exactly 12 s after the last reconnect", rtts: [4000, 4000], msAwaitingPong: 0, msSinceLastReconnect: 12000, want: true },
    { name: "no pong, long after the last reconnect", rtts: [], msAwaitingPong: 6000, msSinceLastReconnect: 60000, want: true },
  ];
  for (const r of rows) {
    expect(
      shouldReconnect({
        rtts: r.rtts,
        msAwaitingPong: r.msAwaitingPong,
        msSinceLastReconnect: r.msSinceLastReconnect,
      }),
      r.name,
    ).toBe(r.want);
  }
});

test("previewLagS: ONE lag number = the worst of beacon lag, last rtt and the unanswered ping (#184)", () => {
  // Nothing measured yet → no report (the badge keeps its last value / 0).
  expect(previewLagS({ beaconLagS: null, rtts: [], msAwaitingPong: 0 })).toBeNull();
  // Beacon only (round F): its value, in seconds.
  expect(previewLagS({ beaconLagS: 30.2, rtts: [], msAwaitingPong: 0 })).toBeCloseTo(30.2, 6);
  // The LAST round trip (ms → s) dominates a beacon blinded by the backlog.
  expect(previewLagS({ beaconLagS: 0.1, rtts: [100, 6000], msAwaitingPong: 0 })).toBeCloseTo(6, 6);
  // Only the last rtt counts, not an older spike.
  expect(previewLagS({ beaconLagS: 0, rtts: [6000, 200], msAwaitingPong: 0 })).toBeCloseTo(0.2, 6);
  // A ping still waiting for its pong is lag we already know about.
  expect(previewLagS({ beaconLagS: 0, rtts: [50], msAwaitingPong: 4500 })).toBeCloseTo(4.5, 6);
  // A negative beacon lag (buffered ahead of the counter) never hides the rtt.
  expect(previewLagS({ beaconLagS: -2, rtts: [50], msAwaitingPong: 0 })).toBeCloseTo(0.05, 6);
});

// Record every preview.ws the page opens, in order, with the application-level
// pings it sent and the pongs it received.
function watchPreviewSockets(page: Page) {
  const sockets: {
    openedAt: number;
    closed: boolean;
    pings: number;
    pongs: number;
  }[] = [];
  page.on("websocket", (ws) => {
    if (!ws.url().includes("/preview.ws")) return;
    const rec = { openedAt: Date.now(), closed: false, pings: 0, pongs: 0 };
    ws.on("close", () => {
      rec.closed = true;
    });
    ws.on("framesent", (f) => {
      if (typeof f.payload === "string" && /^\{"ping":[0-9.]+\}$/.test(f.payload)) {
        rec.pings++;
      }
    });
    ws.on("framereceived", (f) => {
      if (typeof f.payload === "string" && /^\{"pong":[0-9.]+\}$/.test(f.payload)) {
        rec.pongs++;
      }
    });
    sockets.push(rec);
  });
  return sockets;
}

test("a transport backlog shows the lag badge, then the player drops it and reconnects (#184 round G)", async ({
  page,
}) => {
  test.setTimeout(60000);
  // `?pong_delay_ms=6000` (forwarded by the shim onto preview.ws) makes the
  // mock deliver every frame after the init — media, beacon AND the pong — 6 s
  // late, i.e. a tunnel backlog the round-F beacon cannot see.
  const sockets = watchPreviewSockets(page);
  await page.goto("/?pong_delay_ms=6000");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  await card.getByTestId("preview-start").click();
  const clickedAt = Date.now();
  await expect(card.getByTestId("preview-video")).toBeVisible({ timeout: 10000 });

  // The unanswered ping makes the badge show the real lag BEFORE the drop.
  const lag = card.getByTestId("preview-lag");
  await expect(lag).toBeVisible({ timeout: 10000 });
  const shownWith = sockets.length;
  const n = Number((await lag.textContent())?.match(/\d+/)?.[0] ?? "0");
  expect(n, "the badge shows a lag of at least 3 s").toBeGreaterThanOrEqual(3);
  expect(shownWith, "the badge showed while the FIRST socket was still in use").toBe(1);

  // Then the backlog is dropped: a NEW preview socket within ~15 s of the start,
  // and the first one is closed.
  await expect
    .poll(() => sockets.length, { timeout: 15000 })
    .toBeGreaterThanOrEqual(2);
  expect(sockets[1].openedAt - clickedAt, "reconnected within ~15 s").toBeLessThan(15000);
  await expect.poll(() => sockets[0].closed, { timeout: 5000 }).toBe(true);
});

test("a healthy link keeps ONE preview socket and no lag badge for 20 s (#184 round G)", async ({
  page,
}) => {
  test.setTimeout(60000);
  const sockets = watchPreviewSockets(page);
  const { card, video } = await startPreview(page);
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 15000,
    })
    .toBeGreaterThanOrEqual(3);
  // Sample across 20 s (pongs every second) — never a second socket, never a
  // badge, at ANY point of the window (not just at its end).
  const until = Date.now() + 20000;
  while (Date.now() < until) {
    expect(sockets.length, "exactly one preview socket").toBe(1);
    expect(sockets[0].closed, "the socket stays open").toBe(false);
    await expect(card.getByTestId("preview-lag")).toHaveCount(0);
    await page.waitForTimeout(500);
  }
  expect(sockets.length).toBe(1);
  // The window was genuinely MEASURED: the shim pinged ~1 Hz and every ping
  // came back (a player that never pinged would pass the checks above).
  expect(sockets[0].pings, "~1 Hz pings over the 20 s window").toBeGreaterThanOrEqual(15);
  expect(sockets[0].pongs, "the pongs came back").toBeGreaterThanOrEqual(15);
});

test("shouldReconnect: a lost socket is replaced, but never within the reconnect gap (#184 round G review)", () => {
  // `socketLost`: the socket closed on its own (server restart, relay close) or
  // never delivered its init within the connect deadline — nothing to ping on.
  expect(
    shouldReconnect({ rtts: [], msAwaitingPong: 0, msSinceLastReconnect: null, socketLost: true }),
    "lost, never reconnected",
  ).toBe(true);
  expect(
    shouldReconnect({ rtts: [40], msAwaitingPong: 0, msSinceLastReconnect: 60000, socketLost: true }),
    "lost, long after the last reconnect",
  ).toBe(true);
  expect(
    shouldReconnect({ rtts: [], msAwaitingPong: 0, msSinceLastReconnect: 3000, socketLost: true }),
    "lost, but within 12 s of the last reconnect",
  ).toBe(false);
  expect(
    shouldReconnect({ rtts: [40], msAwaitingPong: 0, msSinceLastReconnect: null, socketLost: false }),
    "a live, healthy socket",
  ).toBe(false);
});

test("reconnectGapMs: reconnect attempts back off 12 s → 24 s → 48 s, capped at 60 s (#184 round G2)", () => {
  // `reconnectsWithoutMedia` = reconnects made since a socket last delivered a
  // media fragment. 0 (a healthy socket just reset it) and 1 (the first retry)
  // both wait the base 12 s; every further fruitless retry doubles, max 60 s —
  // round G retried every 12 s forever against an encoder with no init.
  expect(reconnectGapMs(0)).toBe(12000);
  expect(reconnectGapMs(1)).toBe(12000);
  expect(reconnectGapMs(2)).toBe(24000);
  expect(reconnectGapMs(3)).toBe(48000);
  expect(reconnectGapMs(4)).toBe(60000);
  expect(reconnectGapMs(5)).toBe(60000);
  expect(reconnectGapMs(1000)).toBe(60000);
  // Missing / garbage input is the base step, never NaN (never a tight loop).
  expect(reconnectGapMs(undefined)).toBe(12000);
  expect(reconnectGapMs(-3)).toBe(12000);
});

test("shouldReconnect: a lost socket waits the backoff for its retry count (#184 round G2)", () => {
  const lost = (reconnectsWithoutMedia: number, msSinceLastReconnect: number) =>
    shouldReconnect({
      rtts: [],
      msAwaitingPong: 0,
      msSinceLastReconnect,
      socketLost: true,
      reconnectsWithoutMedia,
    });
  // 1st retry: 12 s.
  expect(lost(1, 11999), "1st retry, 12 s not yet up").toBe(false);
  expect(lost(1, 12000), "1st retry at 12 s").toBe(true);
  // 2nd fruitless retry: 24 s.
  expect(lost(2, 23999), "2nd retry, 24 s not yet up").toBe(false);
  expect(lost(2, 24000), "2nd retry at 24 s").toBe(true);
  // 3rd: 48 s.
  expect(lost(3, 47999), "3rd retry, 48 s not yet up").toBe(false);
  expect(lost(3, 48000), "3rd retry at 48 s").toBe(true);
  // 4th and later: capped at 60 s.
  expect(lost(4, 59999), "4th retry, 60 s not yet up").toBe(false);
  expect(lost(4, 60000), "4th retry at the 60 s cap").toBe(true);
  expect(lost(9, 60000), "9th retry still capped at 60 s").toBe(true);
  // A socket that delivered media reset the count: back to the 12 s base.
  expect(lost(0, 12000), "after media arrived, the base 12 s again").toBe(true);
  // The backoff also gates the transport-lag rules, not only a lost socket.
  expect(
    shouldReconnect({
      rtts: [4000, 4000],
      msAwaitingPong: 0,
      msSinceLastReconnect: 30000,
      socketLost: false,
      reconnectsWithoutMedia: 3,
    }),
    "2x slow rtt, but the 3rd retry's 48 s are not up",
  ).toBe(false);
  // The very first reconnect of a player is never delayed.
  expect(
    shouldReconnect({ rtts: [], msAwaitingPong: 0, msSinceLastReconnect: null, socketLost: true, reconnectsWithoutMedia: 0 }),
    "first reconnect ever",
  ).toBe(true);
});

test("socketLost: waiting for the first init is never 'lost' inside its first 12 s (#184 round G2)", () => {
  const s = (o: Partial<{ hasSocket: boolean; wsClosed: boolean; gotInit: boolean; msSinceConnect: number }>) =>
    socketLost({ hasSocket: true, wsClosed: false, gotInit: false, msSinceConnect: 0, ...o });
  // An encoder cold start / the libx264 fallback takes seconds: no init yet is
  // NOT a lost socket (and never transport lag) until the 12 s deadline passes.
  expect(s({ msSinceConnect: 0 }), "just opened").toBe(false);
  expect(s({ msSinceConnect: 11999 }), "no init after 11.999 s").toBe(false);
  expect(s({ msSinceConnect: 12000 }), "no init at exactly 12 s").toBe(false);
  expect(s({ msSinceConnect: 12001 }), "no init past 12 s").toBe(true);
  // Once the init arrived the deadline no longer applies.
  expect(s({ gotInit: true, msSinceConnect: 600000 }), "init arrived long ago").toBe(false);
  // A socket that closed on its own, or none at all, is lost at once.
  expect(s({ wsClosed: true }), "closed by the server").toBe(true);
  expect(s({ hasSocket: false }), "no socket object").toBe(true);
  // And a pre-init socket inside its first 12 s never triggers a reconnect.
  expect(
    shouldReconnect({
      rtts: [],
      msAwaitingPong: 0,
      msSinceLastReconnect: null,
      socketLost: s({ msSinceConnect: 11000 }),
      reconnectsWithoutMedia: 0,
    }),
    "pre-init, 11 s: keep waiting",
  ).toBe(false);
});

// Wait until the <video> is playing again: readyState >= 3 and currentTime
// moving FORWARD between two consecutive samples (the playhead may first jump
// back to the start of the reconnected stream, so compare sample-to-sample,
// never against a value read before the jump). A frozen picture never passes.
async function expectPlayingAgain(page: Page) {
  const video = page.locator(".playlist-card").getByTestId("preview-video");
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
      timeout: 10000,
    })
    .toBeGreaterThanOrEqual(3);
  let last: number | null = null;
  await expect
    .poll(
      async () => {
        const t = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
        const advanced = last !== null && t > last + 0.02;
        last = t;
        return advanced;
      },
      { timeout: 5000, intervals: [200] },
    )
    .toBe(true);
}

test("a socket the server closes is replaced and the preview plays again (#184 round G review)", async ({
  page,
  request,
}) => {
  test.setTimeout(60000);
  // The FIRST preview socket is closed by the server after 2 fragments (a
  // server restart / relay close); the reconnected one is healthy.
  await request.post("/__mock/preview-fault", { data: { close_after_frags: 2 } });
  try {
    const sockets = watchPreviewSockets(page);
    await startPreview(page);
    await expect.poll(() => sockets.length, { timeout: 10000 }).toBeGreaterThanOrEqual(2);
    expect(sockets[0].closed, "the first socket was closed by the server").toBe(true);
    await expectPlayingAgain(page);
    expect(sockets.length, "one reconnect, no churn").toBe(2);
  } finally {
    await request.post("/__mock/preview-fault", { data: {} });
  }
});

test("a backlog on the first socket reconnects via the round-trip rule, then plays live with no badge (#184 round G review)", async ({
  page,
  request,
}) => {
  test.setTimeout(60000);
  // 4 s of backlog on the FIRST socket only: no ping ever waits > 5 s, so the
  // reconnect can only come from the "2 round trips > 3 s" rule.
  await request.post("/__mock/preview-fault", { data: { delay_ms: 4000 } });
  try {
    const sockets = watchPreviewSockets(page);
    const { card } = await startPreview(page);
    const lag = card.getByTestId("preview-lag");
    await expect(lag).toBeVisible({ timeout: 10000 });
    expect(sockets.length, "the badge showed on the first socket").toBe(1);
    await expect.poll(() => sockets.length, { timeout: 15000 }).toBeGreaterThanOrEqual(2);
    expect(sockets[0].pongs, "the round trips that decided it came back").toBeGreaterThanOrEqual(2);
    // The reconnected socket is healthy: the picture plays again and the
    // badge clears.
    await expectPlayingAgain(page);
    await expect(lag).toHaveCount(0, { timeout: 5000 });
    expect(sockets.length, "one reconnect, no churn").toBe(2);
  } finally {
    await request.post("/__mock/preview-fault", { data: {} });
  }
});

// Strict "it is PLAYING": two consecutive samples, each with the element not
// seeking and currentTime stepping forward by a normal-playback amount (a jump
// to the live edge or a snap is a seek, not playback, and never passes).
async function expectSteadyPlayback(page: Page) {
  const video = page.locator(".playlist-card").getByTestId("preview-video");
  let last: number | null = null;
  let steady = 0;
  await expect
    .poll(
      async () => {
        const s = await video.evaluate((el: HTMLVideoElement) => ({
          t: el.currentTime,
          seeking: el.seeking,
        }));
        const step = last === null ? 0 : s.t - last;
        steady = !s.seeking && step > 0.02 && step <= 0.5 ? steady + 1 : 0;
        last = s.t;
        return steady >= 2;
      },
      { timeout: 8000, intervals: [100] },
    )
    .toBe(true);
}

test("after a reconnect onto a restarted timeline the playhead jumps to the new stream's first sample (#184 round G review 2)", async ({
  page,
  request,
}) => {
  test.setTimeout(60000);
  const sockets = watchPreviewSockets(page);
  const { video } = await startPreview(page);
  // Let the 4 s fixture play past 3 s. The mock restarts its fixture at 0 on a
  // new socket (like a respawned encoder child's timeline), so the reconnected
  // stream then lies BEHIND the playhead.
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.currentTime), {
      timeout: 15000,
      intervals: [100],
    })
    .toBeGreaterThanOrEqual(3);
  await video.evaluate((el: HTMLVideoElement) => {
    const w = window as unknown as { __seeks: number[] };
    w.__seeks = [];
    el.addEventListener("seeked", () => w.__seeks.push(el.currentTime));
  });
  // The server drops the socket now; the shim replaces it at once (its first
  // reconnect is not rate-limited).
  await request.post("/__mock/preview-close");
  await expect
    .poll(() => sockets.length, { timeout: 5000, intervals: [100] })
    .toBeGreaterThanOrEqual(2);
  // Without the snap the playhead would sit past the end of everything the new
  // socket buffers and never move; with it a seek lands on the first sample.
  await expect
    .poll(
      () =>
        page.evaluate(() =>
          (window as unknown as { __seeks: number[] }).__seeks.some((t) => t < 0.6),
        ),
      { timeout: 10000, intervals: [100] },
    )
    .toBe(true);
  await expectSteadyPlayback(page);
  expect(sockets.length, "one reconnect, no churn").toBe(2);
});

test("a socket that never delivers its init is replaced after the init timeout (#184 round G review 2)", async ({
  page,
  request,
}) => {
  test.setTimeout(60000);
  // Above the real server's own ~10 s wait for the encoder's init.
  expect(INIT_TIMEOUT_MS).toBe(12000);
  // The FIRST socket opens but stays silent (an encoder that never starts).
  await request.post("/__mock/preview-fault", { data: { hold_init: true } });
  try {
    const sockets = watchPreviewSockets(page);
    await startPreview(page);
    await expect
      .poll(() => sockets.length, { timeout: 20000, intervals: [100] })
      .toBeGreaterThanOrEqual(2);
    const waited = sockets[1].openedAt - sockets[0].openedAt;
    expect(waited, "not replaced before the init timeout").toBeGreaterThanOrEqual(
      INIT_TIMEOUT_MS - 500,
    );
    expect(waited, "replaced soon after the init timeout").toBeLessThanOrEqual(
      INIT_TIMEOUT_MS + 2500,
    );
    expect(sockets[0].pings, "no pings before an init").toBe(0);
    await expectSteadyPlayback(page);
    expect(sockets.length, "one reconnect, no churn").toBe(2);
  } finally {
    await request.post("/__mock/preview-fault", { data: {} });
  }
});

test("a playing card mounts no <video> until start is clicked", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.getByTestId("workspace-title")).toHaveText("Worship", {
    timeout: 10000,
  });
  const card = page.locator(".playlist-card");
  // Playing, but on-demand: the start control is shown and NO <video>/WS exists.
  await expect(card.getByTestId("preview-start")).toBeVisible();
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
});

test("idle card shows the preview placeholder and no start control or <video>", async ({
  page,
}) => {
  await page.goto("/");
  // #165: bring the idle Background playlist into the single work area by
  // selecting its row (Worship is playing and preselected by default).
  await expect(page.getByTestId("playlist-workspace")).toBeVisible({
    timeout: 10000,
  });
  await page
    .getByTestId("playlist-picker-item")
    .filter({ hasText: "Background" })
    .click();
  await expect(page.getByTestId("workspace-title")).toHaveText("Background");
  const card = page.locator(".playlist-card");
  await expect(card.getByTestId("preview-placeholder")).toBeVisible({
    timeout: 10000,
  });
  // An idle card is not watchable — no start control, no <video>, no WS.
  await expect(card.getByTestId("preview-start")).toHaveCount(0);
  await expect(card.getByTestId("preview-video")).toHaveCount(0);
});
