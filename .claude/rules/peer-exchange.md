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
Lanes 2–9 (catalog, peer API, client, ask-first hooks, PP deploy) extend this
file; nothing of them exists yet.

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

- `peer::Exchange { pool, cache_dir }` (pub fields; `cache_dir` has no reader
  until lane 3), built once by `lib.rs` right after `AppState`:
  `Exchange::new(pool, cache_dir) -> Arc<Exchange>`.
- `peer::router(exchange)` holds every exchange route; `lib.rs` merges it into
  the app's router at "11. Axum HTTP server". `merge` is valid because only
  `api::router` carries a fallback (the SPA): axum panics when merging two
  routers that both have one, so an exchange router never gets a fallback.
- Tests: `peer/lan_tests.rs` (the route through `peer::router` + `oneshot`
  over a migrated in-memory database), `peer/config_tests.rs`,
  `api/settings_tests.rs`.
