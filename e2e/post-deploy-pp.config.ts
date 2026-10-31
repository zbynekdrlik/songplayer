import { defineConfig } from "@playwright/test";

/**
 * #229: the post-deploy subset at the PP site, run by
 * .github/workflows/deploy-pp.yml after every main release:
 * `post-deploy-pp.spec.ts` (#229 item C: paid AI off at PP, asserted there) +
 * `post-deploy-max.spec.ts` (SP-program-MAX) +
 * `post-deploy-settings-masked.spec.ts` (every secret masked, also at PP) +
 * `post-deploy-audio-asio.spec.ts` (#233: DVS registered; the ASIO outputs
 * SP_ASIO_OUTPUTS_EXPECTED names run a clean minute, and a child process
 * SongPlayer spawns disturbs none of them: the ASIO host process; #257: no
 * audio path records a loss in the audio fault ledger meanwhile) +
 * `post-deploy-environment.spec.ts` (#233: Windows LTSC or IoT Enterprise,
 * Defender real-time and every firewall profile off) +
 * `post-deploy-audio-faults-ui.spec.ts` (#257: the dashboard's audio
 * segment reads PP's own fault ledger, its panel lists every path) +
 * `post-deploy-wall.spec.ts` (#259: the LED wall report reads PP's box; it
 * fails only when SongPlayer cannot read, never on PP's own setup) +
 * `post-deploy-pp-preview.spec.ts` (#266: the on-program card's live preview
 * decodes video and audio at PP; it needs real H.264/AAC, so it runs in the
 * `edge` project below, on the Edge deploy-pp.yml makes sure of) +
 * `post-deploy-ui-usage.spec.ts` (#256: PP's own usage log answers its
 * summary, and the gate's automated session adds nothing to it).
 * The SNV suite is post-deploy.config.ts; it ignores post-deploy-pp*.
 */
export default defineConfig({
  testDir: ".",
  testMatch: [
    "**/post-deploy-pp.spec.ts",
    "**/post-deploy-max.spec.ts",
    "**/post-deploy-settings-masked.spec.ts",
    "**/post-deploy-audio-asio.spec.ts",
    "**/post-deploy-environment.spec.ts",
    "**/post-deploy-audio-faults-ui.spec.ts",
    "**/post-deploy-wall.spec.ts",
    "**/post-deploy-pp-preview.spec.ts",
    "**/post-deploy-ui-usage.spec.ts",
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
  projects: [
    // Bundled Chromium runs the subset EXCEPT the preview spec: it lacks
    // H.264/AAC. A project-level testIgnore/testMatch replaces the global one
    // for its project.
    {
      name: "chromium",
      use: { browserName: "chromium" },
      testIgnore: ["**/post-deploy-pp-preview.spec.ts"],
    },
    // #266: the live preview <video> decodes the box's real H.264/AAC in Edge
    // (msedge), as SNV's preview specs do (post-deploy.config.ts).
    {
      name: "edge",
      use: { browserName: "chromium", channel: "msedge" },
      testMatch: ["**/post-deploy-pp-preview.spec.ts"],
    },
  ],
});
