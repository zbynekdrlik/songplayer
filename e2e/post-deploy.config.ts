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
  testMatch: "post-deploy.spec.ts",
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
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
