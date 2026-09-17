import { test, expect } from "@playwright/test";

// E2E coverage for #14: the dashboard karaoke control drives
// GET/POST /api/v1/karaoke. This spec opens the real dashboard, changes the
// mode dropdown + the vocal-gain slider, and verifies BOTH the POST request the
// UI sent AND the value the mock recorded (the backend effect), per
// e2e-real-user-testing.md. The actual NDI audio band-drop is verified on the
// wall by the supervisor (a browser cannot observe NDI audio).

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

test("karaoke control loads current state, now-playing song + stem progress tooltip (#14/#177)", async ({
  page,
}) => {
  await page.goto("/");
  const panel = page.locator(".karaoke-control");
  await expect(panel).toBeVisible({ timeout: 10000 });

  // Mode reflects the mock's current state (full_mix).
  await expect(page.locator('[data-testid="karaoke-mode"]')).toHaveValue(
    "full_mix",
  );
  // #177: the panel now names the SELECTED playlist's playing song + its stems
  // state (the mock's playlist-1 song is ready).
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("Never Gonna Give You Up");
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("pripravené");
  // #177: the global done/pending counter moved into the panel-title tooltip.
  await expect(page.locator('[data-testid="karaoke-title"]')).toHaveAttribute(
    "title",
    /5/,
  );
});

test("selecting Instrumental-only POSTs the mode and the mock records it (#14)", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });
  // #177: controls are enabled only once the ready-state resolves for the
  // selected playlist's song.
  await expect(page.locator('[data-testid="karaoke-mode"]')).toBeEnabled();

  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/karaoke") && req.method() === "POST",
  );
  await page
    .locator('[data-testid="karaoke-mode"]')
    .selectOption("instrumental_only");

  const req = await postPromise;
  const body = JSON.parse(req.postData() ?? "{}");
  expect(body.mode).toBe("instrumental_only");

  // Backend effect: the mock recorded the mode the UI sent.
  const recorded = await page.request.get("/__mock/karaoke-last");
  expect((await recorded.json()).mode).toBe("instrumental_only");
});

test("every stem preset enables the vocal-gain slider; Plný mix disables it (#186)", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });

  const slider = page.locator('[data-testid="karaoke-vocal-gain"]');
  const mode = page.locator('[data-testid="karaoke-mode"]');
  await expect(mode).toBeEnabled(); // #177: song is stems-ready

  // Explicitly start from Plný mix (an earlier test may have left another mode in
  // the shared mock state) → slider disabled, with a "no effect" hint.
  await mode.selectOption("full_mix");
  await expect(slider).toBeDisabled();
  await expect(
    page.locator('[data-testid="karaoke-fader-hint"]'),
  ).toContainText("bez efektu");

  // #186: the fader is live in EVERY stem preset now — not karaoke_low only.
  for (const preset of ["karaoke_low", "vocals_only", "instrumental_only"]) {
    await mode.selectOption(preset);
    await expect(slider, `slider must be enabled in ${preset}`).toBeEnabled();
  }

  // Back to full_mix → disabled again.
  await mode.selectOption("full_mix");
  await expect(slider).toBeDisabled();
});

test("KaraokeLow fader POSTs the gain and the mock records it (#14/#186)", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });

  const slider = page.locator('[data-testid="karaoke-vocal-gain"]');
  const mode = page.locator('[data-testid="karaoke-mode"]');
  await expect(mode).toBeEnabled(); // #177: song is stems-ready
  await mode.selectOption("karaoke_low");
  await expect(slider).toBeEnabled();

  // Drag the slider to 20% and release; the UI POSTs the gain as 0.2.
  // The gain is an f32 in the Rust UI, so `serde_json` serializes 20/100 as the
  // f32-widened f64 0.20000000298023224 — never bit-exactly 0.2. Match with the
  // same tolerance the backend-effect assertion below uses (toBeCloseTo(0.2, 5));
  // a strict `=== 0.2` here silently never matches and the wait times out.
  const postPromise = page.waitForRequest(
    (req) =>
      req.url().includes("/api/v1/karaoke") &&
      req.method() === "POST" &&
      Math.abs((JSON.parse(req.postData() ?? "{}").vocal_gain ?? NaN) - 0.2) <
        1e-6,
  );
  await slider.fill("20");
  await slider.dispatchEvent("change");
  await postPromise;

  // Backend effect: the mock recorded the lowered gain.
  const recorded = await page.request.get("/__mock/karaoke-last");
  const body = await recorded.json();
  expect(body.mode).toBe("karaoke_low");
  expect(body.vocal_gain).toBeCloseTo(0.2, 5);
});

// #177: bind the panel to the now-playing song + show its stems state.

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

test("#177: a song without stems disables the controls and offers re-enqueue", async ({
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
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });

  // Header names the song + the "nedostupné" glyph.
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("Never Gonna Give You Up");
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("nedostupné");

  // Controls locked, with a reason.
  await expect(page.locator('[data-testid="karaoke-mode"]')).toBeDisabled();
  await expect(page.locator('[data-testid="karaoke-vocal-gain"]')).toBeDisabled();
  await expect(
    page.locator('[data-testid="karaoke-lock-reason"]'),
  ).toBeVisible();

  // "Zaradiť do fronty" is shown; clicking it POSTs enqueue for video 1.
  const enqueue = page.locator('[data-testid="karaoke-enqueue"]');
  await expect(enqueue).toBeVisible();
  const post = page.waitForRequest(
    (r) => r.url().includes("/api/v1/stems/1/enqueue") && r.method() === "POST",
  );
  await enqueue.click();
  await post;
  // Backend effect: the mock recorded the enqueue for video 1.
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
  await expect(page.locator(".karaoke-control")).toBeVisible({ timeout: 10000 });
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("vo fronte (3.)");
  await expect(page.locator('[data-testid="karaoke-mode"]')).toBeDisabled();
  // queued is not unavailable/failed → the enqueue button stays hidden.
  await expect(page.locator('[data-testid="karaoke-enqueue"]')).toBeHidden();
});
