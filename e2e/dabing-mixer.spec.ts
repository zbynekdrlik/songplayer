import { test, expect } from "@playwright/test";

// #184 round G: the ONE live mixer follows the playing DUB video on EVERY page,
// because `store.dabing` is filled by an App-level poll. A ready dub adds the
// mix-dabing fader; a plain stems song shows only mix-vokaly + mix-podklad. This
// spec proves that WITHOUT navigating to /dabing, plus exactly one PATCH per
// preset click. Zero console errors.

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
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/tick", { data: { enabled: false } });
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/dabing-reset");
  await request.post("/__mock/mix-reset");
  await request.post("/__mock/tick", { data: { enabled: false } });
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test.describe("the dub fader follows the playing dub video on every page", () => {
  test("Dashboard shows mix-dabing without visiting /dabing", async ({
    page,
    request,
  }) => {
    // A ready dub for video 1 — the video the Dashboard's playlist 1 plays by
    // default (the mock WS marks playlist 1 Playing with video_id 1).
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // The App-level poll populated store.dabing → the shared Player's LiveMixer
    // shows the dabing fader for the playing dub, WITHOUT visiting /dabing.
    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-vokaly")).toBeVisible();
    expect(new URL(page.url()).pathname).toBe("/");
  });

  test("Live shows mix-dabing without visiting /dabing", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 700,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });

    await page.goto("/live");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // Make the dub the playing item on the live playlist (id 184).
    await request.post("/__mock/now-playing", {
      data: {
        playlist_id: 184,
        video_id: 700,
        song: "Kázeň",
        duration_ms: 200000,
      },
    });

    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    expect(new URL(page.url()).pathname).toBe("/live");
  });

  test("a stems-ready dub shows all three faders on one strip", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: "done",
      },
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-vokaly")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-podklad")).toBeVisible();
    await expect(page.getByTestId("mix-dabing")).toBeVisible();
    // Still ONE strip (not two mixers).
    await expect(page.locator(".mixer.mixer-live")).toHaveCount(1);
  });

  test("a plain stems song shows only mix-vokaly + mix-podklad", async ({
    page,
  }) => {
    // No dub rows (reset in beforeEach) → the playing song (video 1) is a plain
    // stems song: no dabing fader.
    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-vokaly")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-podklad")).toBeVisible();
    await expect(page.getByTestId("mix-dabing")).toHaveCount(0);
  });
});

test.describe("a dub preset click issues exactly one PATCH", () => {
  test("clicking Originál PATCHes /mix exactly once", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 1,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: null,
      },
    });

    let patchCount = 0;
    await page.route("**/api/v1/mix", async (route) => {
      if (route.request().method() === "PATCH") patchCount += 1;
      await route.continue();
    });

    await page.goto("/");
    await expect(page.getByTestId("mix-presets")).toBeVisible({
      timeout: 15000,
    });

    await page.getByTestId("mixer-preset-original").click();
    await expect.poll(() => patchCount, { timeout: 5000 }).toBe(1);
    await page.waitForTimeout(1000);
    expect(patchCount, "one preset click = one PATCH").toBe(1);
  });
});

// #184 round G1: the ONE strip remembers its faders PER ITEM KIND (song vs dub).
// A dub mixed to a custom value survives a song played in between and comes back;
// a song at full mix never drags the next dub video's vokály. The console keeps
// TWO memories, selected by the playing item's kind — proven on Dashboard AND Live.
test.describe("the strip snaps to the other memory when the item's kind flips", () => {
  const npReady = (playlist_id: number, video_id: number, title: string) => [
    {
      playlist_id,
      video_id,
      title,
      stems_state: "ready",
      stems_error: null,
      queue_position: null,
    },
  ];

  test("Dashboard: a dub mix survives a song and comes back", async ({
    page,
    request,
  }) => {
    // Play a dub (the Dashboard's playlist-1 item is video 1): add its ready dub
    // row and tell /mix the playing item is that dub.
    await request.post("/__mock/dabing-add", {
      data: { video_id: 1, title: "Kázeň", dub_status: "ready", stem_status: null },
    });
    await request.post("/__mock/karaoke-now-playing", {
      data: npReady(1, 1, "Kázeň"),
    });

    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    // Kind = dub → the dabing fader is present and the dub default is (0,1,1).
    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    const vokaly = page.getByTestId("mix-vokaly");
    await expect(vokaly).toHaveValue("0", { timeout: 15000 });

    // Mix the dub: vokály → 30 %.
    await vokaly.fill("30");
    await vokaly.dispatchEvent("change");
    await expect
      .poll(
        async () => (await (await request.get("/__mock/mix-last")).json()).vokaly,
        { timeout: 5000 },
      )
      .toBeCloseTo(0.3, 5);

    // Play a song in between: drop the dub row so the item's kind flips to song.
    await request.post("/__mock/dabing-reset");
    await request.post("/__mock/karaoke-now-playing", {
      data: npReady(1, 1, "Pieseň"),
    });
    // The strip snaps to the SONG memory (1,1) with no dabing fader.
    await expect(page.getByTestId("mix-dabing")).toHaveCount(0, {
      timeout: 15000,
    });
    await expect(vokaly).toHaveValue("100", { timeout: 15000 });

    // Play the dub again → the dub memory (0.3,1,1) is restored, untouched by
    // the song in between.
    await request.post("/__mock/dabing-add", {
      data: { video_id: 1, title: "Kázeň", dub_status: "ready", stem_status: null },
    });
    await request.post("/__mock/karaoke-now-playing", {
      data: npReady(1, 1, "Kázeň"),
    });
    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    await expect(vokaly).toHaveValue("30", { timeout: 15000 });
  });

  test("Live: a dub mix survives a song and comes back", async ({
    page,
    request,
  }) => {
    await request.post("/__mock/dabing-add", {
      data: { video_id: 700, title: "Kázeň", dub_status: "ready", stem_status: null },
    });

    await page.goto("/live");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });

    // Play a given video on the live playlist (184), driving BOTH the WS
    // now-playing (store) and the /mix now-playing (kind), like a real open.
    const play = async (video_id: number, title: string) => {
      await request.post("/__mock/now-playing", {
        data: { playlist_id: 184, video_id, song: title, duration_ms: 200000 },
      });
      await request.post("/__mock/karaoke-now-playing", {
        data: npReady(184, video_id, title),
      });
    };

    await play(700, "Kázeň");
    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    const vokaly = page.getByTestId("mix-vokaly");
    await expect(vokaly).toHaveValue("0", { timeout: 15000 });
    await vokaly.fill("30");
    await vokaly.dispatchEvent("change");
    await expect
      .poll(
        async () => (await (await request.get("/__mock/mix-last")).json()).vokaly,
        { timeout: 5000 },
      )
      .toBeCloseTo(0.3, 5);

    // Play a plain song (video 999, no dub row) → the song memory (1,1), no dabing.
    await play(999, "Pieseň");
    await expect(page.getByTestId("mix-dabing")).toHaveCount(0, {
      timeout: 15000,
    });
    await expect(vokaly).toHaveValue("100", { timeout: 15000 });

    // Back to the dub → its memory (0.3,1,1) is restored.
    await play(700, "Kázeň");
    await expect(page.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    await expect(vokaly).toHaveValue("30", { timeout: 15000 });
  });
});

