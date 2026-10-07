---
paths:
  - "crates/sp-server/src/peer/**"
  - "crates/sp-core/src/config.rs"
  - "crates/sp-server/src/db/models_peer*.rs"
  - "crates/sp-server/src/db/mod_tests_v29.rs"
  - "e2e/post-deploy-peer-serving.spec.ts"
---

# The node exchange (#229): serve what you have, ask peers first

Every SongPlayer node (SNV, PP) is to serve what it has processed and ask its
peers before a heavy job, so no node redoes what another already did.

Spec: `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`.
Plan: `docs/superpowers/plans/2026-10-06-pp-node-exchange.md`.

Lane 1 ships the settings, their validation and the status route below.
Lane 2 ships the exchange's vocabulary: kinds, versions, wire types, the job
board. Lanes 3–5, one combined lane (ROZHODNUTÉ 6031694082), ship this
node's catalog with its sha256 cache, the peer API with SNV's serving gate,
and the peer client with the probe. Lanes 6–9 (PP deploy, the ask-first
hooks) are to extend this file; nothing of them exists yet: no worker asks a
peer, nothing outside the tests fetches.

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
  either way (`sp_core::config::peer_transfers_paused`, read through
  `Exchange::transfers_paused`, which WARNs a failed read and reads it as not
  paused): the peer API answers 503 on every route, `Exchange::fetch`
  refuses, the hasher stops. A transfer already running finishes.
- `peer_serve_max_mbps` (1..=10000, default 20): what this node sends, parsed
  by `sp_core::config::peer_serve_max_mbps`: the peer API's upload cap.
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
  its own host: the peer client (`peer::client`) sends them to `base_url`.
- Set them only through the secret channel: `airuleset.py secret exec <NAME>
  -- python3 <script>`, where the script PATCHes `/api/v1/settings` from the
  env var (the `lyrics-eval-backends.md` recipe). Never in chat, on a command
  line or in a commit.

## `GET /api/v1/exchange/status` (`peer::lan`, LAN API, no peer key)

- Answers `{node_name, serving, transfers_paused, config_error, peers:
  [{name, base_url, has_key, cf_access, last_read}], catalog: {files,
  listed, queued}, jobs}` — never a key or a Cloudflare secret. `serving` =
  a node name and a key (the peer API answers); `cf_access` = a Cloudflare
  Access token is configured for that peer; `last_read` = that peer's last
  catalog read since this process started (`client::LastRead`: ok, at,
  artifacts, jobs, latency_ms, error), `null` before one; `catalog` =
  `catalog::counts` (the files the rows name, how many are hashed so
  listed, the queued job entries listed), `null` when the rows cannot be
  read (WARNed); `jobs` = the board's running jobs.
- A config that does not hold reads as the exchange OFF (no node, not
  serving, no peer) plus `config_error` = the reason.
- `POST /api/v1/exchange/probe` (LAN, no key) reads every peer's catalog
  now, one after the other: `[{name, base_url, ok, artifacts, jobs,
  latency_ms, error}]`, 409 with the reason when the settings do not hold.
  It is the live gate of PP → SNV through Cloudflare (lane 6's PP
  post-deploy subset is to call it).
- Later lanes may ADD fields to `ExchangeStatus` / `PeerStatus`.

## Wiring

- `peer::Exchange { pool, cache_dir, board, client }` (`client` is
  `pub(crate)`; `cache_dir` holds the `{yt}_lyrics.json` the catalog names
  and the fetch parts dir `<cache>/peer/`; `board` = the jobs announced as
  running, below, empty in production until lane 8), built once by `lib.rs`
  right after `AppState`: `Exchange::new(pool, cache_dir) -> Arc<Exchange>`
  (it builds the board and the peer client). `lib.rs` spawns
  `peer::hasher::run` next to it.
