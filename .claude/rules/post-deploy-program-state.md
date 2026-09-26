---
paths:
  - "e2e/post-deploy*.spec.ts"
  - "e2e/program-state*.ts"
  - "e2e/box-api.ts"
---

# Post-deploy specs: the OBS program can be ANY sp-* scene (#184)

The post-deploy suite runs on the live box and never switches OBS scenes to set
itself up. After an event the operator leaves the program on whatever scene the
event ended with. On 26.9.2026 that was `sp-dabing`, so the Dabing output was ON
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
  `player-program-badge` and the health row `state` follow the program:
  - on program: `state == "Playing"` and "● Na programe";
  - off program while decoding: `Paused` (the ndi_health reconcile) or
    `WaitingForScene`, and "○ Mimo programu".
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
