import { test, expect } from "@playwright/test";

// E2E for #181 (D2): the ONE modern mixer, in both of its homes —
//   • the dashboard karaoke mixer (song stems, POST /api/v1/karaoke)
//   • the Dabing-page dub mixer (PATCH /api/v1/videos/{id}/dub-mix)
// Verifies faders drive the API on `change`, presets apply, the disabled state
// carries a reason when the source isn't ready, keyboard operability, and a
// clean console — per e2e-real-user-testing.md + browser-console-zero-errors.md.

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

// ── Song (karaoke) mixer — dashboard ────────────────────────────────────────

test.describe("song mixer", () => {
  test.afterEach(async ({ page }) => {
    await setNowPlaying(page, READY_NP);
  });

  test("renders the mixer with the now-playing state line (#181)", async ({
    page,
  }) => {
    await page.goto("/");
    const mixer = page.locator(".mixer.mixer-karaoke");
    await expect(mixer).toBeVisible({ timeout: 10000 });
    // The #177 state contract is preserved: the state line names the song.
    await expect(
      page.locator('[data-testid="karaoke-now-playing"]'),
    ).toContainText("Stemy — Never Gonna Give You Up");
    // Two channels + four presets.
    await expect(mixer.locator(".mixer-channel")).toHaveCount(2);
    await expect(mixer.locator(".mixer-preset")).toHaveCount(4);
  });

  test("a preset button POSTs its mode and the mock records it (#181)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.locator(".mixer.mixer-karaoke")).toBeVisible({
      timeout: 10000,
    });

    const postPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/karaoke") && req.method() === "POST",
    );
    await page.locator('[data-testid="mixer-preset-instrumental_only"]').click();
    const req = await postPromise;
    expect(JSON.parse(req.postData() ?? "{}").mode).toBe("instrumental_only");

    const recorded = await page.request.get("/__mock/karaoke-last");
    expect((await recorded.json()).mode).toBe("instrumental_only");
    // The active preset carries the accent class.
    await expect(
      page.locator('[data-testid="mixer-preset-instrumental_only"]'),
    ).toHaveClass(/active/);
  });

  test("the vocal fader is live in stem presets, off in Plný mix, and POSTs the gain (#181/#186)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.locator(".mixer.mixer-karaoke")).toBeVisible({
      timeout: 10000,
    });
    const fader = page.locator('[data-testid="karaoke-vocal-gain"]');

    // Plný mix → the fader is a no-op (disabled).
    await page.locator('[data-testid="mixer-preset-full_mix"]').click();
    await expect(fader).toBeDisabled();

    // Every stem preset enables it.
    for (const p of ["karaoke_low", "vocals_only", "instrumental_only"]) {
      await page.locator(`[data-testid="mixer-preset-${p}"]`).click();
      await expect(fader, `fader must be live in ${p}`).toBeEnabled();
    }

    // In Karaoke, dragging the fader to 20 % POSTs vocal_gain ≈ 0.2.
    await page.locator('[data-testid="mixer-preset-karaoke_low"]').click();
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
    expect(body.vocal_gain).toBeCloseTo(0.2, 5);
  });

  test("the vocal fader is keyboard operable (#181)", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".mixer.mixer-karaoke")).toBeVisible({
      timeout: 10000,
    });
    await page.locator('[data-testid="mixer-preset-karaoke_low"]').click();
    const fader = page.locator('[data-testid="karaoke-vocal-gain"]');
    await expect(fader).toBeEnabled();
    await fader.focus();
    const before = await fader.inputValue();
    await fader.press("ArrowUp");
    const after = await fader.inputValue();
    expect(after).not.toBe(before);
  });

  test("a song without stems locks the mixer with a reason and offers re-enqueue (#181/#177)", async ({
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

    // Locked: the reason shows, the vocal fader is disabled.
    await expect(mixer).toHaveClass(/mixer-locked/);
    await expect(mixer.locator(".mixer-reason")).toBeVisible();
    await expect(mixer.locator(".mixer-reason")).toContainText("bez vokálov");
    await expect(page.locator('[data-testid="karaoke-vocal-gain"]')).toBeDisabled();

    // The footer stays usable: "Zaradiť do fronty" enqueues video 1.
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
});

// ── Dub mixer — Dabing page ─────────────────────────────────────────────────

// #194: the dub mixer no longer renders per dabing ROW — it lives in the Dabing
// page's shared <Player/> (top of the page) and appears only when the PLAYING
// item on the Dabing playlist (id 500) is a dub video. The Player picks the dub
// adapter when the now-playing `video_id` for the playlist matches a row in
// `store.dabing`. Drive it: dabing-add the row, wait for it to land in
// store.dabing (its list row renders), then broadcast a NowPlaying for the
// Dabing playlist carrying that `video_id`.
const DABING_PLAYLIST_ID = 500;
async function playDubInPlayer(page, videoId, title) {
  await expect(
    page.locator(
      `[data-testid="dabing-list"] .dabing-row[data-video-id="${videoId}"]`,
    ),
  ).toBeVisible({ timeout: 10000 });
  await page.request.post("/__mock/now-playing", {
    data: {
      playlist_id: DABING_PLAYLIST_ID,
      video_id: videoId,
      song: title,
      duration_ms: 200000,
    },
  });
}

test.describe("dub mixer", () => {
  test.beforeEach(async ({ page }) => {
    await page.request.post("/__mock/dabing-reset");
  });

  test("a dub-ready video WITHOUT stems shows two faders and the dabing fader PATCHes the ratio (#181/#182)", async ({
    page,
  }) => {
    // A dub-ready video WITHOUT stems (stem_status null → 2-stream mix) is a
    // first-class ready state; the mixer renders TWO faders (ambient hidden) and
    // drives the ratio API.
    await page.request.post("/__mock/dabing-add", {
      data: {
        video_id: 700,
        title: "Svedectvo",
        dub_status: "ready",
        dub_mix_ratio: 1.0,
        stem_status: null,
      },
    });
    await page.goto("/dabing");
    await playDubInPlayer(page, 700, "Svedectvo");
    const mixer = page.locator(".mixer.mixer-dub").first();
    await expect(mixer).toBeVisible({ timeout: 10000 });
    // #182: without stems only originál + dabing (no ambient) — two channels +
    // three presets.
    await expect(mixer.locator(".mixer-channel")).toHaveCount(2);
    await expect(mixer.locator(".mixer-preset")).toHaveCount(3);

    const fader = mixer.locator('[data-testid="dub-mix-fader"]');
    await expect(fader).toBeEnabled();
    const patchPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/videos/700/dub-mix") &&
        req.method() === "PATCH",
    );
    await fader.fill("40");
    await fader.dispatchEvent("change");
    await patchPromise;
    const last = await (await page.request.get("/__mock/dub-mix-last")).json();
    expect(last.video_id).toBe(700);
    expect(last.ratio).toBeCloseTo(0.4, 5);
  });

  test("a dub preset PATCHes its ratio and marks itself active (#181)", async ({
    page,
  }) => {
    await page.request.post("/__mock/dabing-add", {
      data: {
        video_id: 701,
        title: "Svedectvo B",
        dub_status: "ready",
        dub_mix_ratio: 1.0,
        stem_status: "done",
      },
    });
    await page.goto("/dabing");
    await playDubInPlayer(page, 701, "Svedectvo B");
    const mixer = page.locator(".mixer.mixer-dub").first();
    await expect(mixer).toBeVisible({ timeout: 10000 });
    // #182: WITH stems the full three-fader strip (originál hlas / dabing /
    // ambient).
    await expect(mixer.locator(".mixer-channel")).toHaveCount(3);

    const patchPromise = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/videos/701/dub-mix") &&
        req.method() === "PATCH",
    );
    await mixer.locator('[data-testid="mixer-preset-half"]').click();
    await patchPromise;
    const last = await (await page.request.get("/__mock/dub-mix-last")).json();
    expect(last.ratio).toBeCloseTo(0.5, 5);
    await expect(mixer.locator('[data-testid="mixer-preset-half"]')).toHaveClass(
      /active/,
    );
  });

  test("a dub that isn't generated yet is locked with a reason (#181)", async ({
    page,
  }) => {
    await page.request.post("/__mock/dabing-add", {
      data: {
        video_id: 702,
        title: "Svedectvo C",
        dub_status: "queued",
        dub_mix_ratio: 1.0,
        stem_status: null,
      },
    });
    await page.goto("/dabing");
    await playDubInPlayer(page, 702, "Svedectvo C");
    const mixer = page.locator(".mixer.mixer-dub").first();
    await expect(mixer).toBeVisible({ timeout: 10000 });
    await expect(mixer).toHaveClass(/mixer-locked/);
    await expect(mixer.locator(".mixer-reason")).toContainText(
      "ešte nie je vygenerovaný",
    );
    await expect(mixer.locator('[data-testid="dub-mix-fader"]')).toBeDisabled();
  });
});
