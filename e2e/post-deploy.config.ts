import { defineConfig } from "@playwright/test";

/**
 * Dedicated Playwright config for the post-deploy suite.
 *
 * Unlike `playwright.config.ts` (which spawns the local mock API server),
 * this config targets the real deployed SongPlayer on the machine where
 * it is running. There is no webServer, baseURL is taken from the
 * `SONGPLAYER_URL` environment variable.
 */
export default defineConfig({
  testDir: ".",
  testMatch: /post-deploy.*\.spec\.ts$/,
  timeout: 90_000,
  retries: 0,
  workers: 1,
  reporter: [
    ["list"],
    ["html", { outputFolder: "post-deploy-report", open: "never" }],
  ],
  use: {
    baseURL: process.env.SONGPLAYER_URL || "http://localhost:8920",
    headless: true,
    ignoreHTTPSErrors: true,
    // #170: a failing post-deploy test must carry evidence. The trace bundles
    // the browser console, network log and per-step DOM snapshots; the html
    // report embeds it and the raw zip lands in test-results/ (both uploaded
    // by ci.yml on failure). Without this a failure like test 16's 260ms WS
    // death had no console/trace to diagnose from.
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    // Bundled Chromium runs every post-deploy spec EXCEPT the two preview ones
    // (bundled Chromium lacks H.264/AAC). A project-level testIgnore REPLACES
    // the global testMatch's effect for this project, so it is the only filter
    // needed to drop the preview spec here.
    {
      name: "chromium",
      use: { browserName: "chromium" },
      testIgnore: ["**/post-deploy-preview.spec.ts", "**/post-deploy-owner-path.spec.ts"],
    },
    // The #178 live preview <video> must decode real H.264/AAC on the box.
    // Edge is always present on the Windows runner and carries the proprietary
    // codecs. Runs ONLY the two preview specs: post-deploy-preview.spec.ts and
    // the #184 round-G2 owner-path acceptance (post-deploy-owner-path.spec.ts —
    // it listens to the preview audio, so it needs the codecs too).
    {
      name: "edge",
      use: { browserName: "chromium", channel: "msedge" },
      testMatch: ["**/post-deploy-preview.spec.ts", "**/post-deploy-owner-path.spec.ts"],
    },
  ],
});
