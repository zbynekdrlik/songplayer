/**
 * #266 post-deploy at PP: the dashboard's live A/V preview of the playlist ON
 * PROGRAM decodes video AND audio on PP's box.
 *
 * The owner, at PP (13.10.2026): "dal som si zapnut nahlad co hraje a okno
 * ostalo cierne … Nepaci sa mi ze takato chyba prechadza cez e2e a ze vobec
 * to musim tu otvarat!!!!" — SNV's suite has its preview gate
 * (`post-deploy-preview.spec.ts`), PP's subset had none. This is PP's: the
 * same checks as SNV's first preview test, on the card of the playlist on
 * program (`pp-preview-gate.ts::previewTarget`, mock-suite tested):
 *  - the row says "NA PROGRAME" (#266, words, never a glyph);
 *  - "▶ Živý náhľad" is offered and its `<video>` reaches `readyState >= 3`
 *    (the box's ffmpeg encoder child produced an init + fragments and the
 *    browser DECODED them), `currentTime` advances and
 *    `webkitAudioDecodedByteCount` GROWS (the #178 round-3 audio-track guard);
 *  - then it stops the preview, so no encoder child keeps running at PP.
 *
 * It switches no scene and presses no ▶ (PP's wall is live): nothing on
 * program, or a playlist on program that does not decode, FAILS naming why.
 * Real H.264/AAC: the `edge` project (msedge) of `post-deploy-pp.config.ts`;
 * `deploy-pp.yml` makes sure Edge is installed.
 */

import { test, expect } from "@playwright/test";
import { healthRows } from "./box-api";
import { PREVIEW_NOT_DECODING, previewTarget } from "./pp-preview-gate";
import { describeProgram, readProgramState } from "./program-state";

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

test.describe("#266 PP live preview post-deploy", () => {
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

  test("the on-program card's live preview decodes video AND audio at PP and advances", async ({
    page,
    request,
  }) => {
    // The encoder child's cold start is part of what is measured: the same
    // 120 s budget as SNV's preview tests (a budget, not a loosened check).
    test.setTimeout(120_000);

    const program = await readProgramState(request);
    console.log(`[#266 pp] ${describeProgram(program)}`);
    const pick = previewTarget(program, await healthRows(request));
    if ("error" in pick) throw new Error(`PP's preview gate: ${pick.error}`);
    if ("none" in pick) {
      // The operator's state: nothing of SongPlayer on program. The
      // dashboard still loads; the live preview is not checked this deploy.
      console.log(`[#266 pp] live preview NOT checked: ${pick.none}`);
      await page.goto("/");
      await expect(page.getByTestId("playlist-workspace")).toBeVisible({ timeout: 30_000 });
      return;
    }
    if ("explained" in pick) {
      // The operator's state: the on-program playlist does not decode. Its
      // card must say why instead of offering a preview that stays black
      // (the owner's report on #266).
      console.log(`[#266 pp] live preview NOT checked: ${pick.why}`);
      const paused = pick.explained;
      await page.goto("/");
      await expect(page.getByTestId("playlist-workspace")).toBeVisible({ timeout: 30_000 });
      const pausedRow = page.locator(
        `[data-testid="playlist-picker-item"][data-playlist-id="${paused.id}"]`,
      );
      await expect(pausedRow).toBeVisible({ timeout: 15_000 });
      await pausedRow.click();
      const pausedCard = page.locator(".playlist-card");
      await expect(pausedCard.locator(".playlist-id")).toHaveText(paused.ndi_output_name, {
        timeout: 15_000,
      });
      await expect(pausedCard.getByTestId("preview-start")).toHaveCount(0);
      const line = (await pausedCard.getByTestId("preview-placeholder").innerText()).trim();
      expect(PREVIEW_NOT_DECODING, `the card explains why: "${line}"`).toContain(line);
      return;
    }
    const target = pick.target;
    console.log(
      `[#266 pp] the preview of ${target.id} (${target.name}, ${target.ndi_output_name})`,
    );

    // Select the on-program card EXPLICITLY (never "the first card").
    await page.goto("/");
    await expect(page.getByTestId("playlist-workspace")).toBeVisible({
      timeout: 30_000,
    });
    const row = page.locator(
      `[data-testid="playlist-picker-item"][data-playlist-id="${target.id}"]`,
    );
    await expect(row, `the playlist on program, ${target.name}, is on the dashboard`).toBeVisible({
      timeout: 15_000,
    });
    // #266: its row says so in words.
    await expect(row.getByTestId("playlist-picker-status")).toHaveText("NA PROGRAME", {
      timeout: 15_000,
    });
    await row.click();
    const card = page.locator(".playlist-card");
    await expect(card.locator(".playlist-id")).toHaveText(target.ndi_output_name, {
      timeout: 15_000,
    });

    try {
      // Click-to-start: mounts the <video>, opens preview.ws → the box's
      // ffmpeg encoder child for this playlist.
      await card.getByTestId("preview-start").click({ timeout: 20_000 });
      const video = card.getByTestId("preview-video");
      await expect(video).toBeVisible({ timeout: 20_000 });

      // The encoder child must spawn on this first viewer, produce a
      // keyframe-aligned fMP4, and the browser must DECODE it. The owner's
      // black window never got here (no init segment).
      await expect
        .poll(async () => video.evaluate((el: HTMLVideoElement) => el.readyState), {
          timeout: 40_000,
          message: "the preview <video> decodes (readyState >= 3)",
        })
        .toBeGreaterThanOrEqual(3);

      const t0 = await video.evaluate((el: HTMLVideoElement) => el.currentTime);
      await expect
        .poll(async () => video.evaluate((el: HTMLVideoElement) => el.currentTime), {
          timeout: 15_000,
          message: "the preview picture advances (currentTime)",
        })
        .toBeGreaterThan(t0 + 0.05);

      // AUDIO decodes too: an audio-less fMP4 is unplayable in MSE (#178).
      const audioBytes = () =>
        video.evaluate(
          (el: HTMLVideoElement) =>
            (el as unknown as { webkitAudioDecodedByteCount: number })
              .webkitAudioDecodedByteCount ?? 0,
        );
      const a0 = await audioBytes();
      await expect
        .poll(audioBytes, {
          timeout: 15_000,
          message: "the preview audio decodes (webkitAudioDecodedByteCount grows)",
        })
        .toBeGreaterThan(a0);

      // Real decoded geometry: the encoder outputs a fixed 640x360 canvas.
      const width = await video.evaluate((el: HTMLVideoElement) => el.videoWidth);
      expect(width).toBeGreaterThan(0);

      // Stop the preview so PP's encoder child is not left running.
      await card.getByTestId("preview-stop").click();
      await expect(card.getByTestId("preview-video")).toHaveCount(0);
    } finally {
      // A failed check above still never leaves an encoder child at PP.
      const stop = card.getByTestId("preview-stop");
      if ((await stop.count()) > 0) await stop.click().catch(() => {});
    }
  });
});
