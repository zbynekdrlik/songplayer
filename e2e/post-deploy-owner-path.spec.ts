/**
 * #184 round G2 — THE OWNER'S PATH on the real box (acceptance for every
 * preview / mixer change, owner ROZHODNUTÉ 2026-09-23 on #184 comment
 * 5802408328: "akceptácia každého ďalšieho kola = post-deploy Playwright test na
 * SKUTOČNOM boxe cestou ownera").
 *
 * Until round G2 every preview/mixer round was verified with box logs and a MOCK
 * Playwright suite, and the owner then opened Chrome and found it "rovnako
 * biedne ako predtým": a fader change was not heard for 20–70 s and the preview
 * then froze into a reconnect loop. This spec walks exactly his clicks:
 *
 *   dashboard (Prehľad) → the Dabing playlist row → ▶ the ready dub video →
 *   "▶ Živý náhľad" → the REAL mouse on the three vertical faders.
 *
 * and asserts, with every timing printed:
 *   (a) the preview is audible (RMS > −50 dBFS) within 10 s;
 *   (b) vokály + podklad + dabing dragged to 0 → RMS < −60 dBFS within 4 s of
 *       the last PATCH;
 *   (c) vokály dragged back to the top → RMS > −50 dBFS within 4 s;
 *   (d) 180 s with the preview open (500 ms samples): never silent (< −60 dBFS)
 *       for more than 5 s while the mix is non-zero, and at most ONE extra
 *       `preview.ws` connection;
 *   (e) the lyrics panel's active line (`.lyr-current`) stays inside its
 *       scroller (`.lyrics-view-scroll`) at every sample;
 *   (f) zero console errors / warnings (the last assertion, repo rule — in
 *       afterEach, after the cleanup, so it also runs when the body failed).
 *
 * The 180 s window is the owner-ruled acceptance MEASUREMENT (3 minutes of
 * preview), not a sleep: every sample is asserted as it is taken and the test
 * fails on the FIRST violation. It never touches OBS scenes (the Dabing output
 * is off-program — the live wall is never switched). Runs ONLY under the `edge`
 * project of post-deploy.config.ts (Edge carries H.264/AAC; bundled Chromium
 * cannot decode the preview). Viewport 1600×1000: the default viewport hides the
 * vertical faders.
 */

import { test, expect, APIRequestContext, Locator, Page, Request } from "@playwright/test";
import {
  audibleStreak,
  longestSilentRunMs,
  quietStreak,
  rmsToDbfs,
} from "./audio-helpers.mjs";

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

/** Audible: RMS above −50 dBFS. Silent: below −60 dBFS (#184 G2 design). */
const AUDIBLE_DB = -50;
const SILENT_DB = -60;
/** A decision needs this many consecutive samples (#206: never one sample). */
const STREAK = 3;
/** Sample spacing while waiting for a level change. */
const LEVEL_POLL_MS = 200;
/** A fader change must be heard within this long after its PATCH. */
const FADER_AUDIBLE_WITHIN_MS = 4000;
/** The owner-path soak: 3 minutes of preview, sampled every 500 ms. */
const SOAK_MS = 180_000;
const SOAK_SAMPLE_MS = 500;
const MAX_SILENT_RUN_MS = 5000;
/** The video the owner walked on 23.9.2026 (preferred when it is a ready dub). */
const OWNER_VIDEO_ID = 344;

test.use({ viewport: { width: 1600, height: 1000 } });

/** Install a one-time Web Audio RMS tap on the preview <video>. */
async function installAudioTap(video: Locator): Promise<void> {
  await video.evaluate((el: HTMLVideoElement) => {
    const w = window as unknown as {
      __g2Analyser?: AnalyserNode;
      __g2Buf?: Float32Array;
      AudioContext: typeof AudioContext;
      webkitAudioContext?: typeof AudioContext;
    };
    if (w.__g2Analyser) return;
    const AC = w.AudioContext || w.webkitAudioContext!;
    const ctx = new AC();
    const src = ctx.createMediaElementSource(el);
    const an = ctx.createAnalyser();
    an.fftSize = 2048;
    src.connect(an);
    an.connect(ctx.destination);
    w.__g2Analyser = an;
    w.__g2Buf = new Float32Array(an.fftSize);
    void ctx.resume();
  });
}

