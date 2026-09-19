import { test, expect } from "@playwright/test";

// E2E coverage for #180 (dubbing D1): the Dabing section page (paste-URL add →
// queued row + Prehrať) and the per-row Dabing toggle on a playlist's song list.
// Opens the real dashboard, interacts with the UI, and asserts BOTH the visible
// result AND the backend effect the mock recorded, per e2e-real-user-testing.md.

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
  // Clean slate for the shared mock process (serial workers).
  await request.post("/__mock/dabing-reset");
});

test.afterEach(async () => {
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("Dabing page renders with the nav entry and empty state (#180)", async ({
  page,
}) => {
  await page.goto("/");
  await page.locator(".navbar button", { hasText: "Dabing" }).click();
  await expect(page.locator(".dabing-page h2")).toHaveText("Dabing");
  // #194: the Dabing page uses the shared ImportBox (`import-input`/`import-btn`).
  await expect(page.locator('[data-testid="import-input"]')).toBeVisible();
  await expect(page.locator(".dabing-empty")).toBeVisible();
});

test("pasting a URL adds a queued row and ▶ dispatches play (#180/#194)", async ({
  page,
}) => {
  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  await page
    .locator('[data-testid="import-input"]')
    .fill("https://youtu.be/AvWOCj48pGw");
  await page.locator('[data-testid="import-btn"]').click();

  // A queued row appears (import + poll refresh) as the shared SongRow.
  const row = page.locator('[data-testid="dabing-list"] [data-testid="song-row"]').first();
  await expect(row).toBeVisible({ timeout: 5000 });
  // #194: the engine chain is gone from the DOM and lives in the dub chip's
  // tooltip; a queued row's dub chip reads "vo fronte", and the chain path
  // folds into its `title`.
  const dubChip = row.locator('[data-testid="chip-dub"]');
  await expect(dubChip).toHaveText("vo fronte");
  await expect(dubChip).toHaveAttribute("title", /stiahnuté/);

  // ▶ dispatches a play-video POST to the Dabing playlist.
  const postPromise = page.waitForRequest(
    (req) =>
      /\/api\/v1\/playlists\/\d+\/play-video/.test(req.url()) &&
      req.method() === "POST",
  );
  await row.locator('[data-testid="song-row-play"]').click();
  const req = await postPromise;
  expect(req.url()).toMatch(/\/play-video$/);
});

test("a dub-ready video WITHOUT stems renders the dub chip hotový (#183 round 2/#194)", async ({
  page,
  request,
}) => {
  // Round 2: long videos the stem worker cannot separate are dubbed via the
  // 2-stream mix and reach `ready` with NO stems. The Dabing row must render the
  // ready dub chip ("hotový") for such a row exactly like a stemmed one — the
  // ready state is first-class regardless of stems.
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 344,
      title: "Morning Prayer & Devotion",
      dub_status: "ready",
      chain_state: "ready",
      stem_status: null, // no stems — the 2-stream DubOverOriginal case
      dub_file_path: "/c/morning_dub.flac",
    },
  });

  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  // The row appears via the 2 s poll; #194: the ready state shows as the dub
  // chip "hotový" and the chain path folds into its tooltip ("pripravené").
  const row = page.locator('[data-testid="dabing-list"] [data-testid="song-row"]').first();
  await expect(row).toBeVisible({ timeout: 5000 });
  const dubChip = row.locator('[data-testid="chip-dub"]');
  await expect(dubChip).toHaveText("hotový");
  await expect(dubChip).toHaveAttribute("title", /pripravené/);
  // A ready row still shows the dub-mix ratio line.
  await expect(row.locator('[data-testid="dabing-row-ratio"]')).toContainText(
    "Pomer dabingu",
  );
});

test("the chain tooltip shows stemy + titulky and drops prepis/preklad with stems (#182/#194)", async ({
  page,
  request,
}) => {
  // #182: the real engine chain. A video with stems shows the stemy step, the
  // new titulky step, and NO prepis/preklad (subtitles come from the dub session).
  // #194: the chain path is no longer its own DOM block — it lives in the dub
  // chip's `title` tooltip.
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 350,
      title: "Kazen so stemami",
      dub_status: "synth",
      chain_state: "synth",
      stem_status: "done",
    },
  });
  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  const dubChip = page
    .locator('[data-testid="dabing-list"] [data-testid="song-row"]')
    .first()
    .locator('[data-testid="chip-dub"]');
  await expect(dubChip).toBeVisible({ timeout: 5000 });
  await expect(dubChip).toHaveAttribute("title", /stemy/);
  await expect(dubChip).toHaveAttribute("title", /titulky/);
  await expect(dubChip).not.toHaveAttribute("title", /prepis/);
  await expect(dubChip).not.toHaveAttribute("title", /preklad/);
});

test("an over-cap dub (unsupported stems) shows no stemy step in the tooltip (#182/#194)", async ({
  page,
  request,
}) => {
  // A long video the stem worker cannot separate (stem_status "unsupported") is
  // dubbed over the original; its chain omits the stemy step entirely.
  await request.post("/__mock/dabing-add", {
    data: {
      video_id: 351,
      title: "Dlha kazen",
      dub_status: "ready",
      chain_state: "ready",
      stem_status: "unsupported",
      dub_file_path: "/c/dlha_dub.flac",
    },
  });
  await page.goto("/dabing");
  await expect(page.locator(".dabing-page h2")).toBeVisible({ timeout: 10000 });

  const dubChip = page
    .locator('[data-testid="dabing-list"] [data-testid="song-row"]')
    .first()
    .locator('[data-testid="chip-dub"]');
  await expect(dubChip).toBeVisible({ timeout: 5000 });
  await expect(dubChip).toHaveAttribute("title", /stiahnuté/);
  await expect(dubChip).toHaveAttribute("title", /titulky/);
  await expect(dubChip).toHaveAttribute("title", /pripravené/);
  await expect(dubChip).not.toHaveAttribute("title", /stemy/);
  await expect(dubChip).not.toHaveAttribute("title", /prepis/);
  await expect(dubChip).not.toHaveAttribute("title", /preklad/);
});

test("the Dabing row toggle flips dub_requested via PATCH (#180)", async ({
  page,
  request,
}) => {
  await page.goto("/");
  const card = page.locator(".playlist-card", { hasText: "Worship" });
  await expect(card).toBeVisible({ timeout: 10000 });

  // #194: the song list is OPEN by default; rows are `.song-row`.
  await expect(card.locator(".video-list")).toBeVisible({ timeout: 5000 });

  const row = card.locator(".song-row", {
    hasText: "Never Gonna Give You Up",
  });
  await expect(row).toBeVisible();

  const patchPromise = page.waitForRequest(
    (req) =>
      /\/api\/v1\/videos\/\d+\/dub$/.test(req.url()) && req.method() === "PATCH",
  );
  await row.locator('[data-testid="dub-toggle"]').check();
  await patchPromise;

  // Backend effect: the mock recorded the toggle as requested=true.
  const last = await (await request.get("/__mock/dub-toggle-last")).json();
  expect(last.toggle.requested).toBe(true);
});
