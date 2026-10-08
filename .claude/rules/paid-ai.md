---
paths:
  - "crates/sp-server/src/paid_ai*.rs"
  - "crates/sp-server/src/gemini_api*.rs"
  - "crates/sp-server/src/metadata/chain*.rs"
  - "crates/sp-server/src/metadata/manual*.rs"
  - "crates/sp-server/src/reprocess/**"
  - "crates/sp-server/src/peer/ask*.rs"
  - "crates/sp-server/src/peer/held_tests.rs"
  - "crates/sp-server/src/peer/lyrics.rs"
  - "crates/sp-server/src/peer/queued*.rs"
  - "crates/sp-server/src/lyrics/worker*.rs"
  - "crates/sp-server/src/dabing/worker*.rs"
  - "crates/sp-server/src/api/lyrics_g35t.rs"
  - "crates/sp-server/src/api/lyrics.rs"
  - "crates/sp-server/src/api/metadata*.rs"
  - "crates/sp-core/src/config.rs"
  - "crates/sp-core/src/health.rs"
  - "sp-ui/src/components/health_bar.rs"
  - "sp-ui/src/components/settings_form.rs"
  - "e2e/settings-paid-ai.spec.ts"
  - "e2e/post-deploy-pp.spec.ts"
---

# Paid AI: one switch per node (#229 item C)

The owner's ruling (8.10.2026), verbatim: "Celkovo v pp by nemalo
dochadzat k ziadnemu platenemu ai spracovaniu dokial to nepovolim".

## The switch

- `paid_ai_enabled` (`sp_core::config::SETTING_PAID_AI_ENABLED`, not a
  secret), read live at every call by `sp-server` `paid_ai::enabled`.
  - Unset or blank = ON, so a node that never set it (SNV) is unchanged.
  - `"true"` = ON. `"false"`, or any other value, = OFF.
  - A switch that cannot be read is OFF, with a WARN: the owner's money
    comes first.
- A settings PATCH takes only `true`, `false` or `""`, stored lowercase
  (`paid_ai::checked`); anything else refuses the whole PATCH (400).
- Nastavenia "Platené AI" → "Povoliť platené AI spracovanie" (a checkbox).
  A save sends the switch only when the checkbox differs from the value the
  page loaded (`sp_core::config::paid_ai_to_send`), so a tab opened before
  the switch changed elsewhere never turns paid AI back on (review round
  12; mock spec "a Nastavenia tab opened before paid AI went off …").
- `GET /api/v1/status` has three fields:
  - `paid_ai_enabled`;
  - `paid_ai_held`: while OFF, the kinds with a hold under 40 min old
    (`HELD_SHOWN`: held work is held again within `HELD_RECHECK`, a
    translation or a dub at its worker's next tick; finished work drops
    out);
  - `node_name`.
- The health bar shows "Uzol: <name>". While the switch is OFF it also
  shows "Platené AI: vypnuté" (amber), with the held kinds in Slovak as the
  tooltip (`sp_core::health::{node_label, paid_ai_label}`). It reads the
  status once per page load: after a save in Nastavenia the chip changes at
  the next load.

## The ONE switch: every paid call site asks `paid_ai`

Paid AI here means Gemini (metered, `gemini_api_key`) and Claude (through
CLIProxyAPI, a paid plan). There is no transport-level backstop (the
transports, `AiClient::chat` and `gemini_api::send_on_key`, have no
database; a process-wide flag would let one test's switch refuse another
test's calls): each call site asks the switch, and each path has its test.
Every path:

- **Lyrics job** (Gemini 3.5 Transcribe, Claude's clean-up, the Spotify
  resolution, the translation):
  - `Job::paid_ai` names it; `Exchange::may_run_here` is asked at EVERY
    "run here" of the exchange, before the run's own records (the wait's
    end, a stand-in, the forgotten origins, the announcement).
  - The run-here points are `ask`'s Local, no-peers and bad-settings paths
    (`Ask::Held`), and the hook's operator ask and "nothing newer"
    (`Exchange::local`). Also another audio, a stand-in past the bound, and
    a failed fetch (held with no bound, so no give-up WARN at every pick).
  - While OFF a peer's copy is still taken and a peer's job still waited
    for; everything else is held (`Exchange::hold`): `paid_ai::hold`, plus
    a `HELD_RECHECK` (30 min) defer with no attempt counted.
  - The catalog announces no lyrics queued while OFF (`peer::queued`).
  - The gate lives in the exchange, so it needs one: production wires it
    into the lyrics worker on every node (`lib.rs`, `with_peer`, even with
    no peer listed). A worker built without one (a unit test's harness,
    `peer` `None`) runs here unasked (review round 14).
- **Translation passes** and `translate_track` (Claude):
  `LyricsWorker::translation_allowed(youtube_id)`, asked once the pass
  picked a song and read its track, so only a song it would translate is
  held: a stale translation whose track cannot be read is only stamped
  forward, as while ON (review round 15,
  `a_stale_translation_with_no_track_is_stamped_forward_not_held_while_off`).