- `peer::router(exchange)` = `lan::router` (status, probe) merged with
  `api::router` (the peer API); `lib.rs` merges it into the app's router at
  "11. Axum HTTP server". `merge` is valid because only `api::router` (the
  dashboard's) carries a fallback (the SPA): axum panics when merging two
  routers that both have one, so an exchange router never gets a fallback.
  The peer routes get none of the dashboard's layers (no CORS).
- Tests: `peer/lan_tests.rs` (the route through `peer::router` + `oneshot`
  over a migrated in-memory database; the merge exactly as `lib.rs` does it,
  `api::router` with a dist dir merged with `peer::router`, still serving
  the SPA), `peer/config_tests.rs`, `api/settings_tests.rs`. On the box:
  `e2e/post-deploy-settings-secrets.spec.ts` reads the status read-only.

## Kinds, versions, jobs (`peer::kind`, `peer::wire`, `peer::board`)

- Kinds (`kind::ArtifactKind`, serde snake_case): `video`, `audio`,
  `stem_vocals`, `stem_instrumental`, `lyrics`, `metadata`; `parse` reads a
  URL segment (exact, never `unknown`). An unknown kind (a newer peer's
  `dub`) reads as `Unknown`.
- `Catalog::sanitized` keeps only what this node can use (Review Focus 3):
  - an entry needs a known kind and a valid YouTube id; an artifact also a
    sha256 as 64 lowercase hex digits; a job also a known state and a
    `node` that is a node name (`peer::config::valid_name`);
  - separately, the catalog's own `node` reads as `""` when it is not a
    node name;
  - a time (`updated_at`, `started_at`) is rewritten in the canonical form
    (`wire::checked_time` = `ms_to_rfc3339` of the parsed instant: UTC,
    milliseconds, `Z`), never kept as the peer's text; one that is not an
    RFC 3339 time, or whose canonical form would not read back (a year
    outside 0000..=9999 once in UTC), reads as `None` (the entry stays);
  - a queued entry's `started_at` is dropped.

  A peer is named by its CONFIGURED name (`PeerConfig::name`), never by the
  `node` it sends. Times are information only, never a decision's input.
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
  version ≥ 1 is taken. The plan's code ranked every labelled non-manual
  row with `gemini_failed = 0` as a provider's, which would have advertised
  a no-provider regex guess as a provider title (#229 comment 6030334867).
  A new `sp_core::metadata::MetadataSource` variant must be ranked here:
  the exhaustive `rank` match in
  `kind_tests.rs::every_metadata_source_label_is_ranked` stops compiling
  until it has an arm there; add it to that test's list and to
  `metadata_version` too (the list is not checked for completeness).
- Jobs (`kind::Job`): Download (needs video+audio, makes
  video+audio+metadata), Lyrics, Stems (needs and makes both stems).
- A catalog's job entry is `wire::CatalogJob { youtube_id, kind, node,
  state, started_at }`, one per kind the job makes. `state` = `running` |
  `queued`: the catalog lists a node's QUEUED jobs too (the plan's
  decisions), and a node is to wait for either (lane 7).
  `started_at` is set only for a running job (`null` for a queued one).
  `Catalog::announces(id, kinds)` = a running OR queued job (lane 7's
  Wait). The plan's lane-2 text calls these `RunningJob` / `Catalog::runs`:
  the code names them `CatalogJob` / `announces` (the plan's later lanes
  already use the new names).
- A RUNNING job is announced by the in-memory `board::JobBoard` while its
  `#[must_use]` `JobGuard` lives (`Exchange::announce(youtube_id, job)`); a
  crash takes the announcements along, never a DB row. Nothing outside the
  tests announces yet: lane 7's `Exchange::ask` answers a job it runs here
  with `Local(JobGuard)`, and the first worker to ask is lane 8's download
  hook (lane 9 adds stems and lyrics). A second guard of the same job keeps
  the first one's start; the last guard to drop ends it.
  `JobBoard::snapshot(node)` = the running entries, sorted by YouTube id,
  then by the kind's WIRE NAME (`stem_instrumental` before `stem_vocals`,
  not the enum's order).
- QUEUED entries are not on the board: the catalog adds them from the rows
  (`peer::queued`, below). Once lane 8 announces downloads, a download in
  progress is still a queued row (`normalized = 0`) AND on the board, so
  `catalog::listed_jobs` lists each `(id, kind)` once: as running when the
  board holds it.
- OPEN for lane 7's design (the main decides it; #229 comment 6030598502
  item 7, corrected by 6030935897): once SNV lists PP as a peer too (phase
  2), two nodes with the same song queued would each wait on the other's
  queued entry for the full 2 h, then both process it — the double work the
  queued entries exist to avoid. A tie-break must keep phase 1 as decided:
  PP is to wait for SNV's queued jobs (ROZHODNUTÉ 6022851957 point 2). A rule
  keyed on the names needs both nodes to see the same pair, i.e. each
  node's configured name for a peer equals that peer's own `node_name`;
  nothing checks that yet (lane 7 could compare `PeerRead.peer` with the
  sanitized `Catalog.node` and WARN on a mismatch). The wire's `state`
  tells queued from running.
- `wire::PeerMetadata::to_bytes` = the metadata artifact's canonical bytes
  (serde field order; the catalog's metadata sha256 is over them, and the
  artifact route serves them). Times: `now_ms`, `ms_to_rfc3339` (`2026-10-06T16:00:00.123Z`; an
  instant out of chrono's range = `""`), `rfc3339_to_ms` (any offset,
  trimmed).
