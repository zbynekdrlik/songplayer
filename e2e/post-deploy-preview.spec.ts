/**
 * #178 post-deploy: the dashboard card's live A/V preview `<video>` (MSE, fed
 * fragmented MP4 over `preview.ws` by the box's on-demand ffmpeg encoder child)
 * must actually DECODE and PLAY on the deployed box.
 *
 * It runs against the REAL deployed SongPlayer, so it must decode real H.264/AAC
 * — which Playwright's bundled Chromium cannot. It therefore runs ONLY under the
 * `edge` project in `post-deploy.config.ts` (channel 'msedge' — Edge is always
 * present on the Windows box and carries the proprietary codecs); the default
 * `chromium` project ignores this file.
 *
 * It does NOT drive OBS at all (safest for the shared live wall — no scene
 * change whatsoever): it verifies the preview for WHATEVER playlist is currently
 * on program. During the deploy the box runs its normal live wall, so a playlist
 * is playing and the dashboard auto-selects it. Round 3: the preview is
 * CLICK-to-start, so the test CLICKS the `preview-start` control to mount the
 * `<video>` (opening `preview.ws` → the box's on-demand ffmpeg child), then
 * clicks stop at the end so nothing keeps encoding. If nothing is on program it
 * fails loudly — a deployed live wall is expected to be playing.
 */

import { test, expect, request as apiRequest, Locator, Page } from "@playwright/test";

const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

// #184 owner-path probes (canvas frame-hash + Web Audio RMS + real-mouse drag).
// The preview <video> is MSE (same-origin blob) so drawing it to a canvas does
// NOT taint it and getImageData works.

/** A cheap FNV-1a hash of a downscaled preview frame — a stable hash across
 *  samples means the picture is frozen. */
async function frameHash(video: Locator): Promise<number> {
  return video.evaluate((el: HTMLVideoElement) => {
    const c = document.createElement("canvas");
    c.width = 64;
    c.height = 36;
    const ctx = c.getContext("2d")!;
    ctx.drawImage(el, 0, 0, 64, 36);
    const d = ctx.getImageData(0, 0, 64, 36).data;
    let h = 2166136261;
    for (let i = 0; i < d.length; i += 4) {
      h = (h ^ (d[i] + d[i + 1] * 3 + d[i + 2] * 7)) >>> 0;
      h = (h * 16777619) >>> 0;
    }
    return h;
  });
}

/** Install a one-time Web Audio RMS tap on the preview <video>. */
async function installAudioTap(video: Locator): Promise<void> {
  await video.evaluate((el: HTMLVideoElement) => {
    const w = window as unknown as {
      __rmsAnalyser?: AnalyserNode;
      __rmsBuf?: Float32Array;
      AudioContext: typeof AudioContext;
      webkitAudioContext?: typeof AudioContext;
    };
    if (w.__rmsAnalyser) return;
    const AC = w.AudioContext || w.webkitAudioContext!;
    const ctx = new AC();
    const src = ctx.createMediaElementSource(el);
    const an = ctx.createAnalyser();
    an.fftSize = 2048;
    src.connect(an);
    an.connect(ctx.destination);
    w.__rmsAnalyser = an;
    w.__rmsBuf = new Float32Array(an.fftSize);
    void ctx.resume();
  });
}

/** Current RMS amplitude of the preview <video>'s audio (0 = silence). */
async function audioRms(video: Locator): Promise<number> {
  return video.evaluate(() => {
    const w = window as unknown as {
      __rmsAnalyser?: AnalyserNode;
      __rmsBuf?: Float32Array;
    };
    const an = w.__rmsAnalyser;
    const buf = w.__rmsBuf;
    if (!an || !buf) return 0;
    an.getFloatTimeDomainData(buf);
    let s = 0;
    for (let i = 0; i < buf.length; i++) s += buf[i] * buf[i];
    return Math.sqrt(s / buf.length);
  });
}

async function currentTime(video: Locator): Promise<number> {
  return video.evaluate((el: HTMLVideoElement) => el.currentTime);
}

/** Real mouse drag along a horizontal range input from `from` to `to` (fractions). */
async function mouseDragRange(page: Page, selector: string, from: number, to: number): Promise<void> {
  await page.locator(selector).scrollIntoViewIfNeeded();
  const box = await page.locator(selector).boundingBox();
  if (!box) throw new Error(`no bounding box for ${selector}`);
  const pt = (f: number) => ({ x: box.x + box.width * f, y: box.y + box.height / 2 });
  const a = pt(from);
  const b = pt(to);
  await page.mouse.move(a.x, a.y);
  await page.mouse.down();
  await page.mouse.move(b.x, b.y, { steps: 8 });
  await page.mouse.up();
}

