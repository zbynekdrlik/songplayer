---
paths:
  - "crates/sp-core/src/ui_usage*.rs"
  - "crates/sp-core/src/query_param*.rs"
  - "crates/sp-server/src/ui_usage*.rs"
  - "crates/sp-server/src/daily_pass*.rs"
  - "crates/sp-server/src/api/ui_usage*.rs"
  - "crates/sp-server/src/db/models_ui_usage.rs"
  - "crates/sp-server/src/db/mod_tests_v42.rs"
  - "sp-ui/src/usage.rs"
  - "sp-ui/usage_env.js"
  - "sp-ui/src/components/ui_usage_line.rs"
  - "e2e/ui-usage*.ts"
  - "e2e/post-deploy-ui-usage.spec.ts"
  - "src-tauri/tauri.conf.json"
  - "src-tauri/src/tray.rs"
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
  (`excluded`, `control_name`, `UsageEvent::new` clips, `UsageBuffer`,
  `PageViews`), the summary (`parse_days`, `window_start`,
  `UsageSummary::from_rows`) and Nastavenia's line (`settings_line`,
  `span_days`, the Slovak plural from `audio_faults::counted`). `days` is
  read by `sp_core::query_param::whole_count`, the one whole-count parse
  the audio fault ledger's `hours` uses too.