- **Metadata chain** (Claude, Gemini):
  - `ProviderChain::providers` is async and answers `None` while OFF. The
    production chain is `gated` on the node's DB (`provider_chain(&pool, …)`);
    a test's chain (`ProviderChain::new`, `provider_chain_at`) never is.
  - A download then takes the title parser's name (free), marked
    `gemini_failed` for the repair (`metadata::parser_while_paid_ai_off`).
  - The repair takes a peer's title, else `ReprocessOutcome::Held`: no
    provider, no backoff, no WARN.
  - The probe answers 409 with `paid_ai::OFF_REASON`.
- **Dub** (Gemini Live-Translate): `DubWorker::may_dub` is checked every
  tick before a job is picked. The jobs wait where they are: no attempt, no
  failure mark; only the job that would run now is held (none without one).
  The job's key is read before it is marked `synth` (`DubWorker::job_key`),
  so a job held there keeps its status too (review round 14).
- **Lyrics source probe** (`POST /api/v1/lyrics/probe-sources`, Claude on
  the YouTube description): the AI client is handed to it only while ON;
  OFF, the description probe reads "skipped".
- **Every Gemini key read** goes through `paid_ai::gemini_keys` (`None`
  while OFF): the lyrics tiers, the dub worker's first key, and the g35t
  probe (which answers `ok:false` with `OFF_REASON` and sends nothing).
  - The metadata chain still reads its keys once at startup (`lib.rs`); its
    switch is read per walk.
- A NEW paid provider reads its credential or decides its call through
  `paid_ai` too. Never add one that bypasses it.

## Calm while OFF

Held work counts no attempt and logs ONE INFO per kind and song
(`paid_ai::hold`; the holds are kept for the process' life, later holds
are DEBUG), never a WARN. A held lyrics pick logs only DEBUG in the worker
(its "worker: processing" INFO comes once the song runs here), and the
repair batch's row count is DEBUG.

**Switching off stops NEW paid work, not work already running.** The
switch is read where a job starts (a pick, a pass, a tick); what is already
running finishes: a lyrics song picked before the switch-off still makes
its gather calls (Claude's clean-up, the description, the Spotify
resolution) and its g35t transcription keys were read at the song's start;
a Live-Translate dub session already streaming keeps streaming to its end.
The next pick of each is held.

Known, kept: the switch is read again right before a call (the metadata
repair's `try_providers`, the lyrics tiers' keys). A switch-off landing
between the two reads, milliseconds apart, costs that one row a WARN and a
backoff stage (or one song a "no key" pass), never a paid call. The dub's
key read is the exception that mattered: a job switched off between
`may_dub` and its key read was failed as "gemini_api_key not set"; it is
held now (`DubWorker::job_key` reads the switch again on no key; review
round 13).

## Tests (0 calls while OFF, unchanged while ON)

- `paid_ai_tests.rs`: the switch, the keys, the PATCH, the log levels, and
  the status through the router.
- `peer/held_tests.rs`: the exchange rig. Held, nothing announced, the wait
  and the stand-in kept. A peer's lyrics are still taken, stems still run,
  and no lyrics are queued.
- `lyrics/worker_tests_paid_ai.rs`: a wiremock Claude through `process_next`
  and both passes.
- The metadata chain, the download title, the repair and the probe: each with
  a counting provider.
- `dabing/worker_tests.rs`: `no_dub_and_no_key_while_paid_ai_is_off`,
  `only_a_dub_job_that_would_run_is_held`,
  `a_dub_switched_off_after_its_pick_is_held_never_failed` (+ the guard
  `a_dub_with_no_key_is_deferred`).
- `api/lyrics_g35t.rs::probe_route_sends_nothing_while_paid_ai_is_off`,
  `api/lyrics_tests.rs::probe_sources_asks_no_claude_while_paid_ai_is_off`.
- The log captures go through `crate::test_log` (one `Captured` writer +
  `capturing(&cap)`, test-only). `capturing` installs a global no-op
  default once, so a capture is never the only registered dispatcher
  (review round 14, `rust-workspace.md`).
- Mock: `e2e/settings-paid-ai.spec.ts`. Box: PP's `post-deploy-pp.spec.ts`
  asserts the switch OFF and the chip.

## PP (MAIN SESSION OPS)

Set `paid_ai_enabled=false` at PP BEFORE the release that carries this
reaches it (`PATCH /api/v1/settings`; an older server stores the unknown
key). Otherwise PP's post-deploy subset fails by design. Then PP's lyrics
worker may be switched on again: it only takes SNV's copies. Nothing in
`deploy-pp.yml` checks the ordering before it stops PP: a pre-stop read
needs a running SongPlayer, and would block a deploy that restores a PP
whose SongPlayer is down.
