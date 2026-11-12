import { test, expect } from "@playwright/test";

// #265 — `/blok`: block a song on this node from a phone in the room. The
// song on program now with "Zablokovať v <uzol>", a search over the node's
// songs by the title the wall shows, the blocked list with "Odblokovať".
// Runs against the mock at a phone's viewport; the mock answers the three
// routes with the server's statuses (`api/blocked.rs`).

test.use({ viewport: { width: 390, height: 844 } });

const ALLOWED_CONSOLE = [
  /WebSocket connection/, // WS reconnect messages are expected
  /favicon/, // favicon not served by mock
  /wasm.*instantiate/, // WASM instantiation warnings in test env
  /module specifier/, // module resolution in test env
  /integrity.*attribute.*ignored/, // Chrome SRI preload warning (crbug.com/981419)
];

let consoleMessages: string[] = [];

test.beforeEach(async ({ page, request }) => {
  consoleMessages = [];
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  await request.post("/__mock/blocked-reset");
  await request.post("/__mock/settings-reset");
  await request.post("/__mock/ws-replay", { data: {} });
});

test.afterEach(async ({ request }) => {
  await request.post("/__mock/blocked-reset");
  await request.post("/__mock/settings-reset");
  await request.post("/__mock/ws-replay", { data: {} });
  const real = consoleMessages.filter((m) => !ALLOWED_CONSOLE.some((r) => r.test(m)));
  expect(real).toEqual([]);
});

test("block the song on program, search and block another, unblock one", async ({
  page,
  request,
}) => {
  // The node's name on the button.
  const patch = await request.patch("/api/v1/settings", { data: { node_name: "PP" } });
  expect(patch.status()).toBe(204);

  await page.goto("/blok");
  await expect(page.getByTestId("block-page")).toBeVisible({ timeout: 10_000 });
  const list = page.getByTestId("block-list");
  await expect(list).toHaveAttribute("data-loaded", "true");
  await expect(list).toContainText("Žiadna pieseň nie je zablokovaná.");

  // The song on program (the mock's playlist 1, video 1): the wall's title.
  const current = page.getByTestId("block-current");
  await expect(current.getByTestId("block-current-title")).toHaveText(
    "Never Gonna Give You Up - Rick Astley",
    { timeout: 10_000 },
  );
  const currentBtn = page.getByTestId("block-current-btn");
  await expect(currentBtn).toHaveText("Zablokovať v PP");
  await expect(currentBtn).toBeEnabled();

  // Block it: one POST, 201, the list names it, the button says so.
  const blockPost = page.waitForResponse(
    (r) => r.url().endsWith("/api/v1/blocked") && r.request().method() === "POST",
  );
  await currentBtn.click();
  const answered = await blockPost;
  expect(answered.status()).toBe(201);
  expect(answered.request().postDataJSON()).toEqual({ youtube_id: "dQw4w9WgXcQ" });
  await expect(list.locator("li[data-youtube-id='dQw4w9WgXcQ']")).toHaveCount(1);
  await expect(currentBtn).toHaveText("Už je zablokovaná");
  await expect(currentBtn).toBeDisabled();

  // Search another by the title the wall shows (diacritics and case ignored).
  const search = page.getByTestId("block-search");
  await search.fill("TRADITIONAL");
  const results = page.getByTestId("block-result");
  await expect(results).toHaveCount(1);
  await expect(results.first()).toHaveAttribute("data-youtube-id", "abc123");
  await results.first().getByRole("button", { name: "Zablokovať" }).click();
  await expect(list.locator("li[data-youtube-id='abc123']")).toHaveCount(1);
  await expect(results.first().getByRole("button")).toHaveText("Zablokovaná");
  await expect(results.first().getByRole("button")).toBeDisabled();
  // The server holds both, the newest first.
  const both = await (await request.get("/api/v1/blocked")).json();
  expect(both.map((b: { youtube_id: string }) => b.youtube_id)).toEqual([
    "abc123",
    "dQw4w9WgXcQ",
  ]);

  // An empty search lists nothing.
  await search.fill("");
  await expect(results).toHaveCount(0);

  // Unblock the song on program: 204, the list keeps only the other.
  const del = page.waitForResponse(
    (r) => r.url().endsWith("/api/v1/blocked/dQw4w9WgXcQ") && r.request().method() === "DELETE",
  );
  await list
    .locator("li[data-youtube-id='dQw4w9WgXcQ']")
    .getByTestId("block-unblock")
    .click();
  expect((await del).status()).toBe(204);
  await expect(list.locator("li[data-youtube-id='dQw4w9WgXcQ']")).toHaveCount(0);
  await expect(list.locator("li[data-youtube-id='abc123']")).toHaveCount(1);
  await expect(currentBtn).toHaveText("Zablokovať v PP");
  await expect(currentBtn).toBeEnabled();

  // The page fits a phone: no horizontal scroll.
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  expect(overflow).toBeLessThanOrEqual(0);
});

