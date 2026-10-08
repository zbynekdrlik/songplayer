---
paths:
  - "e2e/post-deploy*.spec.ts"
  - "e2e/program-state*.ts"
  - "e2e/box-api.ts"
---

# Post-deploy specs: SongPlayer's program can be ANY sp-* scene (#184)

A program-state spec runs on the live box and never switches the program to set
itself up (other post-deploy specs press `sp-*` scenes through SongPlayer's
facade and restore its program in `afterAll`; the A/V gate parks cg OBS on its
probe scene). After an event the operator leaves the program on whatever scene
the event ended with. On 26.9.2026 that was `sp-dabing`, so the Dabing output was ON
program. Two specs had hard-coded "Dabing is always off program" and "the
auto-selected dashboard card is playing", and both went red for a non-product
reason.

Rules for every post-deploy spec:

- **Read the program state, never assume it.** Use
  `readProgramState(request)` from `e2e/program-state.ts`. It returns
  `activeScene`, `activePlaylistIds`, `dabingOnProgram` (by `kind == "dabing"`)
  and `regularOnProgram`. Assert the behaviour that matches each branch. Never
  use `test.skip` on a precondition (skips are banned).
- **Badge vs toggle (#201).** `player-playpause` follows the pipeline's own
  `transport`, so it reads `⏸ Pauza` while the dub decodes, on OR off program.
  `player-program-badge` and the health row `state` follow the program
  (#225: the badge reads the playlist's WS state, the same signal as the
  state label; the health row is the backend check):
  - on program: `state == "Playing"` and "● Na programe";
  - off program while decoding: `Paused` (the ndi_health reconcile) or
    `WaitingForScene`, and "○ Mimo programu".
  - Right after `page.goto` the Player reads "Načítavam…" / "—" / "◌ —"
    until the WS replay lands: prove "a song arrived" with
    `not.toHaveText(NOT_A_SONG)` (`post-deploy.spec.ts`), never
    `not.toHaveText("Nič nehrá")` alone.
- **Select a dashboard card explicitly.** Click
  `playlist-picker-item[data-playlist-id=<id>]` and check that the card's
  `.playlist-id` equals the playlist's NDI name. Never take "the first
  `.playlist-card`". Its `preview-start` exists only while THAT pipeline
  decodes, and a paused on-program Dabing output has none.
- **Box lookups live in `e2e/box-api.ts`** (`readyDub(request, preferVideoId?)`
  and `healthRow(request, pid)`). Do not re-inline `/api/v1/dabing` or
  `/api/v1/ndi/health` parsing in a spec.
- **`frames_submitted_last_5s > 0` is NOT proof of decoding.** Genlock pacing
  submits ~150 frames per 5 s on idle outputs too (live read, 26.9.2026). The
  honest "it decodes" signal is health `transport == "Playing"`, or the Player
  toggle.
- **A Dabing test that plays the dub while `sp-dabing` is on program puts it on
  the wall.** That is accepted (the design on #184). Pause what the test started
  in `finally`/`afterAll`.
- **Side effect of running Playwright locally.** `npx playwright test
  --config post-deploy.config.ts --list` rewrites the committed
  `e2e/post-deploy-report/index.html` through the html reporter. Pass
  `--reporter=list`, or `git checkout` that file before committing.
- **Transpile-check a spec edit without the box** (a worktree has no
  `e2e/node_modules`): symlink the main checkout's
  (`ln -s <repo>/e2e/node_modules e2e/node_modules` inside the worktree's
  `e2e/`), run `./node_modules/.bin/playwright test --config=post-deploy.config.ts
  --list --reporter=list` (a syntax error fails the listing; the mock-suite
  unit specs such as `obs-scene-wait.spec.ts` can even run). A worktree
  worker cannot delete the symlink afterwards (the worktree guard resolves
  it into the main checkout and refuses); `.gitignore` ignores
  `e2e/node_modules` without a trailing slash, so the link is never
  committed and goes with the worktree. Or run `npm ci` in the worktree's
  `e2e/` (a real, ignored directory; it needs no cleanup). Playwright only
  strips types, so for a STRICT type check install `typescript` and
  `@types/node` into a scratch dir and run its `tsc --noEmit --strict
  --esModuleInterop --skipLibCheck --target es2022 --module esnext
  --moduleResolution bundler --typeRoots <scratch>/node_modules/@types
  --types node <files>` from `e2e/` (#221 dev.18). The known noise is the
  untyped `ws` in `obs-driver-protocol.spec.ts`.
- **A post-deploy check's decision logic is a pure helper with a mock-suite
  unit spec** (`e2e/cache-layout.ts` for the FLAC layout, #136;
  `av-sync-gate.ts`, `obs-scene-wait.ts`). The post-deploy spec only reads
  the box and calls it, so the rule is tested in CI without a box. On the
  Tier-0 box a pure helper (interfaces / type annotations only, no enums)
  also runs with plain node and no install: a scratch `check.mts` that
  imports the helper by its absolute `.ts` path and mirrors the spec's cases
  with `node:assert/strict`, run as `node --experimental-strip-types
  check.mts`.
- **`expect.poll` does NOT retry a generator that throws** (#144, Playwright
  1.59: `pollMatcher` awaits `poll.generator()` outside its own try). A
  readiness poll whose `request.get` hits ECONNREFUSED after a restart, or its
  own request timeout, fails the test at once instead of polling: wrap the
  body in `try { … } catch { return false; }` and bound each read
  (`request.get(url, { timeout: 10_000 })`), as `post-deploy-g35t.spec.ts`
  does.
- **#221 L3: the scene driver is SongPlayer's facade** (`FACADE_WS_URL`,
  :4456). **#221 L4b:** `/api/v1/status.active_scene` /
  `active_playlist_ids` are SongPlayer's own program (the resolver, and the
  on-air set = SP-program's playlist alone since B4 step 6: none for "OBS
  manuál"). The playback authority applies a switch a moment after the
  facade answers it: wait for the engine to reach the scene
  (`program-state.ts::waitEngineActiveScene`, the A/V gate's `length === 1`
  poll), never read it once.
- **#221: a receiver is checked on `SP-program`, the only NDI sender**
  (lane 3 retired the per-playlist outputs): poll `GET /api/v1/program`
  through `ndi-health-gate.ts::programReceiverVerdict` (a source on program,
  `health.connections > 0`, `degraded_reason` null), as `post-deploy.spec.ts`
  and `post-deploy-dabing.spec.ts` do. `/api/v1/ndi/health` rows have no
  receiver field any more.

## A failed post-deploy test's trace is a PUBLIC artifact (#229)

`post-deploy.config.ts` keeps a trace on failure, and CI uploads the report
(7 days, public repo). A trace records every response the spec read,
including `request` fixture API calls. Before #229 masked the settings API,
`post-deploy-av-sync`'s trace (it GETs `/api/v1/settings` for `cache_dir`)
put 8 clear credentials into run 37423917199's artifact. A spec that reads a
secret-bearing endpoint sets `test.use({ trace: "off" })` and asserts
without printing a value: a boolean `toBe(true)` with a message that names
the key (`post-deploy-settings-masked.spec.ts`).