- Tests: `peer/kind_tests.rs`, `peer/wire_tests.rs` (a newer peer's
  catalog: Review Focus 3), `peer/board_tests.rs`. A sha256 in a test is
  built (`"0123456789abcdef".repeat(4)`): the staging hook refuses a 40+
  character hex literal.

## The catalog and its sha256 cache (`peer::catalog`, `peer::hasher`, V29)

- The catalog comes from this node's OWN rows (`catalog::artifact_files`):
  - per YouTube id the lowest row with an audio (rows share files, #136):
    its video, its audio, and its stems only when a row of the video with
    that audio has `stem_status = 'done'`, named after the CURRENT audio
    (`stems::stem_paths`);
  - `{yt}_lyrics.json` at the highest pipeline version of the video's rows
    with lyrics, never when ANY row of the video (lyrics or not) is
    dub-requested or carries `gemini-live-translate`: that one file per
    video is then the dub's subtitles (Review Focus 5);
  - metadata from the lowest row with a song (`metadata_for`);
  - a YouTube id that is not one is never listed.
- A file is listed only once `peer_hashes` (V29, `db::models_peer`) holds it,
  keyed by path, valid while size + mtime match. Building a catalog reads no
  file. `updated_at` = when it was hashed; `?since=` keeps files hashed
  strictly after it; metadata entries have no time and are always listed;
  jobs are always listed. A file rewritten under the same path keeps its
  old sha and size in the catalog until the hasher's next pass reaches it:
  a fetch then fails closed (size bound, sha check) and asks again later.
- The hasher (`hasher::run`, every 60 s, the first pass after 60 s, only
  while this node serves and is not paused) hashes one file at a time at
  40 MiB/s: stat → hash → stat, a file that changed meanwhile waits for the
  next pass (`HashPass::changed`); a missing or unreadable file loses its
  entry; a path no row names is pruned (a renamed song). A pass is logged at
  INFO when it hashed, found changed or pruned something, or the missing
  count moved. SNV's first pass over ~115 GB takes ~50 min.
- `throttle::wait_for` keeps an average rate and pauses at most
  `MAX_PAUSE` (2 s) at a time, never reached at the rates in use: a stall or
  a wrong rate never hangs a transfer, a hash, or a test (the mutation gate
  reads a hang as a failure).

## Queued jobs (`peer::queued`; ROZHODNUTÉ 6022851957 point 2)

- A job is queued when this node's own worker would take its row NOW, read
  with the worker's own predicate, never a copy:
  - download: `downloader::DOWNLOAD_DUE` (`fetch_next_unprocessed`'s: not
    downloaded, active playlist, retry due);
  - lyrics: `lyrics::reprocess::queued_where`, buckets 1–3 of the lyrics
    queue (manual, null, stale; bucket 4, the full-mix upgrade, already
    serves lyrics at the current version), only while
    `lyrics_worker_enabled` is on, and never for a video the catalog will
    not serve lyrics of (the dub rule above: any row dub-requested or
    Live-Translate);
  - stems: `db::models_stems_priority::STEM_ELIGIBLE_PRED`, only while
    `stem_worker_enabled` is on.
- A change to a worker's queue changes what the catalog announces: keep the
  shared predicate the one truth (`LYRICS_DUE` / `LYRICS_NOT_PARKED` are
  what the buckets and `queued_where` are built from).
- One entry per kind the job makes (`Job::makes`), `started_at: null`.
- Lane 7 (OPEN, above) is to decide how a node waits on a peer's queued
  entry.

## The peer API (`peer::api`)

- `GET /api/v1/peer/catalog[?since=<RFC 3339>]`, `GET
  /api/v1/peer/videos/{id}`, `GET /api/v1/peer/artifact/{id}/{kind}`.
- The guard (a `route_layer`, in this order): 404 when this node does not
  serve (no name, no key, or settings that do not hold, WARNed); 401
  without `X-SP-Peer-Key` = this node's `peer_api_key` (sha256 digests
  compared; WARN with the path, never the key); 503 `Retry-After: 600`
  while `peer_transfers_paused` (on EVERY route, the design record
  6032086364; after the key, so a caller without it learns nothing). Every
  answer is `Cache-Control: no-store`. On the public path Cloudflare Access
  is in front as well (a service-token policy, MAIN SESSION OPS).
- `videos/{id}` = `wire::PeerVideo {metadata, duration_ms, lyrics:
  PeerLyrics {source, pipeline_version, alignment_model, reference,
  translation_version, translation_gender}}` (`catalog::peer_video`): the
  first titled row's metadata, the duration of the first row that has
  one, the served lyrics row with the catalog's dub rule. 404 unknown, 400 a bad id. Lanes 8-9 are to adopt from it;
  `PeerClient::video` refuses a row of another video than the one asked.
