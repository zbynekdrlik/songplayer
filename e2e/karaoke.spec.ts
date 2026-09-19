import { test, expect } from "@playwright/test";

// E2E coverage for the song (karaoke) side of the modern mixer (#181), keeping
// the #14/#177/#186 behaviour after the karaoke panel was replaced by the shared
// Mixer: the mode `<select>` became a preset-button row and the panel became
// `.mixer.mixer-karaoke`, but the live control over GET/POST /api/v1/karaoke and
// the per-song stems state contract are unchanged. The NDI audio band-drop is
// verified on the wall by the supervisor (a browser cannot observe NDI audio).

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

const READY_NP = [
  {
    playlist_id: 1,
    video_id: 1,
    title: "Never Gonna Give You Up",
    stems_state: "ready",
    stems_error: null,
    queue_position: null,
  },
];

async function setNowPlaying(page, arr) {
  await page.request.post("/__mock/karaoke-now-playing", { data: arr });
}

test.afterEach(async ({ page }) => {
  // Restore the default ready fixture so the shared mock state does not leak
  // into sibling specs that assume the controls are enabled.
  await setNowPlaying(page, READY_NP);
});

test("mixer loads current state, the now-playing song + the stem counter (#14/#177)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-karaoke");
  await expect(mixer).toBeVisible({ timeout: 10000 });

  // The active preset reflects the mock's current mode (full_mix).
  await expect(
    mixer.locator('[data-testid="mixer-preset-full_mix"]'),
  ).toHaveClass(/active/);

  // #177: the state line names the SELECTED playlist's playing song + its ready
  // glyph (the mock's playlist-1 song is ready).
  const state = page.locator('[data-testid="karaoke-now-playing"]');
  await expect(state).toContainText("Never Gonna Give You Up");
  await expect(state).toContainText("pripravené");

  // #177: the global done/pending counter is now visible in the mixer title.
  await expect(mixer.locator(".mixer-title")).toContainText("5");
});

test("a preset button POSTs the mode and the mock records it (#14)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-karaoke");
  await expect(mixer).toBeVisible({ timeout: 10000 });

  const postPromise = page.waitForRequest(
    (req) => req.url().includes("/api/v1/karaoke") && req.method() === "POST",
  );
  await mixer.locator('[data-testid="mixer-preset-instrumental_only"]').click();
  const req = await postPromise;
  expect(JSON.parse(req.postData() ?? "{}").mode).toBe("instrumental_only");

  const recorded = await page.request.get("/__mock/karaoke-last");
  expect((await recorded.json()).mode).toBe("instrumental_only");
});

test("every stem preset enables the vocal fader; Plný mix disables it (#186)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-karaoke");
  await expect(mixer).toBeVisible({ timeout: 10000 });
  const fader = page.locator('[data-testid="karaoke-vocal-gain"]');

  await mixer.locator('[data-testid="mixer-preset-full_mix"]').click();
  await expect(fader).toBeDisabled();

  for (const preset of ["karaoke_low", "vocals_only", "instrumental_only"]) {
    await mixer.locator(`[data-testid="mixer-preset-${preset}"]`).click();
    await expect(fader, `fader must be enabled in ${preset}`).toBeEnabled();
  }

  await mixer.locator('[data-testid="mixer-preset-full_mix"]').click();
  await expect(fader).toBeDisabled();
});

test("KaraokeLow vocal fader POSTs the gain and the mock records it (#14/#186)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-karaoke");
  await expect(mixer).toBeVisible({ timeout: 10000 });
  const fader = page.locator('[data-testid="karaoke-vocal-gain"]');

  await mixer.locator('[data-testid="mixer-preset-karaoke_low"]').click();
  await expect(fader).toBeEnabled();

  // The gain is an f32 in the Rust UI, so 20/100 serializes as the f32-widened
  // f64 0.20000000298023224 — match with a tolerance, never `=== 0.2`.
  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/karaoke") &&
      req.method() === "POST" &&
      Math.abs((JSON.parse(req.postData() ?? "{}").vocal_gain ?? NaN) - 0.2) <
        1e-6,
  );
  await fader.fill("20");
  await fader.dispatchEvent("change");
  await postPromise;

  const body = await (await page.request.get("/__mock/karaoke-last")).json();
  expect(body.mode).toBe("karaoke_low");
  expect(body.vocal_gain).toBeCloseTo(0.2, 5);
});

test("#177: a song without stems locks the mixer with a reason and offers re-enqueue", async ({
  page,
}) => {
  await setNowPlaying(page, [
    {
      playlist_id: 1,
      video_id: 1,
      title: "Never Gonna Give You Up",
      stems_state: "unavailable",
      stems_error: "skladba je pridlhá alebo bez vokálov",
      queue_position: null,
    },
  ]);
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-karaoke");
  await expect(mixer).toBeVisible({ timeout: 10000 });

  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("nedostupné");
  await expect(mixer).toHaveClass(/mixer-locked/);
  await expect(mixer.locator(".mixer-reason")).toBeVisible();
  await expect(page.locator('[data-testid="karaoke-vocal-gain"]')).toBeDisabled();

  const enqueue = page.locator('[data-testid="karaoke-enqueue"]');
  await expect(enqueue).toBeVisible();
  const post = page.waitForRequest(
    (r) => r.url().includes("/api/v1/stems/1/enqueue") && r.method() === "POST",
  );
  await enqueue.click();
  await post;
  const rec = await page.request.get("/__mock/stems-enqueue-last");
  expect((await rec.json()).video_id).toBe(1);
});

test("#177: a queued song shows its queue position and no re-enqueue button", async ({
  page,
}) => {
  await setNowPlaying(page, [
    {
      playlist_id: 1,
      video_id: 1,
      title: "Never Gonna Give You Up",
      stems_state: "queued",
      stems_error: null,
      queue_position: 3,
    },
  ]);
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-karaoke");
  await expect(mixer).toBeVisible({ timeout: 10000 });
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("vo fronte (3.)");
  await expect(mixer).toHaveClass(/mixer-locked/);
  await expect(page.locator('[data-testid="karaoke-enqueue"]')).toBeHidden();
});
