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
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
