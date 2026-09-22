import { test, expect } from "@playwright/test";

// E2E for #184 round G: the ONE live mixer — three independent faders
// (mix-vokaly / mix-podklad / mix-dabing) over GET/PATCH /api/v1/mix, rendered
// identically wherever the shared Player mounts. Verifies which faders are live
// per the playing item, that a fader move PATCHes its own field, that presets are
// fader snapshots, the disabled reason, keyboard operability, and a clean console.

const ALLOWED_CONSOLE = [
  /WebSocket connection/,
  /favicon/,
  /wasm.*instantiate/,
  /module specifier/,
  /integrity.*attribute.*ignored/,
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/dabing-reset");
});

test.afterEach(async ({ page, request }) => {
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await setNowPlaying(page, READY_NP);
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

const DABING_PLAYLIST_ID = 500;
async function playDubInPlayer(page, videoId, title) {
  await expect(
    page.locator(
      `[data-testid="dabing-list"] .song-row[data-video-id="${videoId}"]`,
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

// ── Song (stems) mixer — Dashboard ──────────────────────────────────────────

test.describe("the song mixer (stems)", () => {
  test("a stems-ready song shows mix-vokaly + mix-podklad live, no mix-dabing", async ({
    page,
  }) => {
    await page.goto("/");
    const mixer = page.locator(".mixer.mixer-live");
    await expect(mixer).toBeVisible({ timeout: 10000 });
    await expect(
      page.locator('[data-testid="karaoke-now-playing"]'),
    ).toContainText("Stemy — Never Gonna Give You Up");
    // Two live faders + four song presets, no dabing fader for a plain song.
    await expect(mixer.locator(".mixer-channel")).toHaveCount(2);
    await expect(mixer.locator(".mixer-preset")).toHaveCount(4);
    await expect(page.getByTestId("mix-dabing")).toHaveCount(0);
    await expect(page.getByTestId("mix-vokaly")).toBeEnabled();
    await expect(page.getByTestId("mix-podklad")).toBeEnabled();
  });

  test("dragging mix-podklad to a mid value PATCHes {podklad} and lights no preset", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.locator(".mixer.mixer-live")).toBeVisible({
      timeout: 10000,
    });
    const fader = page.getByTestId("mix-podklad");
    const patch = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/mix") && req.method() === "PATCH",
    );
    // 50 % podklad with vokály full is a CUSTOM mix — no named preset (0 would be
    // vocals-only, which IS a preset, so use a mid value).
    await fader.fill("50");
    await fader.dispatchEvent("change");
    await patch;
    const last = await (await page.request.get("/__mock/mix-last")).json();
    expect(last.podklad).toBeCloseTo(0.5, 5);
    expect(last.vokaly).toBeUndefined();
    await expect(page.locator(".mixer-preset.active")).toHaveCount(0);
  });

  test("the Iba hudba preset PATCHes {vokaly:0,podklad:1} and lights itself", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.locator(".mixer.mixer-live")).toBeVisible({
      timeout: 10000,
    });
    const patch = page.waitForRequest(
      (req) =>
        req.url().includes("/api/v1/mix") && req.method() === "PATCH",
    );
    await page.getByTestId("mixer-preset-instrumental_only").click();
    await patch;
    const last = await (await page.request.get("/__mock/mix-last")).json();
    expect(last.vokaly).toBeCloseTo(0, 5);
    expect(last.podklad).toBeCloseTo(1, 5);
    expect(last.dabing).toBeUndefined();
    await expect(
      page.getByTestId("mixer-preset-instrumental_only"),
    ).toHaveClass(/active/);
  });

  test("the vokaly fader is keyboard operable", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".mixer.mixer-live")).toBeVisible({
      timeout: 10000,
    });
    const fader = page.getByTestId("mix-vokaly");
    await expect(fader).toBeEnabled();
    await fader.focus();
    const before = await fader.inputValue();
    await fader.press("ArrowDown");
    const after = await fader.inputValue();
    expect(after).not.toBe(before);
  });

  test("a song without stems locks the mixer with a reason and offers re-enqueue", async ({
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
    const mixer = page.locator(".mixer.mixer-live");
    await expect(mixer).toBeVisible({ timeout: 10000 });
    await expect(mixer).toHaveClass(/mixer-locked/);
    await expect(mixer.locator(".mixer-reason")).toContainText("bez vokálov");
    await expect(page.getByTestId("mix-vokaly")).toBeDisabled();
    await expect(page.getByTestId("mix-podklad")).toBeDisabled();

    const enqueue = page.getByTestId("mix-enqueue");
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

test.describe("the dub mixer", () => {
  test("a stems-ready dub shows all three faders live; mix-vokaly PATCHes {vokaly}", async ({
    page,
  }) => {
    await page.request.post("/__mock/dabing-add", {
      data: {
        video_id: 701,
        title: "Svedectvo",
        dub_status: "ready",
        stem_status: "done",
      },
    });
    await page.goto("/dabing");
    await playDubInPlayer(page, 701, "Svedectvo");
    const mixer = page.locator(".mixer.mixer-live").first();
    await expect(mixer).toBeVisible({ timeout: 10000 });
    await expect(mixer.locator(".mixer-channel")).toHaveCount(3);
    await expect(page.getByTestId("mix-vokaly")).toBeEnabled();
    await expect(page.getByTestId("mix-podklad")).toBeEnabled();
    await expect(page.getByTestId("mix-dabing")).toBeEnabled();

    const fader = page.getByTestId("mix-vokaly");
    const patch = page.waitForRequest(
      (req) => req.url().includes("/api/v1/mix") && req.method() === "PATCH",
    );
    await fader.fill("0");
    await fader.dispatchEvent("change");
    await patch;
    const last = await (await page.request.get("/__mock/mix-last")).json();
    expect(last.vokaly).toBeCloseTo(0, 5);
    expect(last.podklad).toBeUndefined();
  });

  test("a no-stems dub shows mix-vokaly + mix-dabing live and mix-podklad locked", async ({
    page,
  }) => {
    await page.request.post("/__mock/dabing-add", {
      data: {
        video_id: 700,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });
    await page.goto("/dabing");
    await playDubInPlayer(page, 700, "Kázeň");
    const mixer = page.locator(".mixer.mixer-live").first();
    await expect(mixer).toBeVisible({ timeout: 10000 });
    await expect(page.getByTestId("mix-vokaly")).toBeEnabled();
    await expect(page.getByTestId("mix-dabing")).toBeEnabled();
    const podklad = page.getByTestId("mix-podklad");
    await expect(podklad).toBeDisabled();
    // The locked podklad carries the "po separácii" note.
    await expect(
      mixer.locator(".mixer-channel", { hasText: "po separácii" }),
    ).toBeVisible();
  });

  test("a dub preset PATCHes all three and marks itself active", async ({
    page,
  }) => {
    await page.request.post("/__mock/dabing-add", {
      data: {
        video_id: 702,
        title: "Svedectvo B",
        dub_status: "ready",
        stem_status: "done",
      },
    });
    await page.goto("/dabing");
    await playDubInPlayer(page, 702, "Svedectvo B");
    const mixer = page.locator(".mixer.mixer-live").first();
    await expect(mixer).toBeVisible({ timeout: 10000 });

    const patch = page.waitForRequest(
      (req) => req.url().includes("/api/v1/mix") && req.method() === "PATCH",
    );
    await mixer.getByTestId("mixer-preset-half").click();
    await patch;
    const last = await (await page.request.get("/__mock/mix-last")).json();
    expect(last.vokaly).toBeCloseTo(0.5, 5);
    expect(last.podklad).toBeCloseTo(1, 5);
    expect(last.dabing).toBeCloseTo(0.5, 5);
    await expect(mixer.getByTestId("mixer-preset-half")).toHaveClass(/active/);
  });
});

// ── one strip, every page ───────────────────────────────────────────────────

test("the same mixer strip renders on Dashboard, Live and Dabing", async ({
  page,
}) => {
  for (const path of ["/", "/live", "/dabing"]) {
    await page.goto(path);
    // Every page mounts the shared Player, whose mixer slot is the ONE live strip
    // (or the idle placeholder when nothing plays — both are the same component).
    const strip = page.locator(".mixer.mixer-live");
    const idle = page.getByTestId("player-mixer-idle");
    await expect(strip.or(idle).first()).toBeVisible({ timeout: 15000 });
  }
});