test.describe("#178 live preview <video> post-deploy", () => {
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

  test("the playing card's preview <video> decodes video AND audio on the box and advances", async ({
    page,
  }) => {
    // A playlist must be on program (the live wall). No OBS is driven.
    const ctx = await apiRequest.newContext({ baseURL: SONGPLAYER_URL });
    let active: number[] = [];
    try {
      const status = (await (await ctx.get("/api/v1/status")).json()) as {
        active_playlist_ids?: number[];
      };
      active = status.active_playlist_ids ?? [];
    } finally {
      await ctx.dispose();
    }
    expect(
      active.length,
      "a playlist must be on program (the deployed live wall should be playing) for the preview <video> to exist",
    ).toBeGreaterThan(0);

    // The dashboard auto-selects the playing playlist. Round 3: click its
    // preview-start control to mount the <video> on demand (opens preview.ws →
    // the box's ffmpeg child).
    await page.goto("/");
    await expect(page.getByTestId("playlist-workspace")).toBeVisible({
      timeout: 30_000,
    });
    const card = page.locator(".playlist-card");
    await card.getByTestId("preview-start").click({ timeout: 20_000 });
    const video = card.getByTestId("preview-video");
    await expect(video).toBeVisible({ timeout: 20_000 });

    // The box's ffmpeg encoder child must spawn on this first viewer, produce a
    // keyframe-aligned fMP4, and the browser must DECODE it: readyState >= 3.
    await expect
      .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
        timeout: 40_000,
      })
      .toBeGreaterThanOrEqual(3);

    const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
    await expect
      .poll(async () => video.evaluate((el: HTMLVideoElement) => el.currentTime), {
        timeout: 15_000,
      })
      .toBeGreaterThan(t0 + 0.05);

    // AUDIO decodes too — the round-3 root cause was an audio-less fMP4 (PCM
    // wall-clock stamps → zero audio packets → empty audio track → unplayable in
    // MSE). webkitAudioDecodedByteCount must GROW, proving real audio frames.
    const a0 = await video.evaluate(
      (el: HTMLVideoElement) =>
        (el as unknown as { webkitAudioDecodedByteCount: number })
          .webkitAudioDecodedByteCount ?? 0,
    );
    await expect
      .poll(
        async () =>
          video.evaluate(
            (el: HTMLVideoElement) =>
              (el as unknown as { webkitAudioDecodedByteCount: number })
                .webkitAudioDecodedByteCount ?? 0,
          ),
        { timeout: 15_000 },
      )
      .toBeGreaterThan(a0);

    // Real decoded geometry — the encoder outputs a fixed 640x360 canvas.
    const width = await video.evaluate((el: HTMLVideoElement) => el.videoWidth);
    expect(width).toBeGreaterThan(0);

    // Stop the preview so the box's encoder child is not left running.
    await card.getByTestId("preview-stop").click();
    await expect(card.getByTestId("preview-video")).toHaveCount(0);
  });

  test("throttled to 1 Mb/s the preview stays live and the dub mixer preset is responsive (#184)", async ({
    page,
    request,
  }) => {
    // #184 round F: the owner's actual condition — watching the preview over a
    // ~1 Mb/s internet link — reproduced with CDP network emulation. The 500k
    // stream + 4-fragment (2 s) backlog must keep the picture within ~5 s of the
    // wall, and a control change (the dub mixer 'Originál' preset) must still land
    // fast with the picture uninterrupted. Driven on the OFF-program Dabing output
    // (never the live wall), following post-deploy-dabing.spec.ts.
    //
    // The liveness is proven by a BOUNDED, early-exit `expect.poll` (media reaches
    // t0 + 15 s within ~20 s wall), NOT a fixed 60 s soak — the project's CLAUDE.md
    // hard rule forbids a sleep-dominated test on the gating post-deploy path; a
    // ~15 s window already distinguishes the fix (media tracks real time) from the
    // bug (the backlog plateaued ~33 s behind and the media barely advanced). The
    // full 60 s throttled soak is the supervisor's manual box verification
    // (design acceptance item 4: probe-preview-throttled.mjs 1000 100 150).
    test.setTimeout(120_000);

    const dab = await request.get("/api/v1/dabing");
    expect(dab.status()).toBe(200);
    const body = (await dab.json()) as {
      playlist_id: number;
      videos: Array<{ video_id?: number; id?: number; dub_status: string }>;
    };
    const dabingPid = body.playlist_id;
    const ready = body.videos.find((v) => v.dub_status === "ready");
    expect(
      ready,
      "a ready dub must exist on the box for the throttled preview test",
    ).toBeTruthy();
    const sampleVideoId = Number(ready!.video_id ?? ready!.id);
    expect(sampleVideoId).toBeGreaterThan(0);

    await page.goto("/dabing");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 30_000 });

    // Start the dub on the off-program Dabing output; prove playback by frames.
    const row = page.locator(
      `[data-testid="song-row"][data-video-id="${sampleVideoId}"]`,
    );
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.getByTestId("song-row-play").click();
    await expect
      .poll(
        async () => {
          const h = (await (await request.get("/api/v1/ndi/health")).json()) as Array<{
            playlist_id: number;
            frames_submitted_last_5s: number;
          }>;
          return (
            h.find((r) => r.playlist_id === dabingPid)?.frames_submitted_last_5s ?? 0
          );
        },
        { timeout: 30_000, message: "the Dabing output must start decoding" },
      )
      .toBeGreaterThan(0);

    // Throttle to ~1 Mb/s / 100 ms RTT BEFORE starting the preview.
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Network.enable");
    await cdp.send("Network.emulateNetworkConditions", {
      offline: false,
      downloadThroughput: 1_000_000 / 8,
      uploadThroughput: 1_000_000 / 8,
      latency: 100,
    });

    try {
      await page.getByTestId("preview-start").click({ timeout: 20_000 });
      const video = page.getByTestId("preview-video");
      await expect(video).toBeVisible({ timeout: 20_000 });
      await expect
        .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
          timeout: 40_000,
        })
        .toBeGreaterThanOrEqual(3);

      // Bounded, early-exit liveness poll (NOT a fixed soak): the 500k stream
      // fits 1 Mb/s, so under the throttle the media tracks ~real time and reaches
      // t0 + 15 s within ~20 s wall — the poll resolves the moment it does. Before
      // round F the backlog plateaued 33–38 s behind and the media barely advanced,
      // so it would never reach t0 + 15 s and the poll times out (fail). This
      // proves the plateau is gone without a sleep-dominated gating test.
      const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      await expect
        .poll(
          async () => video.evaluate((el: HTMLVideoElement) => el.currentTime),
          {
            timeout: 25_000,
            message:
              "the throttled preview must track real time — the media must keep advancing, not plateau behind the wall",
          },
        )
        .toBeGreaterThanOrEqual(t0 + 15);

      // The lag readout must be absent or under 5 s.
      const lag = page.getByTestId("preview-lag");
      if ((await lag.count()) > 0) {
        const n = Number((await lag.textContent())?.match(/\d+/)?.[0] ?? "0");
        expect(n, "if shown, the picture lag must be < 5 s").toBeLessThan(5);
      }

      // The mixer 'Originál' preset must respond fast even under the throttle:
      // the PATCH /api/v1/mix lands ≤ 2 s and the picture keeps advancing (#184 G).
      const patch = page.waitForResponse(
        (r) =>
          r.url().includes("/api/v1/mix") && r.request().method() === "PATCH",
        { timeout: 2000 },
      );
      await page.getByTestId("mixer-preset-original").click();
      expect((await patch).status()).toBe(200);

      // Poll (≤ 3 s, early-exit) that the picture advances after the mix change —
      // proves no stall > 3 s, without a fixed sleep.
      const before = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      await expect
        .poll(
          async () => video.evaluate((el: HTMLVideoElement) => el.currentTime),
          {
            timeout: 3000,
            message:
              "the picture must keep advancing after the mix change (no stall > 3 s)",
          },
        )
        .toBeGreaterThan(before + 0.5);
    } finally {
      // Restore: full console (dub-only default), stop the preview, pause output.
      await request
        .patch("/api/v1/mix", { data: { vokaly: 1.0, podklad: 1.0, dabing: 1.0 } })
        .catch(() => {});
      await page
        .getByTestId("preview-stop")
        .click()
        .catch(() => {});
      await request.post(`/api/v1/playback/${dabingPid}/pause`).catch(() => {});
    }
  });

  test("pause freezes the preview picture + audio within 3 s, and a real-mouse seek lands on target — the owner's path (#184)", async ({
    page,
    request,
  }) => {
    // #184: the owner's actual complaint path, proven per deploy on the box —
    // (1) clicking the Player's pause freezes the preview PICTURE (canvas
    // frame-hash stable >= 2 s) AND its AUDIO (Web Audio RMS goes quiet) within
    // 3 s; (2) resuming and a REAL-mouse seek lands the seek bar on the target
    // and the pipeline actually fast-forwards there. Driven on the OFF-program
    // Dabing output (never the live wall), following the throttled test above.
    // All timings are printed. Bounded, early-exit polls only — no fixed soak.
    test.setTimeout(120_000);

    const dab = await request.get("/api/v1/dabing");
    expect(dab.status()).toBe(200);
    const body = (await dab.json()) as {
      playlist_id: number;
      videos: Array<{ video_id?: number; id?: number; dub_status: string }>;
    };
    const dabingPid = body.playlist_id;
    const ready = body.videos.find((v) => v.dub_status === "ready");
    expect(
      ready,
      "a ready dub must exist on the box for the pause/seek proof",
    ).toBeTruthy();
    const sampleVideoId = Number(ready!.video_id ?? ready!.id);
    expect(sampleVideoId).toBeGreaterThan(0);

    await page.goto("/dabing");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 30_000 });

    const row = page.locator(
      `[data-testid="song-row"][data-video-id="${sampleVideoId}"]`,
    );
    await expect(row).toBeVisible({ timeout: 15_000 });
    await row.getByTestId("song-row-play").click();
    await expect
      .poll(
        async () => {
          const h = (await (await request.get("/api/v1/ndi/health")).json()) as Array<{
            playlist_id: number;
            frames_submitted_last_5s: number;
          }>;
          return (
            h.find((r) => r.playlist_id === dabingPid)?.frames_submitted_last_5s ?? 0
          );
        },
        { timeout: 30_000, message: "the Dabing output must start decoding" },
      )
      .toBeGreaterThan(0);

    // Mount the live preview and let it decode.
    await page.getByTestId("preview-start").click({ timeout: 20_000 });
    const video = page.getByTestId("preview-video");
    await expect(video).toBeVisible({ timeout: 20_000 });
    await expect
      .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
        timeout: 40_000,
      })
      .toBeGreaterThanOrEqual(3);

    // Unmute + install the Web Audio RMS tap (the preview started from a real
    // user gesture, so the AudioContext resumes).
    await page.getByTestId("preview-unmute").click().catch(() => {});
    await installAudioTap(video);

    try {
      // --- (1) PAUSE: the picture freezes AND the audio goes quiet within 3 s ---
      // Prove it is PLAYING first: the preview media clock advances + audible.
      const ct0 = await currentTime(video);
      await page.waitForTimeout(1000);
      expect(
        await currentTime(video),
        "the preview must be playing (media advancing) before pause",
      ).toBeGreaterThan(ct0 + 0.1);

      let baseRms = 0;
      for (let i = 0; i < 6; i++) {
        baseRms = Math.max(baseRms, await audioRms(video));
        await page.waitForTimeout(150);
      }
      console.log(`[#184] preview baseline audio RMS while playing: ${baseRms.toFixed(4)}`);
      expect(
        baseRms,
        "the preview audio must be audible before pause (proves the RMS tap works)",
      ).toBeGreaterThan(0.01);

      // Pause the pipeline via the Player toggle.
      await page.getByTestId("player-playpause").click();
      const pauseAt = Date.now();

      // Frame-hash freeze: track the last frame change; it must be within 3 s of
      // pause and the picture must then stay stable >= 2 s. Bounded, early-exit.
      let lastHash = await frameHash(video);
      let lastChangeMs = Date.now() - pauseAt;
      let freezeStableMs = 0;
      const freezeDeadline = Date.now() + 6000;
      while (Date.now() < freezeDeadline) {
        await page.waitForTimeout(250);
        const nowMs = Date.now() - pauseAt;
        const h = await frameHash(video);
        if (h !== lastHash) {
          lastChangeMs = nowMs;
          lastHash = h;
        } else if (nowMs - lastChangeMs >= 2000) {
          freezeStableMs = nowMs - lastChangeMs;
          break;
        }
      }
      console.log(
        `[#184] preview picture last changed ${lastChangeMs} ms after pause, then stable ${freezeStableMs} ms`,
      );
      expect(
        lastChangeMs,
        "the preview picture must freeze within 3 s of pause",
      ).toBeLessThanOrEqual(3000);
      expect(
        freezeStableMs,
        "the frozen picture must stay stable >= 2 s",
      ).toBeGreaterThanOrEqual(2000);

      // Audio goes quiet within 3 s of pause (early-exit poll).
      let silenceMs: number | null = null;
      await expect
        .poll(
          async () => {
            const r = await audioRms(video);
            if (r < 0.005 && silenceMs === null) silenceMs = Date.now() - pauseAt;
            return r;
          },
          {
            timeout: 3500,
            intervals: [200],
            message: "the preview audio must go quiet within 3 s of pause",
          },
        )
        .toBeLessThan(0.005);
      console.log(`[#184] preview audio went quiet ${silenceMs} ms after pause`);

      // --- (2) RESUME + a real-mouse seek lands on the target ---
      await page.getByTestId("player-playpause").click();
      const seek = page.getByTestId("player-seek");
      await expect(seek).toBeEnabled({ timeout: 15_000 });
      // Resume is proven by the WS-fed position (the seek bar) advancing again —
      // independent of the preview <video> re-buffering.
      const posResume0 = Number(await seek.inputValue());
      await expect
        .poll(async () => Number(await seek.inputValue()), {
          timeout: 15_000,
          message: "playback must resume (the live position advances) after unpause",
        })
        .toBeGreaterThan(posResume0 + 500);

      const duration = Number(await seek.getAttribute("max"));
      const preDrag = Number(await seek.inputValue());
      // ~ +60 s ahead, clamped below the end.
      const target = Math.min(
        preDrag + 60000,
        Math.max(duration - 5000, preDrag + 20000),
      );
      expect(target).toBeGreaterThan(preDrag + 1500);

      let committed: number | null = null;
      await page.route("**/api/v1/playback/*/seek", async (route) => {
        const b = JSON.parse(route.request().postData() || "{}");
        if (typeof b.position_ms === "number") committed = b.position_ms;
        await route.continue();
      });
      await mouseDragRange(
        page,
        '[data-testid="player-seek"]',
        preDrag / duration,
        target / duration,
      );
      await expect
        .poll(() => committed, {
          timeout: 5000,
          message: "the seek must commit exactly one POST",
        })
        .not.toBeNull();
      const seekTarget = committed as number;
      console.log(
        `[#184] seek: preDrag ${preDrag} ms, committed target ${seekTarget} ms, duration ${duration} ms`,
      );

      // The bar DISPLAYS >= the committed target immediately (the pending hold)
      // and never drops below the pre-drag value after the commit.
      await expect
        .poll(async () => Number(await seek.inputValue()), {
          timeout: 5000,
          message: "the bar must display >= the committed target during the fast-forward",
        })
        .toBeGreaterThanOrEqual(seekTarget);
      let minAfter = Number.POSITIVE_INFINITY;
      const dipT0 = Date.now();
      while (Date.now() - dipT0 < 3000) {
        minAfter = Math.min(minAfter, Number(await seek.inputValue()));
        await page.waitForTimeout(200);
      }
      console.log(
        `[#184] min displayed position after commit: ${minAfter} ms (pre-drag ${preDrag} ms)`,
      );
      expect(
        minAfter,
        "the bar must never drop below the pre-drag value after the commit",
      ).toBeGreaterThanOrEqual(preDrag);

      // Backend proof: the bar can only EXCEED the target once the pending hold
      // releases to the real live WS-fed position — i.e. the pipeline actually
      // fast-forwarded to the target. A failed seek would let the 5 s hold expire
      // and the bar drop back to the stale position, never exceeding the target.
      const ffStart = Date.now();
      await expect
        .poll(async () => Number(await seek.inputValue()), {
          timeout: 10_000,
          intervals: [400],
          message:
            "the backend must fast-forward: the live position (once the hold releases) must pass the seek target",
        })
        .toBeGreaterThan(seekTarget);
      console.log(
        `[#184] backend position reached (and passed) the seek target in ~${Date.now() - ffStart} ms`,
      );
    } finally {
      await page.getByTestId("preview-stop").click().catch(() => {});
      await request.post(`/api/v1/playback/${dabingPid}/pause`).catch(() => {});
    }
  });
});
