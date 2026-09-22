import { test, expect, Page } from "@playwright/test";

/**
 * Post-deploy Dabing checks on the REAL box (#184 D5 item 2 + #200).
 *
 * The Dabing output (SP-dabing) is never on program during CI, so starting the
 * sample dub here touches only the sp-dabing OBS inputs, never the wall. What
 * this proves after every deploy:
 *  1. the Dabing section lists a READY dub (the 40-min acceptance sample) and
 *     the SP-dabing NDI output carries at least one receiver;
 *  2. the shared Player on /dabing is driven by a REAL mouse: a drag on the
 *     mix-vokaly fader PATCHes the console and stays put, a drag on the seek bar posts a
 *     seek — the two controls the owner found dead on 20.9.2026 (#200);
 *  3. zero console errors throughout.
 */

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];
let dabingPid = 0;
let sampleVideoId = 0;

async function mouseDrag(page: Page, selector: string, from: number, to: number) {
  // page.mouse has no auto-scroll: a control below the fold receives nothing.
  await page.locator(selector).scrollIntoViewIfNeeded();
  const box = await page.locator(selector).boundingBox();
  if (!box) throw new Error(`no bounding box for ${selector}`);
  const vertical = box.height > box.width;
  const pt = (f: number) =>
    vertical
      ? { x: box.x + box.width / 2, y: box.y + box.height * (1 - f) }
      : { x: box.x + box.width * f, y: box.y + box.height / 2 };
  const a = pt(from);
  const b = pt(to);
  await page.mouse.move(a.x, a.y);
  await page.mouse.down();
  await page.mouse.move(b.x, b.y, { steps: 8 });
  await page.mouse.up();
}