// #184 round G2: NO global "active kind" — the wall runs SEVERAL outputs at once,
// each fed from its OWN memory. Two players open at the SAME TIME (Dashboard = a
// plain song → the song memory; Dabing = a ready dub → the dub memory) render
// DIFFERENT memories, and a song-side PATCH leaves the dub output's strip untouched.
test.describe("two outputs render different memories at the same time", () => {
  const DABING_PLAYLIST_ID = 500;

  test("a song-side PATCH leaves the dub output's memory intact", async ({
    page,
    request,
  }) => {
    // A ready dub for the Dabing item (701); video 1 stays a plain song (no dub row).
    await request.post("/__mock/dabing-add", {
      data: {
        video_id: 701,
        title: "Kázeň",
        dub_status: "ready",
        stem_status: "done",
      },
    });

    // Page A — Dashboard: playlist 1 plays the plain song (video 1) → the SONG strip
    // (default (1,1), no dabing fader).
    await page.goto("/");
    await expect(page.getByTestId("player")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("mix-dabing")).toHaveCount(0, {
      timeout: 15000,
    });
    const songVokaly = page.getByTestId("mix-vokaly");
    await expect(songVokaly).toHaveValue("100", { timeout: 15000 });

    // Page B — Dabing: the dub item plays on playlist 500 → the DUB strip (0,1,1).
    const pageB = await page.context().newPage();
    pageB.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[B ${msg.type()}] ${msg.text()}`);
      }
    });
    await pageB.goto("/dabing");
    await expect(
      pageB.locator(
        `[data-testid="dabing-list"] .song-row[data-video-id="701"]`,
      ),
    ).toBeVisible({ timeout: 15000 });
    await request.post("/__mock/now-playing", {
      data: {
        playlist_id: DABING_PLAYLIST_ID,
        video_id: 701,
        song: "Kázeň",
        duration_ms: 200000,
      },
    });
    const dubVokaly = pageB.getByTestId("mix-vokaly");
    await expect(pageB.getByTestId("mix-dabing")).toBeVisible({ timeout: 15000 });
    await expect(dubVokaly).toHaveValue("0", { timeout: 15000 });

    // Mix the DUB output (page B): vokály → 40 % (writes ONLY the dub memory).
    await dubVokaly.fill("40");
    await dubVokaly.dispatchEvent("change");
    await expect
      .poll(
        async () => (await (await request.get("/__mock/mix-last")).json()).kind,
        { timeout: 5000 },
      )
      .toBe("dub");

    // Now a SONG PATCH on the OTHER output (page A): vokály → 60 %.
    await songVokaly.fill("60");
    await songVokaly.dispatchEvent("change");
    await expect
      .poll(
        async () => (await (await request.get("/__mock/mix-last")).json()).kind,
        { timeout: 5000 },
      )
      .toBe("song");

    // The dub output's strip is UNTOUCHED by the song edit — its memory is its own.
    await expect(dubVokaly).toHaveValue("40", { timeout: 15000 });
    // The server holds two independent memories: the song moved, the dub did not.
    const mix = (await (await request.get("/api/v1/mix")).json()) as {
      song: { vokaly: number };
      dub: { vokaly: number };
    };
    expect(mix.dub.vokaly).toBeCloseTo(0.4, 5);
    expect(mix.song.vokaly).toBeCloseTo(0.6, 5);

    await pageB.close();
  });
});