/** One sample: the preview's RMS (dBFS) + whether the active lyric line is
 *  inside its scroller (null when no line is active at this instant). */
async function sample(page: Page): Promise<{ db: number; lyrInside: boolean | null; lyrDetail: string }> {
  const s = await page.evaluate(() => {
    const w = window as unknown as { __g2Analyser?: AnalyserNode; __g2Buf?: Float32Array };
    let rms = 0;
    if (w.__g2Analyser && w.__g2Buf) {
      w.__g2Analyser.getFloatTimeDomainData(w.__g2Buf);
      let sum = 0;
      for (let i = 0; i < w.__g2Buf.length; i++) sum += w.__g2Buf[i] * w.__g2Buf[i];
      rms = Math.sqrt(sum / w.__g2Buf.length);
    }
    const cur = document.querySelector(".lyrics-view-scroll .lyr-current");
    if (!cur) return { rms, lyrInside: null as boolean | null, lyrDetail: "" };
    const box = (cur.closest(".lyrics-view-scroll") as HTMLElement).getBoundingClientRect();
    const r = cur.getBoundingClientRect();
    const inside = r.top >= box.top - 1 && r.bottom <= box.bottom + 1;
    return {
      rms,
      lyrInside: inside as boolean | null,
      lyrDetail: `line [${r.top.toFixed(0)}, ${r.bottom.toFixed(0)}] panel [${box.top.toFixed(0)}, ${box.bottom.toFixed(0)}]`,
    };
  });
  return { db: rmsToDbfs(s.rms), lyrInside: s.lyrInside, lyrDetail: s.lyrDetail };
}

/** Real-mouse drag on a VERTICAL range fader. `from`/`to` are fractions of the
 *  slider HEIGHT measured from its TOP (value 0 is at the BOTTOM). */
async function dragFader(page: Page, testid: string, from: number, to: number): Promise<void> {
  const fader = page.getByTestId(testid);
  await fader.scrollIntoViewIfNeeded();
  const box = await fader.boundingBox();
  if (!box) throw new Error(`no bounding box for ${testid}`);
  const x = box.x + box.width / 2;
  await page.mouse.move(x, box.y + box.height * from);
  await page.mouse.down();
  await page.mouse.move(x, box.y + box.height * to, { steps: 10 });
  await page.mouse.up();
}

/** Poll the level every LEVEL_POLL_MS until a STREAK of samples satisfies the
 *  condition, or the deadline passes. Returns the wall time of the FIRST sample
 *  of the streak (null if never reached) and the samples for the failure text. */
async function waitForLevel(
  page: Page,
  kind: "audible" | "quiet",
  deadline: number,
): Promise<{ at: number | null; trace: string }> {
  const dbs: number[] = [];
  const times: number[] = [];
  for (;;) {
    const s = await sample(page);
    dbs.push(s.db);
    times.push(Date.now());
    const end =
      kind === "audible"
        ? audibleStreak(dbs, AUDIBLE_DB, STREAK)
        : quietStreak(dbs, SILENT_DB, STREAK);
    if (end >= 0) {
      return { at: times[end - STREAK + 1], trace: dbs.map((d) => d.toFixed(1)).join(" ") };
    }
    if (Date.now() >= deadline) {
      return { at: null, trace: dbs.map((d) => d.toFixed(1)).join(" ") };
    }
    await page.waitForTimeout(LEVEL_POLL_MS);
  }
}

/** A ready dub on the box: the owner's video 344 when it is ready, else the
 *  first ready one (the same precondition as post-deploy-dabing.spec.ts). */
