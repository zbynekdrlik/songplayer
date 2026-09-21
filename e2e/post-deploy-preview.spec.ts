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

import { test, expect, request as apiRequest } from "@playwright/test";

const SONGPLAYER_URL = process.env.SONGPLAYER_URL || "http://localhost:8920";

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

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
    test.setTimeout(150_000);

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

      // Hold 60 s under the throttle: the 500k stream fits 1 Mb/s, so the media
      // must advance ~real time — at least 50 of the 60 s (lag ≤ 5 s incl. the
      // startup ramp). Before round F this plateaued 33–38 s behind and the media
      // barely advanced.
      const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      await page.waitForTimeout(60_000);
      const t1 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      expect(
        t1 - t0,
        "the throttled preview must stay near real time (≤ 5 s behind)",
      ).toBeGreaterThanOrEqual(50);

      // The lag readout must be absent or under 5 s.
      const lag = page.getByTestId("preview-lag");
      if ((await lag.count()) > 0) {
        const n = Number((await lag.textContent())?.match(/\d+/)?.[0] ?? "0");
        expect(n, "if shown, the picture lag must be < 5 s").toBeLessThan(5);
      }

      // The dub mixer 'Originál' preset must respond fast even under the throttle:
      // the PATCH lands ≤ 2 s and the picture keeps advancing (no stall > 3 s).
      const patch = page.waitForResponse(
        (r) =>
          r.url().includes(`/api/v1/videos/${sampleVideoId}/dub-mix`) &&
          r.request().method() === "PATCH",
        { timeout: 2000 },
      );
      await page.getByTestId("mixer-preset-original").click();
      expect((await patch).status()).toBe(200);

      const before = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      await page.waitForTimeout(4000);
      const after = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      expect(
        after - before,
        "the picture must keep advancing after the mix change (no stall > 3 s)",
      ).toBeGreaterThan(0.5);
    } finally {
      // Restore: dub-only mix, stop the preview, pause the off-program output.
      await request
        .patch(`/api/v1/videos/${sampleVideoId}/dub-mix`, { data: { ratio: 1.0 } })
        .catch(() => {});
      await page
        .getByTestId("preview-stop")
        .click()
        .catch(() => {});
      await request.post(`/api/v1/playback/${dabingPid}/pause`).catch(() => {});
    }
  });
});