- `artifact/{id}/{kind}`: 404 for an unknown or absent kind (a dubbed
  video's lyrics too), 400 a bad id; files through tower-http `ServeFile`
  (Range → 206, HEAD, 416) in a body throttled to `peer_serve_max_mbps`
  (`throttle::throttled`), per response: phase 1 has one asking peer, which
  sends one transfer at a time; a node-wide cap is for a second peer
  (phase 2). The `metadata` kind answers
  `PeerMetadata::to_bytes` as `application/json` (its sha is the catalog's).
- SNV's post-deploy gate `e2e/post-deploy-peer-serving.spec.ts` needs no
  key: the status says node `snv`, settings that hold, serving, catalog
  files > 0; `GET /api/v1/peer/catalog` WITHOUT the key answers 401, no
  body, `no-store` (a 200 would be the SPA: no peer API). If it reddens
  after a settings change, read `/api/v1/exchange/status`'s `config_error`
  first. SNV's identity was set once by the main session through the secret
  channel (#229 comment 6031719797).

## The peer client (`peer::client`, `peer::fetch`)

- Redirects are NEVER followed: Cloudflare Access refuses a bad or missing
  service token with a 302 to its login page (`auto_redirect_to_identity`),
  and the login page answers 200. Status → `PeerError`: 3xx/403
  `AccessRefused`, 401 `KeyRefused`, 404 `NotFound` (for a catalog: the
  API is off there), 503 `Paused`, other non-2xx `BadResponse`; a transport
  error is `Unreachable`: its cause chain (connect, DNS, TLS, timeout)
  without the URL (a TLS name mismatch names the peer's host, as the status
  does). No error text holds a key.
- Every GET carries `X-SP-Peer-Key`, plus `CF-Access-Client-Id/Secret` for a
  peer with a token. Timeouts: 10 s connect, 60 s between two reads, 30 s
  for a whole catalog or video row (an artifact has no total bound).
- Bodies are bounded (catalog 32 MiB, video row 64 KiB) and parsed into the
  typed wire structs (never `serde_json::Value`); a parse error names only
  line and column. A catalog is `Catalog::sanitized`.
- A good catalog is cached 60 s (`fresh`, `PeerClient::catalog`); a failed
  read is not cached; `read_catalog` reads now and records the peer's
  `LastRead`.
- An artifact → `<cache>/peer/<yt>_<kind>_<sha16>.part` (`fetch::part_name`;
  another part of the same video and kind, an older copy's, is dropped):
  resumed with `Range: bytes=N-` (a 206 must start at N, else the part is
  dropped; a 200 restarts it), never past the catalog's size (one byte over:
  dropped), sha-checked at the end (a mismatch drops the part AND the
  cached catalog); a short body keeps the part for the next attempt. One
  transfer at a time per peer (`PeerClient::slot`); `Exchange::fetch`
  refuses while paused. The caller renames the verified part into place
  (lanes 8-9) and fetches one job's artifacts from one peer: two fetches of
  the same video and kind from two peers would drop each other's part
  (phase 2 has two peers; lane 7's design is to keep one fetch per job).
- The Cloudflare Access service token for PP is MAIN SESSION OPS (plan task
  5.3 step 8, pending the owner on 7.10.2026, #229 comment 6031719797):
  until it exists the probe of `https://sp.newlevel.media` answers
  `AccessRefused(302)`.

## Tests

- `peer::rig::TestNode` = a node with its own in-memory DB, cache dir and a
  real 127.0.0.1 port serving `peer::router`; `give_song` / `give_stems` /
  `give_lyrics` / `hash_now`; `SNV_KEY` / `OTHER_KEY` (fixtures contain
  `example`, never a real key). Two nodes talk over real HTTP
  (`client_tests.rs`, `fetch_tests.rs`, `lan_tests.rs`).
- wiremock stands in for Cloudflare Access and a peer (`client_tests.rs`,
  `fetch_tests.rs`); the routes go through the real `peer::router`
  (`api_tests.rs`, `lan_tests.rs`). A real-time rate test asserts a LOWER
  bound only (a slow runner passes); exact rates run on a paused clock
  (`throttle_tests.rs`).

## Writing a lane's docs (lane 2: review rounds 7–9 on this alone)

- Code that only later lanes call: every doc comment and rules line about
  what a LATER lane builds says it as future work and names the lane that
  does it ("lane 3's catalog is to list them"), never in the present tense.
- Read the lane from the plan's lane sections, not from memory: the lane
  that first CALLS the code is often not the one that builds the API it
  goes through (`Exchange::ask` is lane 7's, but "nothing calls `ask` yet";
  the first production caller is lane 8's download hook).
- A design point a later lane owns (e.g. lane 7's tie-break) is written as
  OPEN with its constraints, never as a rule this lane picked.
- Do this pass before the first review round: each later round reads the
  same docs again.