- sp-ui `usage.rs` only wires the browser, installed once by `App` (whose
  root never unmounts: the listeners and the timer are `forget`-ed):
  - ONE capture-phase listener on `document` per kind (click, change,
    submit), so a new control is covered with no code of its own; a click
    is recorded BEFORE the control's own handler, so a tab click names the
    page it was made on;
  - the control = the nearest `data-testid` (`Element::closest`), else the
    target's own tag as `<tag>`: a control with no id shows up as a gap
    (the Nastavenia heading reads `<h1>`, the form's save `<button>` and its
    submit `<form>`). A click on a `<label>` that wraps an input also
    clicks the input: one action, two click rows (the label's id or tag,
    then the input's) — merge them when reading the summary;
  - one `page` event per change of the router's `page` signal (an `Effect`
    over its previous value, so a click on the tab already shown records
    none), the first run being the page the load opened; its control is
    empty. A view counts only for a page the operator can SEE, visible
    AND focused (`PageViews`): SongPlayer's own app window (Tauri,
    `tauri.conf.json` `visible: false`) loads the dashboard at every start
    and stays hidden until the tray shows it, so its load would add a
    made-up `/` view per restart (each deploy). A view shown while it
    cannot be seen is owed and recorded at the next `focus` (on `window`)
    or `visibilitychange` that finds it seen. Focus is required as well:
    WebView2 keeps its controller visible in a hidden Tauri window (`wry`
    `SetIsVisible(attributes.visible)`), so `document.hidden` alone may
    read false there. And the window is created UNFOCUSED
    (`tauri.conf.json` `"focus": false`): with Tauri's default `focus:
    true`, wry calls `controller.MoveFocus(…PROGRAMMATIC)` when it creates
    the webview, inside the hidden window, so the page could read focused.
    The tray's "Open", a click on the tray icon and a second launch call
    `show()` + `set_focus()`; wry passes the window's focus into the
    webview, whose `window` `focus` should bring the owed view in (read
    from the sources). Neither half is observed on a box yet: (a) a
    restart leaves the `/` page row as it was; (b) opening the window from
    the tray adds one `/` view within 10 s. Keep `"focus": false` on that
    window. Clicks need no gate (only a person clicks);
  - the buffer goes out every 10 s when not empty (`gloo_net` POST, JSON),
    at once when it holds 200, and by `navigator.sendBeacon` (a string,
    so `text/plain`) on `visibilitychange` → hidden;
  - a failed send is dropped silently, never shown. This module logs
    nothing (the browser itself may log a failed request, e.g. while
    SongPlayer restarts);
  - `navigator.webdriver` and `Date.now()` come from the module shim
    `sp-ui/usage_env.js` (web-sys binds no `webdriver`, and sp-ui has no
    `js-sys`): the `preview_player.js` mechanism.
- `api::ui_usage`: `POST /api/v1/ui-usage` reads the body as BYTES whatever
  its content type (axum's `Json` would answer a beacon's `text/plain` with
  415) through `parse_batch(body, now_ms)` (typed, never
  `serde_json::Value`; a negative stamp is refused; every stamp is capped
  at the server's clock: a use cannot come after its arrival, and a stamp
  years ahead would otherwise sit in every window and never be pruned); a
  refusal is a 400 with the fixed text and nothing written, a stored batch
  a 204. Each row gets `ProgramBus::on_air_now().source` raw (a playlist
  id, -1 the NDI input, -2 Blank, NULL before the first selection).
- `db::models_ui_usage` (V42 `ui_usage` + `idx_ui_usage_ts`): one
  transaction per batch; the summary is ONE GROUP BY over `from..=now` per
  (page, control, kind) with the count, `COUNT(DISTINCT date(ts_ms / 1000,
  'unixepoch', 'localtime'))` (the box's own calendar), first and last use,
  and the uses while a PLAYLIST was on air (`on_air_source NOT IN (-1,
  -2)`, NULL excluded: `program_on_air::wall_owner`'s rule, the ticket's
  "while a playlist was on air"); most used first.
- `crate::ui_usage::run` prunes rows older than 400 days (`cutoff_ms` =
  the summary's `days=400` window start) on the shared daily timer
  `crate::daily_pass` (the #260 retention's too: 5 min after the start,
  then daily, an hour later when #230's hold held it; `Job::UiUsagePrune`,
  Slovak "mazanie starého záznamu používania"); `PRUNE_BATCH` (5 000) rows
  per DELETE until a statement finds none, at most `MAX_STATEMENTS` (1 000)
  per pass (`prune_with` takes the batch so a test deletes in several
  statements; the bound keeps a mutant from spinning).
- Nastavenia (`components/ui_usage_line.rs`, last on the page): one line
  from `GET /api/v1/ui-usage/summary?days=400` ("Záznam používania: 3
  udalosti za 2 dni", "… zatiaľ žiadna udalosť", "… nedostupný") and the
  link `súhrn (JSON)` to that summary.
- No volume cap: the route is LAN-only and unauthenticated like every
  other write route of the API, a batch is bounded (200 events, 120
  characters, axum's 2 MiB body), and a refused batch is one WARN naming
  only its size.

## Who is never recorded

- Every Playwright session: `navigator.webdriver === true`. That covers the
  mock suite (every other spec) and the post-deploy specs on the boxes, so
  CI never writes into a node's statistics.
- A browser with the localStorage key `sp_no_usage` (any value), for other
  tools that drive a dashboard.
- Both are read ONCE at install; nothing is installed then (no listener, no
  timer, no `Effect`).
- SongPlayer's own hidden app window records no page view until it is
  shown and focused (above).
- Before a walk of the dashboard on a box in a person's browser (the
  owner's path: real Chrome through the `win-*` tools, where
  `navigator.webdriver` is false), set the key in that browser profile
  first (`localStorage.setItem("sp_no_usage", "1")` on the dashboard's
  origin): such a walk would otherwise be counted as operator use. The one
  deliberate "a real session is counted" check after a deploy is the
  exception.

## Tests

- sp-core `ui_usage_tests.rs` (bounds at 200 / 201 events and 120 / 121
  characters — characters, not bytes —, a stamp at 0 taken and at -1
  refused, a stamp ahead capped, the buffer's hand-back at the 200th
  event, `PageViews`, the days parse, the span counted up, the line's
  plurals), `query_param_tests.rs`; sp-server `api/ui_usage_tests.rs`
  (through the router: the source on air per batch, a `text/plain`
  beacon, a refusal writes nothing, a stamp ahead stored at the server's
  clock, the summary's counts, distinct days, first / last and on-air per
  control, its window and its end at now, the 400s), `ui_usage_tests.rs`
  (the prune: the cutoff row kept, several statements, the daily pass),
  `daily_pass_tests.rs`, `db/mod_tests_v42.rs`.
- Distinct days in a test: put every stamp of one day within the first
  milliseconds after 12:00:00.000 UTC (`(now / DAY - 3) * DAY + 12 h + k`).
  Time zones are whole quarter hours off UTC, so no local midnight falls
  inside such a block and `localtime` gives the same days at every fixed
  offset (a DST change on one of those days could still move one: Norfolk
  in April). A spread of hours around noon does NOT hold (UTC+10, −11:15
  split it).
- Mock E2E `e2e/ui-usage.spec.ts`: the positive tests first make the page
  read as a person's browser (`addInitScript` defining
  `navigator.webdriver` false); Playwright emulates focus, so every page
  reads visible and focused. A hidden page is faked by redefining
  `document.visibilityState` / `hidden` and dispatching `visibilitychange`;
  the unseen app window by also overriding `document.hasFocus` from the
  load (`spSeen`), and a visible page with no focus by overriding only
  `hasFocus` and dispatching only `window`'s `focus` (`spFocus`: the path
  the app window takes when WebView2 reads it visible; a listener on
  `document` would never hear it). The beacon test hides the page well inside the first
  10 s (the timer has sent nothing yet); the full-flow test compares the
  events of all posts together (a slow run may split them between the
  timer and the beacon). The exclusion test runs a person's page beside
  the webdriver and `sp_no_usage` pages as its control: hiding sends what
  a log holds at once, so only the person's session arrives — no wait for
  the 10 s flush. The `sp_no_usage` page needs a context of its OWN: pages
  of one context share their origin's localStorage, so its key would also
  silence the control. The mock records each post with its content type (`GET
  /__mock/ui-usage`, reset `POST /__mock/ui-usage-reset`) and answers the
  summary from them (`on_air` always 0: the mock has no program source;
  rows ordered by code point like SQLite). Against an OLD dist (no usage
  module) every test of it fails.
- Post-deploy `e2e/post-deploy-ui-usage.spec.ts` (SNV, and PP's subset —
  `scripts/tests/test_deploy_pp_workflow.py` pins that list): the summary's
  shape (`ui-usage-gate.ts::summaryProblems`), a `days=0` refusal, and its
  OWN session (Prehľad ↔ Texty 4 times, then hidden: a log installed in it
  would send everything at once) adds nothing: each of the three rows it
  would make must grow by less than 4 (`recordedSession`), so an
  operator's own use of the box meanwhile never fails a deploy. It never
  drives a person's session on a box (that would write made-up uses into
  the node's statistics): "a real session is counted", "SongPlayer's own
  hidden window adds no `/` view at a restart" and "opening that window
  from the tray adds one `/` view" are the post-deploy checks made by
  hand.
