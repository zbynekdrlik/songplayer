---
paths:
  - "crates/sp-server/src/peer/**"
  - "crates/sp-core/src/config.rs"
---

# The node exchange (#229): serve what you have, ask peers first

Every SongPlayer node (SNV, PP) is to serve what it has processed and ask its
peers before a heavy job, so no node redoes what another already did.

Spec: `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`.
Plan: `docs/superpowers/plans/2026-10-06-pp-node-exchange.md`.

Lane 1 ships the settings, their validation and the status route below.
Lane 2 ships the exchange's vocabulary: kinds, versions, wire types, the job
board. Lanes 3–9 (own catalog, peer API, client, ask-first hooks, PP deploy)
extend this file; nothing of them exists yet.

## Settings (`peer::config::NodeConfig`, read live — no restart)

- `node_name` (`snv`, `pp`): 1–32 of `a-z`, `0-9`, `-`, trimmed. Empty = the
  exchange is OFF.
- `peer_api_key`: at least 32 characters, trimmed. The key THIS node's peer
  API will accept; empty = not serving.
- `peers`: a JSON list `[{name, base_url, key, cf_client_id?,
  cf_client_secret?}]`, the peers asked first. Each peer:
  - `name`: a node name (as above), not this node's own name, no peer twice;
  - `base_url`: `http(s)://host[/path]`, no query, no fragment, no
    whitespace, no `user:password@` before the host (it would show in clear:
    only the key and the Cloudflare secret are masked);
  - `key`: the peer's `peer_api_key`, at least 32 characters;
  - `cf_client_id` + `cf_client_secret`: a Cloudflare Access service token,
    both or neither.
- `peer_transfers_paused` = `"true"` (exactly, trimmed): no new transfer
  either way (`sp_core::config::peer_transfers_paused`). Lane 1 only reports
  it.
- `peer_serve_max_mbps` (1..=10000, default 20): what this node sends, parsed
  by `sp_core::config::peer_serve_max_mbps`. Nothing reads it yet.
- `NodeConfig::load` reads the three exchange settings from the database on
  each use, so a saved change applies at once. A config that does not hold
  is an `Err` with the reason; the reason names settings, peers and
  positions, never a secret. `NodeConfig`'s and `PeerConfig`'s `Debug` show
  the mask.
- Secrets: the settings API masks `peer_api_key` and each peer's `key` /
  `cf_client_secret`, keeps them on a masked PATCH, and refuses (400,
  nothing written) a PATCH whose exchange setting does not hold — see
  `settings-secrets.md`.
- A masked peer secret stays where it was stored for
  (`peer::config::unmask_peers`): a masked `key` only with the stored
  `base_url`, a masked `cf_client_secret` only with the stored `base_url`
  and `cf_client_id`; a peer re-pointed at another host sends its secrets
  again in clear, else 400. The settings PATCH has no login, so this is
  what stops a LAN page from routing the stored key and Cloudflare token to
  its own host once the peer client (lane 5) exists.
- Set them only through the secret channel: `airuleset.py secret exec <NAME>
  -- python3 <script>`, where the script PATCHes `/api/v1/settings` from the
  env var (the `lyrics-eval-backends.md` recipe). Never in chat, on a command
  line or in a commit.

## `GET /api/v1/exchange/status` (`peer::lan`, LAN API, no peer key)

- Answers `{node_name, serving, transfers_paused, config_error, peers:
  [{name, base_url, has_key, cf_access}]}` — never a key or a Cloudflare
  secret. `serving` = a node name and a key; `cf_access` = a Cloudflare
  Access token is configured for that peer.
- A config that does not hold reads as the exchange OFF (no node, not
  serving, no peer) plus `config_error` = the reason.
- A failed read of `peer_transfers_paused` is WARNed and reads as not paused.
- Later lanes ADD fields to `ExchangeStatus` / `PeerStatus`.

## Wiring

- `peer::Exchange { pool, cache_dir, board }` (pub fields; `cache_dir` has
  no reader until lane 3; `board` = the running jobs, below), built once by
  `lib.rs` right after `AppState`: `Exchange::new(pool, cache_dir) ->
  Arc<Exchange>` (it builds the board).
- `peer::router(exchange)` holds every exchange route; `lib.rs` merges it into
  the app's router at "11. Axum HTTP server". `merge` is valid because only
  `api::router` carries a fallback (the SPA): axum panics when merging two
  routers that both have one, so an exchange router never gets a fallback.
- Tests: `peer/lan_tests.rs` (the route through `peer::router` + `oneshot`
  over a migrated in-memory database; the merge exactly as `lib.rs` does it,
  `api::router` with a dist dir merged with `peer::router`, still serving
  the SPA), `peer/config_tests.rs`, `api/settings_tests.rs`. On the box:
  `e2e/post-deploy-settings-secrets.spec.ts` reads the status read-only.

## Kinds, versions, jobs (`peer::kind`, `peer::wire`, `peer::board`)

