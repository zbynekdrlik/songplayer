---
paths:
  - "crates/sp-server/src/paid_ai*.rs"
  - "crates/sp-server/src/gemini_api*.rs"
  - "crates/sp-server/src/metadata/chain*.rs"
  - "crates/sp-server/src/metadata/manual*.rs"
  - "crates/sp-server/src/reprocess/**"
  - "crates/sp-server/src/peer/ask*.rs"
  - "crates/sp-server/src/peer/held_tests.rs"
  - "crates/sp-server/src/peer/queued*.rs"
  - "crates/sp-server/src/lyrics/worker*.rs"
  - "crates/sp-server/src/dabing/worker.rs"
  - "crates/sp-server/src/api/lyrics_g35t.rs"
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
- `GET /api/v1/status` has three fields:
  - `paid_ai_enabled`;
  - `paid_ai_held`: the kinds holding work while OFF;
  - `node_name`.
- The health bar shows "Uzol: <name>". While the switch is OFF it also
  shows "Platené AI: vypnuté" (amber), with the held kinds in Slovak as the
  tooltip (`sp_core::health::{node_label, paid_ai_label}`).

## The ONE gate: every paid call asks `paid_ai`

Paid AI here means Gemini (metered, `gemini_api_key`) and Claude (through
CLIProxyAPI, a paid plan). Every path:

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
- **Translation passes** and `translate_track` (Claude):
  `LyricsWorker::translation_allowed`.
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
  failure mark.
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
are DEBUG), never a WARN.

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
- `dabing/worker.rs::no_dub_and_no_key_while_paid_ai_is_off` and
  `api/lyrics_g35t.rs::probe_route_sends_nothing_while_paid_ai_is_off`.
- Mock: `e2e/settings-paid-ai.spec.ts`. Box: PP's `post-deploy-pp.spec.ts`
  asserts the switch OFF and the chip.

## PP (MAIN SESSION OPS)

Set `paid_ai_enabled=false` at PP BEFORE the release that carries this
reaches it (`PATCH /api/v1/settings`; an older server stores the unknown
key). Otherwise PP's post-deploy subset fails by design. Then PP's lyrics
worker may be switched on again: it only takes SNV's copies.