test.describe.serial("Dabing output on the box (#184, #200)", () => {
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

  test.afterAll(async ({ request }) => {
    // Leave the box as found: the full default console, Dabing output paused.
    await request
      .patch("/api/v1/mix", { data: { vokaly: 1.0, podklad: 1.0, dabing: 1.0 } })
      .catch(() => {});
    if (dabingPid) {
      await request.post(`/api/v1/playback/${dabingPid}/pause`);
    }
  });

  // Best-effort 12–16 kHz band energy (dB) of the preview <video>'s audio via a
  // Web Audio AnalyserNode. Returns null when audio cannot be captured (a
  // codec-less runner) so the mechanical console checks still run (#184 G item 3).
  async function bandDb(page: Page): Promise<number | null> {
    return page.evaluate(async () => {
      const video = document.querySelector(
        '[data-testid="preview-video"] video, video',
      ) as HTMLVideoElement | null;
      if (!video) return null;
      try {
        const stream =
          (video as unknown as { captureStream?: () => MediaStream }).captureStream?.() ?? null;
        if (!stream || stream.getAudioTracks().length === 0) return null;
        const Ctx =
          (window as unknown as { AudioContext?: typeof AudioContext }).AudioContext ||
          (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
        if (!Ctx) return null;
        const ctx = new Ctx();
        const src = ctx.createMediaStreamSource(stream);
        const analyser = ctx.createAnalyser();
        analyser.fftSize = 2048;
        src.connect(analyser);
        await new Promise((r) => setTimeout(r, 800));
        const bins = new Float32Array(analyser.frequencyBinCount);
        analyser.getFloatFrequencyData(bins);
        const nyquist = ctx.sampleRate / 2;
        const binHz = nyquist / bins.length;
        let sum = 0;
        let n = 0;
        for (let i = 0; i < bins.length; i++) {
          const hz = i * binHz;
          if (hz >= 12000 && hz <= 16000 && Number.isFinite(bins[i])) {
            sum += bins[i];
            n += 1;
          }
        }
        await ctx.close();
        return n > 0 ? sum / n : null;
      } catch {
        return null;
      }
    });
  }

  test("a READY dub is listed and SP-dabing has a receiver", async ({ request }) => {
    const dab = await request.get("/api/v1/dabing");
    expect(dab.status()).toBe(200);
    const body = (await dab.json()) as {
      playlist_id: number;
      videos: Array<{ video_id?: number; id?: number; dub_status: string; chain_state: string }>;
    };
    dabingPid = body.playlist_id;
    const ready = body.videos.find((v) => v.dub_status === "ready");
    expect(ready, "at least one dub must be ready on the box").toBeTruthy();
    // DubRow carries the id as `video_id` (the row is keyed by the video).
    sampleVideoId = Number(ready!.video_id ?? ready!.id);
    expect(sampleVideoId, "the ready dub must carry a numeric video id").toBeGreaterThan(0);

    // The deploy restarts SongPlayer a couple of minutes before this suite; the
    // OBS/DistroAV inputs re-attach within the +30 s self-check window, so poll
    // (the "wall is not dark" test uses the same 60 s budget).
    let out: { ndi_name: string; connections: number } | undefined;
    await expect
      .poll(
        async () => {
          const health = await request.get("/api/v1/ndi/health");
          expect(health.status()).toBe(200);
          const rows = (await health.json()) as Array<{
            playlist_id: number;
            ndi_name: string;
            connections: number;
          }>;
          out = rows.find((r) => r.playlist_id === dabingPid);
          return out?.connections ?? -1;
        },
        { timeout: 60000, message: "SP-dabing must be advertised with ≥ 1 receiver" },
      )
      .toBeGreaterThanOrEqual(1);
    expect(out!.ndi_name).toBe("SP-dabing");
  });

  test("real mouse: the dub fader and the seek bar commit on release", async ({
    page,
    request,
  }) => {
    // The Player's transport label follows the live PlaybackStateChanged
    // message. #201 round 2 also made the on-connect replay carry the raw
    // transport (the reload assertion below proves it), so a reload/late-socket
    // dashboard now reads the honest label — but we still wait for the app
    // socket before the first click for a deterministic start (goto resolves on
    // DOM load, before the socket opens).
    const wsOpen = page.waitForEvent("websocket", {
      predicate: (ws) => !ws.url().includes("preview"),
      timeout: 15000,
    });
    await page.goto("/dabing");
    await wsOpen;
    await page.waitForTimeout(500);
    const row = page.locator(`[data-testid="song-row"][data-video-id="${sampleVideoId}"]`);
    await expect(row).toBeVisible({ timeout: 15000 });
    await expect(row.getByTestId("chip-dub")).toContainText("hotový");

    // Start the sample on the (off-program) Dabing output. The Player's
    // play/pause label follows the on-program state (the health registry maps
    // an off-program decoding pipeline to Paused), so the proof that playback
    // started is the BACKEND effect: frames flowing on the Dabing output.
    await row.getByTestId("song-row-play").click();
    await expect
      .poll(
        async () => {
          const h = (await (await request.get("/api/v1/ndi/health")).json()) as Array<{
            playlist_id: number;
            frames_submitted_last_5s: number;
          }>;
          return h.find((r) => r.playlist_id === dabingPid)?.frames_submitted_last_5s ?? 0;
        },
        { timeout: 30000, message: "the Dabing output must start submitting frames" },
      )
      .toBeGreaterThan(0);

    // #201: while frames flow on the OFF-program Dabing output, the Player's
    // transport label must read `⏸ Pauza` — it follows the pipeline's own
    // decoding state, not the on/off-program state (the badge shows the latter).
    await expect(page.getByTestId("player-playpause")).toContainText("⏸ Pauza", {
      timeout: 20000,
    });
    await expect(page.getByTestId("player-program-badge")).toContainText(
      "○ Mimo programu",
    );

    // #184 round G item 3: the ONE mixer console. Start the live preview so the
    // audio band can be measured (best-effort), then drag mix-vokaly to 0 and
    // prove the original voice leaves the mix — the 12–16 kHz band drops ≥ 8 dB.
    await page.getByTestId("preview-start").click().catch(() => {});
    await page.waitForTimeout(2000);
    const before = await bandDb(page);

    const vokaly = page.getByTestId("mix-vokaly");
    await expect(vokaly).toBeEnabled({ timeout: 15000 });
    const patch = page.waitForResponse(
      (r) => r.url().includes("/api/v1/mix") && r.request().method() === "PATCH",
      { timeout: 10000 },
    );
    await mouseDrag(page, '[data-testid="mix-vokaly"]', 0.98, 0.02);
    expect((await patch).status()).toBe(200);
    await page.waitForTimeout(1500);
    // The fader stays where it was released (no snap-back).
    expect(Number(await vokaly.inputValue())).toBeLessThan(15);
    // GET /api/v1/mix reports the change (the console persisted).
    const mix = (await (await request.get("/api/v1/mix")).json()) as {
      vokaly: number;
      podklad: number;
      dabing: number;
    };
    expect(mix.vokaly).toBeLessThan(0.15);

    // Best-effort HF band drop (skips on a codec-less runner where audio can't be
    // captured — the wall audio is the owner's real acceptance).
    await page.waitForTimeout(3000);
    const after = await bandDb(page);
    if (before !== null && after !== null) {
      expect(
        before - after,
        "removing the original voice must drop the 12–16 kHz band ≥ 8 dB",
      ).toBeGreaterThanOrEqual(8);
    }

    // The Originál preset restores the original voice (vokály 1) and mutes dabing.
    await page.getByTestId("mixer-preset-original").click();
    await page.waitForTimeout(1000);
    const restored = (await (await request.get("/api/v1/mix")).json()) as {
      vokaly: number;
      dabing: number;
    };
    expect(restored.vokaly).toBeCloseTo(1, 1);
    expect(restored.dabing).toBeCloseTo(0, 1);

    // Seek bar: drag to ~30 % → a seek is posted (204) and the position follows.
    const seek = page.getByTestId("player-seek");
    await expect(seek).toBeEnabled({ timeout: 15000 });
    const seekResp = page.waitForResponse(
      (r) => r.url().includes(`/api/v1/playback/${dabingPid}/seek`) && r.request().method() === "POST",
      { timeout: 10000 },
    );
    await mouseDrag(page, '[data-testid="player-seek"]', 0.05, 0.3);
    expect((await seekResp).status()).toBe(204);

    // #201 round 2: a reload WHILE the off-program dub still decodes must read
    // ⏸ Pauza immediately — the on-connect replay now carries the pipeline's
    // RAW transport (round 1 replayed the scene-reconciled Paused label, so a
    // reload showed ▶ Prehrať until the next live message). Wait for the app
    // socket after the reload so any follow-up reaches an open socket.
    const wsAfterReload = page.waitForEvent("websocket", {
      predicate: (ws) => !ws.url().includes("preview"),
      timeout: 15000,
    });
    await page.reload();
    await wsAfterReload;
    await expect(page.getByTestId("player-playpause")).toContainText(
      "⏸ Pauza",
      { timeout: 15000 },
    );

    // #201: pausing the off-program dub flips the transport label to `▶ Prehrať`
    // (the pipeline stops decoding), proving the label tracks the pipeline.
    await request.post(`/api/v1/playback/${dabingPid}/pause`);
    await expect(page.getByTestId("player-playpause")).toContainText(
      "▶ Prehrať",
      { timeout: 10000 },
    );
  });
});