- Kinds (`kind::ArtifactKind`, serde snake_case): `video`, `audio`,
  `stem_vocals`, `stem_instrumental`, `lyrics`, `metadata`; `parse` reads a
  URL segment (exact, never `unknown`). An unknown kind (a newer peer's
  `dub`) reads as `Unknown`; `Catalog::sanitized` drops it with any artifact
  whose YouTube id or sha256 (64 lowercase hex) is malformed. Node names in
  a peer's catalog follow `peer::config::valid_name`:
  - a job entry whose `node` is not one is dropped;
  - separately, the catalog's own `node` reads as `""` when it is not one.

  A peer is named by its CONFIGURED name (`PeerConfig::name`), never by the
  `node` it sends. A time (`updated_at`, `started_at`) is rewritten in the
  canonical form (`wire::checked_time` = `ms_to_rfc3339` of the parsed
  instant: UTC, milliseconds, `Z`), never kept as the peer's text; one that
  is not an RFC 3339 time reads as `None` (the entry stays). Times are
  information only, never a decision's input.
- A node takes a peer's artifact only at ITS OWN current format
  (`kind::acceptable`): `MEDIA_VERSION` (video/audio), `STEMS_VERSION`,
  `LYRICS_PIPELINE_VERSION` (equality — a dev peer's newer lyrics are not
  taken). Bump `MEDIA_VERSION` / `STEMS_VERSION` in the same change that
  alters that output.
- The metadata version says who named the title (`kind::metadata_version`):
  `manual` = 2 (an operator); the chain's provider label `gemini` (the
  Claude provider writes it too) with `gemini_failed = 0` = 1; everything
  else = 0 (a parser): `regex` — also the one written with `gemini_failed =
  0` when no provider is configured (`metadata::fallback_from_title`) — no
  source, a label this node does not know, a row in the repair queue. A
  version ≥ 1 is taken. The plan's code ranked every non-manual
  `gemini_failed = 0` row as a provider's, which would have advertised a
  no-provider regex guess as a provider title (#229 comment 6030334867). A
  new `sp_core::metadata::MetadataSource` variant must be ranked here: the
  exhaustive `rank` match in
  `kind_tests.rs::every_metadata_source_label_is_ranked` stops compiling
  until it has an arm there; add it to that test's list and to
  `metadata_version` too (the list is not checked for completeness).
- Jobs (`kind::Job`): Download (needs video+audio, makes
  video+audio+metadata), Lyrics, Stems (needs and makes both stems).
- A catalog's job entry is `wire::CatalogJob { youtube_id, kind, node, state,
  started_at }`, one per kind the job makes. `state` = `running` | `queued`:
  the catalog lists a peer's QUEUED jobs too (the plan's decisions, lanes
  2/3), and a node waits for either. `started_at` is set only for a running
  job (`null` for a queued one). An unknown state reads as `Unknown` and
  `sanitized` drops the entry. `Catalog::announces(id, kinds)` = a running
  OR queued job (lane 7's Wait). The plan's text calls these `RunningJob` /
  `Catalog::runs`: the code names them `CatalogJob` / `announces`.
- A RUNNING job is announced by the in-memory `board::JobBoard` while its
  `#[must_use]` `JobGuard` lives (`Exchange::announce(youtube_id, job)`); a
  crash takes the announcements along, never a DB row. A second guard of
  the same job keeps the first one's start; the last guard to drop ends it.
  `JobBoard::snapshot(node)` = the running entries, sorted by YouTube id,
  then by the kind's WIRE NAME (`stem_instrumental` before `stem_vocals`,
  not the enum's order). QUEUED entries are not on the board: lane 3's catalog reads
  them from the rows. A download in progress is still a queued row
  (`normalized = 0`) AND on the board, so lane 3 lists an `(id, kind)` the
  board holds as running only and skips its queued entry (one entry per
  `(id, kind)`).
- OPEN for lane 7's design (the main decides it; #229 comment 6030598502
  item 7 and its correction): once SNV lists PP as a peer too (phase 2),
  two nodes with the same song queued would each wait on the other's queued
  entry for the full 2 h, then both process it — the double work the queued
  entries exist to avoid. A tie-break must keep phase 1 as decided: PP
  waits for SNV's queued jobs (ROZHODNUTÉ 6022851957 point 2). A rule keyed
  on the names needs both nodes to see the same pair, i.e. each node's
  configured name for a peer equals that peer's own `node_name`; nothing
  checks that yet (lane 7 could compare `PeerRead.peer` with the sanitized
  `Catalog.node` and WARN on a mismatch). The wire's `state` tells queued
  from running.
- `wire::PeerMetadata::to_bytes` = the metadata artifact's canonical bytes
  (serde field order; the catalog's metadata sha256 is over them). Times:
  `now_ms`, `ms_to_rfc3339` (`2026-10-06T16:00:00.123Z`; out of chrono's
  range = `""`), `rfc3339_to_ms` (any offset, trimmed).
- Tests: `peer/kind_tests.rs`, `peer/wire_tests.rs` (a newer peer's catalog:
  Review Focus 3), `peer/board_tests.rs`. A sha256 in a test is built
  (`"0123456789abcdef".repeat(4)`): the staging hook refuses a 40+ character
  hex literal.