async function readyDub(request: APIRequestContext): Promise<{ pid: number; videoId: number }> {
  const dab = await request.get("/api/v1/dabing");
  expect(dab.status()).toBe(200);
  const body = (await dab.json()) as {
    playlist_id: number;
    videos: Array<{ video_id?: number; id?: number; dub_status: string }>;
  };
  const ready = body.videos.filter((v) => v.dub_status === "ready");
  expect(ready.length, "at least one dub must be ready on the box").toBeGreaterThan(0);
  const ids = ready.map((v) => Number(v.video_id ?? v.id));
  const videoId = ids.includes(OWNER_VIDEO_ID) ? OWNER_VIDEO_ID : ids[0];
  expect(videoId, "the ready dub must carry a numeric video id").toBeGreaterThan(0);
  return { pid: body.playlist_id, videoId };
}

type MixMemory = { vokaly: number; podklad: number; dabing: number };

let consoleMessages: string[] = [];
/** What afterEach must put back: set as soon as the test knows it. */
let cleanup: { pid: number; foundDabing: number } | null = null;

test.beforeEach(async ({ page }) => {
  consoleMessages = [];
  cleanup = null;
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

// Runs even when the test body failed or timed out: vokály + podklad full
// (100 %), dabing as found (the #184 G2 restore rule), stop the preview this
// test started and pause the (off-program) Dabing output. Then (f) — zero
// console errors / warnings — is the LAST assertion.
test.afterEach(async ({ page, request }) => {
  if (cleanup) {
    await request
      .patch("/api/v1/mix", {
        data: { kind: "dub", vokaly: 1.0, podklad: 1.0, dabing: cleanup.foundDabing },
        timeout: 10000,
      })
      .catch(() => {});
    await page.getByTestId("preview-stop").click({ timeout: 3000 }).catch(() => {});
    await request
      .post(`/api/v1/playback/${cleanup.pid}/pause`, { timeout: 10000 })
      .catch(() => {});
  }
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real, "(f) zero console errors / warnings").toEqual([]);
});

test("the owner's path: Prehľad → Dabing → play → Živý náhľad → real-mouse faders are heard within 4 s, 3 min without a freeze (#184 G2)", async ({
  page,
  request,
}) => {
  test.setTimeout(330_000);

  // Every preview socket this page opens (a reconnect = a new one).
  const previewSockets: string[] = [];
  page.on("websocket", (ws) => {
    if (ws.url().includes("preview.ws")) previewSockets.push(ws.url());
  });
  // Every mixer PATCH with the wall time it left the browser.
  const patches: Array<{ at: number; body: Record<string, unknown> }> = [];
  page.on("request", (req: Request) => {
    if (req.method() === "PATCH" && req.url().includes("/api/v1/mix")) {
      let body: Record<string, unknown> = {};
      try {
        body = (req.postDataJSON() ?? {}) as Record<string, unknown>;
      } catch {
        body = {};
      }
      patches.push({ at: Date.now(), body });
    }
  });

  const { pid, videoId } = await readyDub(request);
  const foundMix = (await (await request.get("/api/v1/mix")).json()) as {
    dub: MixMemory;
    song?: MixMemory;
  };
  console.log(
    `[#184 G2] owner path on Dabing playlist ${pid}, video ${videoId}; dub mix as found ` +
      JSON.stringify(foundMix.dub),
  );
  cleanup = { pid, foundDabing: foundMix.dub.dabing };
  let kind = "";

  // ── The owner's clicks ─────────────────────────────────────────────────
  const appWs = page.waitForEvent("websocket", {
    predicate: (ws) => !ws.url().includes("preview"),
    timeout: 15000,
  });
  await page.goto("/");
  await appWs;
  // The Dabing playlist row of the dashboard's playlist list.
  const plRow = page.locator(
    `[data-testid="playlist-picker-item"][data-playlist-id="${pid}"]`,
  );
  await expect(plRow, "the Dabing playlist row is on the dashboard").toBeVisible({
    timeout: 15000,
  });
  await plRow.click();
  const card = page.locator(".playlist-card");
  const row = card.locator(`[data-testid="song-row"][data-video-id="${videoId}"]`);
  await expect(row, `video ${videoId} is in the Dabing song list`).toBeVisible({
    timeout: 15000,
  });
  await row.getByTestId("song-row-play").click();
  await expect
    .poll(
      async () => {
        const h = (await (await request.get("/api/v1/ndi/health")).json()) as Array<{
          playlist_id: number;
          frames_submitted_last_5s: number;
        }>;
        return h.find((r) => r.playlist_id === pid)?.frames_submitted_last_5s ?? 0;
      },
      { timeout: 30000, message: "the Dabing output must start decoding" },
    )
    .toBeGreaterThan(0);

  // "▶ Živý náhľad" — a real click, the gesture that lets the audio start.
  const startClickAt = Date.now();
  await card.getByTestId("preview-start").click({ timeout: 20000 });
  const video = card.getByTestId("preview-video");
  await expect(video).toBeVisible({ timeout: 20000 });
  await page.getByTestId("preview-unmute").click({ timeout: 2000 }).catch(() => {});
  await expect
    .poll(async () => video.evaluate((el: HTMLVideoElement) => el.muted), {
      timeout: 10000,
      message: "the preview must end up UNMUTED (the owner hears it)",
    })
    .toBe(false);
  await installAudioTap(video);

  // ── (a) audible within 10 s ────────────────────────────────────────────
  const a = await waitForLevel(page, "audible", startClickAt + 10_000);
  console.log(
    `[#184 G2] (a) audible ${a.at === null ? "NEVER" : `${a.at - startClickAt} ms`} after ` +
      `"Živý náhľad"; dBFS: ${a.trace}`,
  );
  expect(a.at, `(a) the preview must be audible (> ${AUDIBLE_DB} dBFS) within 10 s`).not.toBeNull();

  // ── (b) all three faders to 0 → silent within 4 s of the last PATCH ───
  for (const id of ["mix-vokaly", "mix-podklad", "mix-dabing"]) {
    await expect(page.getByTestId(id), `${id} fader is live`).toBeEnabled({ timeout: 15000 });
  }
  const patchesBefore = patches.length;
  for (const [id, key] of [
    ["mix-vokaly", "vokaly"],
    ["mix-podklad", "podklad"],
    ["mix-dabing", "dabing"],
  ] as const) {
    await dragFader(page, id, 0.1, 0.99);
    await expect
      .poll(
        () => patches.slice(patchesBefore).some((p) => p.body[key] === 0),
        { timeout: 5000, message: `dragging ${id} to the bottom must PATCH ${key}=0` },
      )
      .toBe(true);
  }
  const zeroPatches = patches.slice(patchesBefore);
  kind = String(zeroPatches[zeroPatches.length - 1].body.kind ?? "");
  expect(kind, "a ready dub video drives the DUB mix memory").toBe("dub");
  const lastZeroPatchAt = Math.max(...zeroPatches.map((p) => p.at));
  const mixAtZero = (await (await request.get("/api/v1/mix")).json()) as Record<string, MixMemory>;
  expect(mixAtZero[kind], `GET /api/v1/mix shows the ${kind} memory at 0/0/0`).toEqual(
    expect.objectContaining({ vokaly: 0, podklad: 0, dabing: 0 }),
  );
  const b = await waitForLevel(
    page,
    "quiet",
    lastZeroPatchAt + FADER_AUDIBLE_WITHIN_MS + STREAK * LEVEL_POLL_MS + 500,
  );
  console.log(
    `[#184 G2] (b) silent ${b.at === null ? "NEVER" : `${b.at - lastZeroPatchAt} ms`} after ` +
      `the last PATCH; dBFS: ${b.trace}`,
  );
  expect(b.at, `(b) all faders at 0 → the preview must fall below ${SILENT_DB} dBFS`).not.toBeNull();
  expect(
    b.at! - lastZeroPatchAt,
    `(b) the preview must be silent within ${FADER_AUDIBLE_WITHIN_MS} ms of the last PATCH`,
  ).toBeLessThanOrEqual(FADER_AUDIBLE_WITHIN_MS);

  // ── (c) vokály back to the top → audible within 4 s ────────────────────
  const patchesBeforeUp = patches.length;
  await dragFader(page, "mix-vokaly", 0.99, 0.01);
  await expect
    .poll(
      () => patches.slice(patchesBeforeUp).some((p) => Number(p.body.vokaly) >= 0.95),
      { timeout: 5000, message: "dragging mix-vokaly to the top must PATCH vokaly ≈ 1" },
    )
    .toBe(true);
  const upPatchAt = Math.max(...patches.slice(patchesBeforeUp).map((p) => p.at));
  const c = await waitForLevel(
    page,
    "audible",
    upPatchAt + FADER_AUDIBLE_WITHIN_MS + STREAK * LEVEL_POLL_MS + 500,
  );
  console.log(
    `[#184 G2] (c) audible ${c.at === null ? "NEVER" : `${c.at - upPatchAt} ms`} after the ` +
      `vokály PATCH; dBFS: ${c.trace}`,
  );
  expect(c.at, `(c) vokály up → the preview must be audible (> ${AUDIBLE_DB} dBFS)`).not.toBeNull();
  expect(
    c.at! - upPatchAt,
    `(c) the preview must be audible within ${FADER_AUDIBLE_WITHIN_MS} ms of the PATCH`,
  ).toBeLessThanOrEqual(FADER_AUDIBLE_WITHIN_MS);

  // The soak runs on a full bed — vokály, podklad AND dabing up (real mouse) —
  // so a pause in the (mostly spoken) dub is not read as a frozen preview.
  await dragFader(page, "mix-podklad", 0.99, 0.01);
  await dragFader(page, "mix-dabing", 0.99, 0.01);

  // ── (d) + (e): 3 minutes with the preview open ─────────────────────────
  const socketsAtSoakStart = previewSockets.length;
  const soakStart = Date.now();
  const soak: Array<{ t: number; db: number }> = [];
  let lyrSeen = 0;
  while (Date.now() - soakStart < SOAK_MS) {
    const s = await sample(page);
    const t = Date.now() - soakStart;
    soak.push({ t, db: s.db });
    const run = longestSilentRunMs(soak, SILENT_DB);
    expect(
      run,
      `(d) at ${t} ms the preview has been silent (< ${SILENT_DB} dBFS) for ${run} ms ` +
        `(max ${MAX_SILENT_RUN_MS}) with the mix non-zero — frozen / starved preview`,
    ).toBeLessThanOrEqual(MAX_SILENT_RUN_MS);
    expect(
      previewSockets.length,
      `(d) at ${t} ms: ${previewSockets.length} preview.ws connections — more than 1 reconnect`,
    ).toBeLessThanOrEqual(2);
    if (s.lyrInside !== null) {
      lyrSeen += 1;
      expect(s.lyrInside, `(e) at ${t} ms the active lyric line left its panel: ${s.lyrDetail}`).toBe(
        true,
      );
    }
    await page.waitForTimeout(Math.max(0, SOAK_SAMPLE_MS - ((Date.now() - soakStart) - t)));
  }
  const dbs = soak.map((x) => x.db).filter((d) => Number.isFinite(d));
  console.log(
    `[#184 G2] (d) ${soak.length} samples over ${SOAK_MS} ms: longest silent run ` +
      `${longestSilentRunMs(soak, SILENT_DB)} ms, dBFS min ${Math.min(...dbs).toFixed(1)} ` +
      `max ${Math.max(...dbs).toFixed(1)}; preview.ws connections ${previewSockets.length} ` +
      `(${previewSockets.length - socketsAtSoakStart} during the soak); ` +
      `(e) active lyric line seen in ${lyrSeen} samples, always inside the panel`,
  );
  expect(lyrSeen, "(e) the lyrics panel must show an active line during the 3 minutes").toBeGreaterThan(0);
});
