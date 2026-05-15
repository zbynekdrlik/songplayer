import { test, expect } from "@playwright/test";

// Regression for #94: the /live global Pause / Skip / Previous / Play / Mode
// buttons used to discard `Result::Err` from their POST helpers, so a failing
// playback endpoint produced a silent no-op button. These tests arm the
// mock-api `__mock/fail-mode` switch on a single endpoint at a time and
// assert that the `.live-setlist-error` row surfaces the error to the
// operator.
//
// 500-resource console errors are expected in this file (we flip endpoints
// into fail mode on purpose), so no shared zero-console-errors gate is
// imposed here.

async function setFailMode(
  request: import("@playwright/test").APIRequestContext,
  kind: string,
  enabled: boolean,
) {
  const resp = await request.post("/__mock/fail-mode", {
    data: { kind, enabled },
  });
  expect(resp.ok()).toBeTruthy();
}

test.describe("live setlist playback errors surface to operator", () => {
  test.afterEach(async ({ request }) => {
    // Always clear every fail-mode flag so a later test starts clean.
    for (const kind of ["play", "pause", "skip", "previous", "mode"]) {
      await setFailMode(request, kind, false);
    }
  });

  test("pause failure shows error in .live-setlist-error", async ({
    page,
    request,
  }) => {
    await setFailMode(request, "pause", true);

    await page.goto("/live");
    await expect(page.locator(".live-setlist-controls")).toBeVisible({
      timeout: 10000,
    });

    await page.getByRole("button", { name: "⏸" }).click();

    await expect(page.locator(".live-setlist-error")).toHaveText(/.+/, {
      timeout: 5000,
    });
  });

  test("skip failure shows error in .live-setlist-error", async ({
    page,
    request,
  }) => {
    await setFailMode(request, "skip", true);

    await page.goto("/live");
    await expect(page.locator(".live-setlist-controls")).toBeVisible({
      timeout: 10000,
    });

    await page.getByRole("button", { name: "⏭" }).click();

    await expect(page.locator(".live-setlist-error")).toHaveText(/.+/, {
      timeout: 5000,
    });
  });

  test("previous failure shows error in .live-setlist-error", async ({
    page,
    request,
  }) => {
    await setFailMode(request, "previous", true);

    await page.goto("/live");
    await expect(page.locator(".live-setlist-controls")).toBeVisible({
      timeout: 10000,
    });

    await page.getByRole("button", { name: "⏮" }).click();

    await expect(page.locator(".live-setlist-error")).toHaveText(/.+/, {
      timeout: 5000,
    });
  });

  test("global play (no resume) failure shows error", async ({
    page,
    request,
  }) => {
    await setFailMode(request, "play", true);

    await page.goto("/live");
    await expect(page.locator(".live-setlist-controls")).toBeVisible({
      timeout: 10000,
    });

    await page.getByRole("button", { name: "▶ Play" }).click();

    await expect(page.locator(".live-setlist-error")).toHaveText(/.+/, {
      timeout: 5000,
    });
  });

  test("mode-change failure shows error in .live-setlist-error", async ({
    page,
    request,
  }) => {
    await setFailMode(request, "mode", true);

    await page.goto("/live");
    await expect(page.locator(".live-setlist-controls")).toBeVisible({
      timeout: 10000,
    });

    await page.locator(".live-setlist-mode").selectOption("continuous");

    await expect(page.locator(".live-setlist-error")).toHaveText(/.+/, {
      timeout: 5000,
    });
  });
});
