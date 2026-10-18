import { test, expect, APIRequestContext } from "@playwright/test";

// #279 P3a: a playlist whose pipeline decodes its song GPU-resident (its
// `/api/v1/ndi/health` row's `gpu_decoded`: the pictures stay in VRAM, in
// its producer's ring) gives the live preview's taps no picture before P3b,
// so a preview started there would stay black. The Player says so instead
// of offering it (`sp_core::player_view::gpu_preview`), and offers it again
// once the song decodes in CPU memory (a fall back to software, the next
// song). Zero console errors, per browser-console-zero-errors.md.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

const PREVIEW_GPU = "Náhľad nedostupný pri GPU dekódovaní.";

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  await request.post("/__mock/ndi-health-reset");
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
});

test.afterEach(async ({ request }) => {
  // The health rows are global in-memory mock state: never leak them.
  await request.post("/__mock/ndi-health-reset");
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

/** Worship's (1) health row says its song decodes on the GPU, or not. */
async function worshipGpuDecoded(request: APIRequestContext, gpu: boolean) {
  const rows = await (await request.get("/api/v1/ndi/health")).json();
  const worship = rows.find((r: { playlist_id: number }) => r.playlist_id === 1);
  expect(worship, "the default fixture has Worship's row").toBeTruthy();
  worship.gpu_decoded = gpu;
  const set = await request.post("/__mock/ndi-health", { data: rows });
  expect(set.ok()).toBeTruthy();
}

test("a song decoded on the GPU offers no live preview, and says why (#279 P3a)", async ({
  page,
  request,
}) => {
  await worshipGpuDecoded(request, true);
  // Worship plays on program (the replay); `?playlist=1` opens its card.
  await page.goto("/?playlist=1");
  const card = page.locator(".playlist-card");
  await expect(page.getByTestId("player-state")).toHaveText("Hrá", { timeout: 15000 });
  await expect(card.getByTestId("preview-placeholder")).toHaveText(PREVIEW_GPU, {
    timeout: 10000,
  });
  await expect(card.getByTestId("preview-start")).toHaveCount(0);
  await expect(card.getByTestId("preview-video")).toHaveCount(0);

  // The song decodes in CPU memory again (a fall back, the next song): the
  // preview is offered once more, from the next health poll.
  await worshipGpuDecoded(request, false);
  await expect(card.getByTestId("preview-start")).toBeVisible({ timeout: 10000 });
  await expect(card.getByTestId("preview-placeholder")).not.toHaveText(PREVIEW_GPU);
});

test("a song decoded in CPU memory keeps the live preview (#279 P3a)", async ({ page }) => {
  // The default rows say `gpu_decoded: false`.
  await page.goto("/?playlist=1");
  const card = page.locator(".playlist-card");
  await expect(page.getByTestId("player-state")).toHaveText("Hrá", { timeout: 15000 });
  await expect(card.getByTestId("preview-start")).toBeVisible({ timeout: 10000 });
  await expect(page.getByTestId("player")).not.toContainText(PREVIEW_GPU);
});
