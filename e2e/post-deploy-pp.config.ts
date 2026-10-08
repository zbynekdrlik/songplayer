import { defineConfig } from "@playwright/test";

/**
 * #229: the post-deploy subset at the PP site, run by
 * .github/workflows/deploy-pp.yml after every main release:
 * `post-deploy-pp.spec.ts` (#229 item C: paid AI off at PP, asserted there) +
 * `post-deploy-max.spec.ts` (SP-program-MAX) +
 * `post-deploy-settings-masked.spec.ts` (every secret masked, also at PP) +
 * `post-deploy-audio-asio.spec.ts` (#233: DVS registered; the ASIO outputs
 * SP_ASIO_OUTPUTS_EXPECTED names run a clean minute).
 * The SNV suite is post-deploy.config.ts; it ignores post-deploy-pp*.
 */
export default defineConfig({
  testDir: ".",
  testMatch: [
    "**/post-deploy-pp.spec.ts",
    "**/post-deploy-max.spec.ts",
    "**/post-deploy-settings-masked.spec.ts",
    "**/post-deploy-audio-asio.spec.ts",
  ],
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
    // A failing PP test carries its evidence (console, network, DOM
    // snapshots), uploaded by deploy-pp.yml on failure.
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
