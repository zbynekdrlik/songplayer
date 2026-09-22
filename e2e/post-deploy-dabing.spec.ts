import { test, expect, Page, APIRequestContext, Locator } from "@playwright/test";
import { averageDb } from "./audio-helpers.mjs";

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
    // Leave the box as found: the full default DUB memory, Dabing output paused
    // (#184 round G2: kind required; the Dabing player edits the dub memory).
    await request
      .patch("/api/v1/mix", {
        data: { kind: "dub", vokaly: 1.0, podklad: 1.0, dabing: 1.0 },
      })
      .catch(() => {});
    if (dabingPid) {
      await request.post(`/api/v1/playback/${dabingPid}/pause`);
    }
  });

  // #206: N best-effort 12–16 kHz band-energy (dB) samples of the preview
  // <video>'s audio, spread over `spanMs`, from ONE AnalyserNode. Each entry is
  // the mean of the 12–16 kHz bins at that instant, or null when audio cannot be
  // captured (a codec-less runner) — averaged by `averageDb` so one snapshot
  // never decides the sign (the old single `getFloatFrequencyData` compared two
  // different live moments and went red on 22.9.2026). Returns `count` entries.
  async function collectBandSamples(
    page: Page,
    count: number,
    spanMs: number,
  ): Promise<Array<number | null>> {
    return page.evaluate(
      async ({ count, spanMs }) => {
        const video = document.querySelector(
          '[data-testid="preview-video"] video, video',
        ) as HTMLVideoElement | null;
        const nulls = (): Array<number | null> => new Array(count).fill(null);
        if (!video) return nulls();
        try {
          const stream =
            (video as unknown as { captureStream?: () => MediaStream }).captureStream?.() ?? null;
          if (!stream || stream.getAudioTracks().length === 0) return nulls();
          const Ctx =
            (window as unknown as { AudioContext?: typeof AudioContext }).AudioContext ||
            (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
          if (!Ctx) return nulls();
          const ctx = new Ctx();
          const src = ctx.createMediaStreamSource(stream);
          const analyser = ctx.createAnalyser();
          analyser.fftSize = 2048;
          src.connect(analyser);
          const bins = new Float32Array(analyser.frequencyBinCount);
          const nyquist = ctx.sampleRate / 2;
          const binHz = nyquist / bins.length;
          const bandAt = (): number | null => {
            analyser.getFloatFrequencyData(bins);
            let sum = 0;
            let n = 0;
            for (let i = 0; i < bins.length; i++) {
              const hz = i * binHz;
              if (hz >= 12000 && hz <= 16000 && Number.isFinite(bins[i])) {
                sum += bins[i];
                n += 1;
              }
            }
            return n > 0 ? sum / n : null;
          };
          // Warm the analyser, then take `count` samples spread across `spanMs`.
          await new Promise((r) => setTimeout(r, 200));
          const step = Math.max(1, Math.floor(spanMs / count));
          const out: Array<number | null> = [];
          for (let s = 0; s < count; s++) {
            out.push(bandAt());
            if (s < count - 1) await new Promise((r) => setTimeout(r, step));
          }
          await ctx.close();
          return out;
        } catch {
          return nulls();
        }
      },
      { count, spanMs },
    );
  }

  // #206: seek the (off-program) dub to a fixed audio position via the API and
  // wait until the reported live position has actually landed AND played ~1 s
  // past it, so both band measurements read the SAME audio window. Position is
  // WS-pushed onto the seek bar (there is no position endpoint — preview.md), so
  // we read the bar: phase 1 waits for the position to land near T (the decoder
  // snaps to the next keyframe, so "near" allows the GOP length), phase 2 waits
  // for it to advance 1 s past the landing point. Never a blind timeout.
  async function seekAndSettle(
    request: APIRequestContext,
    seekBar: Locator,
    pid: number,
    targetMs: number,
  ): Promise<void> {
    const resp = await request.post(`/api/v1/playback/${pid}/seek`, {
      data: { position_ms: targetMs },
    });
    expect(resp.status(), `seek to ${targetMs} ms must be accepted`).toBe(204);
    // Phase 1: the seek landed. The decoder snaps a seek to the NEXT keyframe
    // (YouTube AV1/H.264 GOPs run up to ~20 s — a seek to 645 482 ms landed at
    // 665 000 ms on the box), so "landed" = the reported position is inside
    // [T − 0.5 s, T + KEYFRAME_SNAP_MAX_MS]. The same T snaps to the same
    // keyframe both times, so both band windows still read the same audio.
    const KEYFRAME_SNAP_MAX_MS = 30000;
    const landDeadline = Date.now() + 20000;
    let landed = Number.NaN;
    for (;;) {
      const pos = Number(await seekBar.inputValue());
      if (pos >= targetMs - 500 && pos <= targetMs + KEYFRAME_SNAP_MAX_MS) {
        landed = pos;
        break;
      }
      expect(
        Date.now() < landDeadline,
        `the seek to ${targetMs} ms must land within [T − 0.5 s, T + ${KEYFRAME_SNAP_MAX_MS} ms] in 20 s — last position ${pos} ms`,
      ).toBe(true);
      await new Promise((r) => setTimeout(r, 150));
    }
    // Phase 2: play ~1 s past the landing point so the sampling window is the
    // same both times (same keyframe → same landing → same window).
    await expect
      .poll(async () => Number(await seekBar.inputValue()), {
        timeout: 20000,
        intervals: [150],
        message: `the dub must play to ${landed + 1000} ms after the seek landed at ${landed} ms`,
      })
      .toBeGreaterThanOrEqual(landed + 1000);
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
    // #206: the content-matched band compare adds two API seeks + two ~2 s
    // sampling windows over the default 90 s post-deploy budget; give this
    // real-box test the same 120 s wall-clock budget the sibling preview tests
    // use (a budget, not a loosened assertion — every gate below is unchanged).
    test.setTimeout(120_000);

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

    // #184 round G item 3 + #206: the ONE mixer console. Start the live preview
    // so the audio band can be measured, then prove that removing the original
    // voice drops the 12–16 kHz band ≥ 8 dB — measured CONTENT-MATCHED: seek to a
    // fixed audio position T, average the band over ~2 s; drag mix-vokaly to 0;
    // seek to the SAME T, average again. Comparing the same audio (only the
    // original voice removed) makes the sign deterministic — the old single
    // snapshot before/after straddled ~5 s of DIFFERENT live content and read the
    // band LOUDER (−9.3 dB) on 22.9.2026.
    await page.getByTestId("preview-start").click().catch(() => {});

    // A fixed audio position (~30 % in, deep enough for vocals — reuses where the
    // seek-bar drag below already goes), used for BOTH measurements.
    const seekBar = page.getByTestId("player-seek");
    await expect(seekBar).toBeEnabled({ timeout: 15000 });
    const duration = Number(await seekBar.getAttribute("max"));
    const T = Math.min(
      Math.max(Math.round(duration * 0.3), 60000),
      Math.max(60000, duration - 60000),
    );

    const vokaly = page.getByTestId("mix-vokaly");
    await expect(vokaly).toBeEnabled({ timeout: 15000 });
    // Read the fader so the shared box console is left as found (#206).
    const originalVokaly = (
      (await (await request.get("/api/v1/mix")).json()) as { dub: { vokaly: number } }
    ).dub.vokaly;

    try {
      // BEFORE: the original voice present. Seek to T, play ~1 s past it, average.
      await seekAndSettle(request, seekBar, dabingPid, T);
      const before = averageDb(await collectBandSamples(page, 8, 2000));

      // Remove the original voice with a REAL mouse (the #200 acceptance: the
      // control is not dead), and prove the PATCH landed to exactly 0.
      const patch = page.waitForResponse(
        (r) => r.url().includes("/api/v1/mix") && r.request().method() === "PATCH",
        { timeout: 10000 },
      );
      await mouseDrag(page, '[data-testid="mix-vokaly"]', 0.98, 0);
      expect((await patch).status()).toBe(200);
      await page.waitForTimeout(500);
      // The fader stays where it was released (no snap-back).
      expect(Number(await vokaly.inputValue())).toBeLessThan(15);
      // GET /api/v1/mix shows the original voice fully removed (#200 intent).
      const mix = (await (await request.get("/api/v1/mix")).json()) as {
        dub: { vokaly: number; podklad: number; dabing: number };
      };
      expect(
        mix.dub.vokaly,
        "dragging mix-vokaly to the bottom must set dub.vokaly to 0 (the PATCH landed)",
      ).toBe(0);

      // AFTER: the SAME audio (seek back to the same T), original voice removed.
      await seekAndSettle(request, seekBar, dabingPid, T);
      const after = averageDb(await collectBandSamples(page, 8, 2000));

      console.log(
        `[#206] dabing 12–16 kHz band before=${before} dB after=${after} dB (T=${T} ms)`,
      );
      // Best-effort HF band drop — skips only on a codec-less runner where audio
      // cannot be captured (the wall audio is the owner's real acceptance). On the
      // box (Edge, real codecs) both reads are numeric and the drop is asserted.
      if (before !== null && after !== null) {
        expect(
          before - after,
          "removing the original voice must drop the 12–16 kHz band ≥ 8 dB on the SAME audio (seek T twice)",
        ).toBeGreaterThanOrEqual(8);
      }
    } finally {
      // Leave the shared dub console + preview as found: restore the original
      // fader and stop the preview this test started.
      await request
        .patch("/api/v1/mix", { data: { kind: "dub", vokaly: originalVokaly } })
        .catch(() => {});
      await page.getByTestId("preview-stop").click().catch(() => {});
    }

    // The Originál preset restores the original voice (vokály 1) and mutes dabing.
    await page.getByTestId("mixer-preset-original").click();
    await page.waitForTimeout(1000);
    const restored = (await (await request.get("/api/v1/mix")).json()) as {
      dub: { vokaly: number; dabing: number };
    };
    expect(restored.dub.vokaly).toBeCloseTo(1, 1);
    expect(restored.dub.dabing).toBeCloseTo(0, 1);

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