test("a node with no name says 'tomto uzle'; the routes answer the server's statuses", async ({
  page,
  request,
}) => {
  await page.goto("/blok");
  await expect(page.getByTestId("block-current-btn")).toHaveText("Zablokovať v tomto uzle", {
    timeout: 10_000,
  });
  // 404 for a song the node does not have, 404 for an unblock of one not blocked.
  expect((await request.post("/api/v1/blocked", { data: { youtube_id: "nope" } })).status()).toBe(
    404,
  );
  expect((await request.delete("/api/v1/blocked/nope")).status()).toBe(404);
  // Idempotent: a second block keeps the first time.
  const first = await request.post("/api/v1/blocked", { data: { youtube_id: "abc123" } });
  expect(first.status()).toBe(201);
  const again = await request.post("/api/v1/blocked", { data: { youtube_id: "abc123" } });
  expect(again.status()).toBe(201);
  expect(await again.json()).toEqual(await first.json());
});

// #225's rule on this page: nothing is claimed before it was told. Every
// `data-state` the area takes is recorded from the first DOM change (a
// MutationObserver from an init script), so a one-render "nothing" flash is
// caught, not only what an `expect` happens to poll. Held back: the WS
// replay, then the playlist list the page reads itself.
async function recordAreaStates(page: import("@playwright/test").Page) {
  await page.addInitScript(() => {
    const seen: string[] = [];
    (window as unknown as { spAreaStates: string[] }).spAreaStates = seen;
    const note = (state: string | null | undefined) => {
      if (state && seen[seen.length - 1] !== state) seen.push(state);
    };
    // Each record's OLD value first, then the value now: two writes inside
    // one task, coalesced into one callback, still leave both states.
    new MutationObserver((records) => {
      for (const r of records) {
        const el = r.target as Element;
        if (r.type === "attributes" && el.getAttribute?.("data-testid") === "block-current") {
          note(r.oldValue);
        }
      }
      note(document.querySelector('[data-testid="block-current"]')?.getAttribute("data-state"));
    }).observe(document, {
      subtree: true,
      childList: true,
      attributes: true,
      attributeOldValue: true,
      attributeFilter: ["data-state"],
    });
  });
}

async function areaStates(page: import("@playwright/test").Page): Promise<string[]> {
  return page.evaluate(() => (window as unknown as { spAreaStates: string[] }).spAreaStates);
}

test("the song on program reads 'Načítavam…' until the program state is told", async ({
  page,
  request,
}) => {
  await recordAreaStates(page);
  await request.post("/__mock/ws-replay", { data: { delay_ms: 3000 } });
  await page.goto("/blok");
  const current = page.getByTestId("block-current");
  await expect(current).toContainText("Načítavam…", { timeout: 10_000 });
  await expect(current).toHaveAttribute("data-state", "song", { timeout: 10_000 });
  await expect(page.getByTestId("block-current-title")).toHaveText(
    "Never Gonna Give You Up - Rick Astley",
  );
  expect(await areaStates(page)).toEqual(["loading", "song"]);
});

test("with the playlist list held back the area never claims 'nothing'", async ({
  page,
  request,
}) => {
  await recordAreaStates(page);
  // The playlist list lands 1 s late and the WS replay (every state) 3 s
  // late: with no list read yet the page must not take "no playlist" for
  // "every state known, nothing plays".
  await request.post("/__mock/ws-replay", { data: { delay_ms: 3000 } });
  await page.route("**/api/v1/playlists", async (route) => {
    await new Promise((r) => setTimeout(r, 1000));
    await route.continue();
  });
  await page.goto("/blok");
  const current = page.getByTestId("block-current");
  await expect(current).toHaveAttribute("data-state", "song", { timeout: 10_000 });
  expect(await areaStates(page)).toEqual(["loading", "song"]);
  // The button is enabled once the song lists name the song's id.
  await expect(page.getByTestId("block-current-btn")).toBeEnabled({ timeout: 10_000 });
  await page.unroute("**/api/v1/playlists");
});
