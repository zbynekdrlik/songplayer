import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  // Glob match: every *.spec.ts under e2e/ runs. The previous explicit
  // allow-list silently dropped any spec file the author forgot to
  // register (caught on PR #100: live-playlist.spec.ts was missed and
  // CI ran 22 tests instead of 29). Glob means new spec files run by
  // default; opting OUT is an explicit decision.
  //
  // post-deploy*.spec.ts is excluded — it targets the real deployed
  // server and runs under post-deploy.config.ts, not against the
  // local mock-api.
  testMatch: ["**/*.spec.ts"],
  testIgnore: ["**/post-deploy*.spec.ts"],
  timeout: 30000,
  retries: 0,
  // Single worker: the mock-api process keeps in-memory state
  // (failModes, liveItems, reprocessResult) that multiple parallel
  // workers would stomp. The whole suite runs in ~10s so the speed
  // cost is negligible compared to the flake cost of cross-worker
  // 500s leaking into unrelated specs.
  workers: 1,
  use: {
    baseURL: "http://127.0.0.1:8920",
    headless: true,
  },
  projects: [
    { name: "chromium", use: { browserName: "chromium" } },
  ],
  reporter: [["html", { outputFolder: "playwright-report" }], ["list"]],
});
