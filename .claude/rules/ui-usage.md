---
paths:
  - "crates/sp-core/src/ui_usage*.rs"
  - "crates/sp-server/src/ui_usage*.rs"
  - "crates/sp-server/src/api/ui_usage*.rs"
  - "crates/sp-server/src/db/models_ui_usage.rs"
  - "crates/sp-server/src/db/mod_tests_v42.rs"
  - "sp-ui/src/usage.rs"
  - "sp-ui/usage_env.js"
  - "sp-ui/src/components/ui_usage_line.rs"
  - "e2e/ui-usage*.ts"
  - "e2e/post-deploy-ui-usage.spec.ts"
---

# The dashboard's usage log (#256)

Every node (SNV, PP) records how its own dashboard is used: which control,
on which page, while a playlist was on air or not. It is the measured basis
the #248 redesign starts from (the owner rejected a re-sort of the same
layout: every control gets a class by how often it is used). Design record:
#256 comment 6226648888.

## The pieces

- `sp_core::ui_usage` holds every rule, pure and workspace-tested (sp-ui
  has no unit tests): the wire (`UsageBatch` / `UsageEvent` / `UsageKind`,
  `parse_batch`, `check_batch`, `BATCH_REFUSAL`), the client's rules
  (`excluded`, `control_name`, `UsageEvent::new` clips, `UsageBuffer`), the
  summary (`parse_days`, `window_start`, `UsageSummary::from_rows`) and
  Nastavenia's line (`settings_line`, `span_days`, the Slovak plural from
  `audio_faults::counted`).
- sp-ui `usage.rs` only wires the browser, installed once by `App` (whose
  root never unmounts: the listeners and the timer are `forget`-ed):
  - ONE capture-phase listener on `document` per kind (click, change,
    submit), so a new control is covered with no code of its own; a click
    is recorded BEFORE the control's own handler, so a tab click names the
    page it was made on;
  - the control = the nearest `data-testid` (`Element::closest`), else the
    target's own tag as `<tag>`: a control with no id shows up as a gap
    (the Nastavenia heading reads `<h1>`, the form's save `<button>` and its
    submit `<form>`);
  - one `page` event per change of the router's `page` signal (an `Effect`
    over its previous value, so a click on the tab already shown records
    none), the first run being the page the load opened; its control is
    empty;
  - the buffer goes out every 10 s when not empty (`gloo_net` POST, JSON),
    at once when it holds 200, and by `navigator.sendBeacon` (a string,
    so `text/plain`) on `visibilitychange` → hidden;
  - a failed send is dropped silently: no console line, never shown;
  - `navigator.webdriver` and `Date.now()` come from the module shim
    `sp-ui/usage_env.js` (web-sys binds no `webdriver`, and sp-ui has no
    `js-sys`): the `preview_player.js` mechanism.
- `api::ui_usage`: `POST /api/v1/ui-usage` reads the body as BYTES whatever
  its content type (axum's `Json` would answer a beacon's `text/plain` with
  415) through `parse_batch` (typed, never `serde_json::Value`); a refusal
  is a 400 with the fixed text and nothing written, a stored batch a 204.
  Each row gets `ProgramBus::on_air_now().source` raw (a playlist id, -1
  the NDI input, -2 Blank, NULL before the first selection).
- `db::models_ui_usage` (V42 `ui_usage` + `idx_ui_usage_ts`): one
  transaction per batch; the summary is ONE GROUP BY per (page, control,
  kind) with the count, `COUNT(DISTINCT date(ts_ms / 1000, 'unixepoch',
  'localtime'))` (the box's own calendar), first and last use, and the uses
  while a PLAYLIST was on air (`on_air_source NOT IN (-1, -2)`, NULL
  excluded: `program_on_air::wall_owner`'s rule, the ticket's "while a
  playlist was on air"); most used first.
- `crate::ui_usage::run` prunes rows older than 400 days (`cutoff_ms` =
  the summary's `days=400` window start): 5 min after the start, then
  daily, an hour later when #230's hold held it (`Job::UiUsagePrune`,
  Slovak "mazanie starého záznamu používania"); `PRUNE_BATCH` (5 000) rows
  per DELETE until a statement finds none (`prune_with` takes the batch so
  a test deletes in several statements).
- Nastavenia (`components/ui_usage_line.rs`, last on the page): one line
  from `GET /api/v1/ui-usage/summary?days=400` ("Záznam používania: 3
  udalosti za 2 dni", "… zatiaľ žiadna udalosť", "… nedostupný") and the
  link `súhrn (JSON)` to that summary.

## Who is never recorded

- Every Playwright session: `navigator.webdriver === true`. That covers the
  mock suite (every other spec) and the post-deploy specs on the boxes, so
  CI never writes into a node's statistics.
- A browser with the localStorage key `sp_no_usage` (any value), for other
  tools that drive a dashboard.
- Both are read ONCE at install; nothing is installed then (no listener, no
  timer, no `Effect`).

## Tests

- sp-core `ui_usage_tests.rs` (bounds at 200 / 201 events and 120 / 121
  characters — characters, not bytes —, the buffer's hand-back at the
  200th event, the days parse, the span counted up, the line's plurals);
  sp-server `api/ui_usage_tests.rs` (through the router: the source on air
  per batch, a `text/plain` beacon, a refusal writes nothing, the summary's
  counts, distinct days, first / last and on-air per control, its window,
  the 400s), `ui_usage_tests.rs` (the prune: the cutoff row kept, several
  statements, the daily pass), `db/mod_tests_v42.rs`.
- Distinct days in a test: put the stamps around NOON UTC (`(now / DAY - 3)
  * DAY + 12 h`), so `localtime` gives the same days in any zone the CI
  runner has.
- Mock E2E `e2e/ui-usage.spec.ts`: the positive tests first make the page
  read as a person's browser (`addInitScript` defining
  `navigator.webdriver` false); the hidden page is faked by redefining
  `document.visibilityState` / `hidden` and dispatching `visibilitychange`.
  The beacon test hides the page well inside the first 10 s (the timer has
  sent nothing yet); the full-flow test compares the events of all posts
  together (a slow run may split them between the timer and the beacon).
  The mock records each post with its content type (`GET /__mock/ui-usage`,
  reset `POST /__mock/ui-usage-reset`) and answers the summary from them
  (`on_air` always 0: the mock has no program source). Against an OLD dist
  (no usage module) four of its five tests fail and the exclusion test
  passes: run it on a new build before trusting the exclusion test.
- Post-deploy `e2e/post-deploy-ui-usage.spec.ts` (SNV, and PP's subset —
  `scripts/tests/test_deploy_pp_workflow.py` pins that list): the summary's
  shape (`ui-usage-gate.ts::summaryProblems`), a `days=0` refusal, and its
  OWN session (Prehľad ↔ Texty 4 times, hidden, past the flush) adds
  nothing: each of the three rows it would make must grow by less than 4
  (`recordedSession`), so an operator's own use of the box meanwhile never
  fails a deploy. It never drives a person's session on a box (that would
  write made-up uses into the node's statistics); "a real session is
  counted" is checked by hand after a deploy.
