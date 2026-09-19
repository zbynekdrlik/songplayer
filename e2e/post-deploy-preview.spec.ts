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
});
