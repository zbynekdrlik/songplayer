import { test, expect } from "@playwright/test";

// The song side of the ONE live mixer (#184 round G) keeps the #14/#177/#186
// behaviour: the now-playing state line names the song + its stems state, the
// stem counter shows in the title, presets drive the faders, and Plný mix pins
// the original bit-exact (both faders full). The NDI audio band-drop is verified
// on the wall by the supervisor (a browser cannot observe NDI audio).

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

test.afterEach(async ({ page, request }) => {
  await request.post("/__mock/mix-reset");
  await setNowPlaying(page, READY_NP);
  const real = consoleMessages.filter(
    (m) => !ALLOWED_CONSOLE.some((r) => r.test(m)),
  );
  expect(real).toEqual([]);
});

test("mixer loads the now-playing song + the stem counter (#14/#177)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-live");
  await expect(mixer).toBeVisible({ timeout: 10000 });

  // The active preset reflects the default console (both faders full = Plný mix).
  await expect(page.getByTestId("mixer-preset-full_mix")).toHaveClass(/active/);

  // #177: the state line names the SELECTED playlist's playing song + its state.
  const state = page.locator('[data-testid="karaoke-now-playing"]');
  await expect(state).toContainText("Never Gonna Give You Up");
  await expect(state).toContainText("hotové");

  // The global done/pending counter shows in the mixer title.
  await expect(mixer.locator(".mixer-title")).toContainText("5");
});

test("a stem preset PATCHes the faders and the mock records them (#186)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-live");
  await expect(mixer).toBeVisible({ timeout: 10000 });

  const patch = page.waitForRequest(
    (req) => req.url().includes("/api/v1/mix") && req.method() === "PATCH",
  );
  await mixer.getByTestId("mixer-preset-vocals_only").click();
  await patch;
  const last = await (await page.request.get("/__mock/mix-last")).json();
  expect(last.vokaly).toBeCloseTo(1, 5);
  expect(last.podklad).toBeCloseTo(0, 5);
  await expect(mixer.getByTestId("mixer-preset-vocals_only")).toHaveClass(
    /active/,
  );
});

test("the Karaoke preset lowers the vokaly fader below full (#186)", async ({
  page,
}) => {
  await page.goto("/");
  const mixer = page.locator(".mixer.mixer-live");
  await expect(mixer).toBeVisible({ timeout: 10000 });

  const patch = page.waitForRequest(
    (req) => req.url().includes("/api/v1/mix") && req.method() === "PATCH",
  );
  await mixer.getByTestId("mixer-preset-karaoke_low").click();
  await patch;
  const last = await (await page.request.get("/__mock/mix-last")).json();
  // Karaoke snapshot = (0.3, 1, ·): vocals down, instrumental full.
  expect(last.vokaly).toBeCloseTo(0.3, 5);
  expect(last.podklad).toBeCloseTo(1, 5);
  await expect(mixer.getByTestId("mixer-preset-karaoke_low")).toHaveClass(
    /active/,
  );
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
  const mixer = page.locator(".mixer.mixer-live");
  await expect(mixer).toBeVisible({ timeout: 10000 });
  await expect(
    page.locator('[data-testid="karaoke-now-playing"]'),
  ).toContainText("vo fronte (3.)");
  await expect(mixer).toHaveClass(/mixer-locked/);
  await expect(page.getByTestId("mix-enqueue")).toBeHidden();
});
