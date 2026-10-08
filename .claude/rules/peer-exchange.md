---
paths:
  - "crates/sp-server/src/peer/**"
  - "crates/sp-core/src/config.rs"
  - "crates/sp-server/src/db/models_peer*.rs"
  - "crates/sp-server/src/db/mod_tests_v29.rs"
  - "crates/sp-server/src/db/mod_tests_v30.rs"
  - "crates/sp-server/src/db/mod_tests_v31.rs"
  - "crates/sp-server/src/lyrics/queue_sql.rs"
  - "crates/sp-server/src/downloader/mod.rs"
  - "crates/sp-server/src/downloader/mod_tests_peer.rs"
  - "crates/sp-server/src/stems/worker.rs"
  - "crates/sp-server/src/lyrics/worker.rs"
  - "crates/sp-server/src/reprocess/mod.rs"
  - "crates/sp-server/src/stems/worker_tests_peer.rs"
  - "crates/sp-server/src/lyrics/worker_tests_peer.rs"
  - "crates/sp-server/src/reprocess/tests_peer.rs"
  - "e2e/post-deploy-peer-serving.spec.ts"
  - ".github/workflows/deploy-pp.yml"
  - "scripts/pp_deploy_pick.py"
  - "scripts/setup-runner.ps1"
  - "scripts/tests/test_deploy_pp_workflow.py"
  - "scripts/tests/test_pp_deploy_pick.py"
  - "e2e/post-deploy-pp*.ts"
  - "e2e/peer-probe-gate*.ts"
  - "e2e/pp-scenes*.ts"
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
and the peer client with the probe. Lane 6 ships the PP deploy
(`deploy-pp.yml`, its runner label and PP's post-deploy subset, below).
Lanes 7–9, one combined lane (design record 6035823568), ship the ask-first
core and its four hooks: the download, stem, lyrics and metadata-repair
workers ask their peers before they run a job (below, from "Ask first").
The PP lyrics lane (PP audit 6054582866, design record 6056986290) makes
PP wait for SNV's lyrics and replace a track it made itself ("The lyrics
wait for a peer that has the song", below).

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
  It is the live gate of PP → SNV through Cloudflare (PP's post-deploy
  subset calls it, "PP deploy" below).
- `POST /api/v1/exchange/probe/transfer` (LAN, no key; `peer::transfer_probe`,
  the #229 follow-up lane) transfers one real artifact of every peer now:
  `[{name, base_url, ok, artifact, bytes, sha256, latency_ms, error}]`.
  - Per peer: its catalog read now, its smallest non-empty FILE artifact
    (`transfer_probe::pick`; a `metadata` entry is answered from a row, not
    through the file path), at most `PROBE_MAX_BYTES` (64 MiB).
  - Fetched through `PeerClient::fetch_unslotted` (the same request, size
    bound and sha check as an adoption) into a temp dir under the OS temp
    dir, NEVER the cache, removed when the probe ends. `bytes` / `sha256`
    are what arrived, read back. A fresh temp dir holds no part, so no
    `Range` request is sent: Range through Cloudflare stays unproven by the
    gate (the adoptions' resume uses it).
  - Bounded in time too: `PROBE_MAX_TIME` (300 s per peer, the catalog
    read included, under the gate's 330 s request bound with PP's one
    peer; phase 2, with more peers probed one after another, can pass
    that bound). A transfer that trickles past it fails (`the probe took
    over 300s`), its temp dir removed, and the probe lock is free for the
    next gate run. Known limit: on Windows a blocking write still holding
    the part when the bound fires can make that removal fail silently,
    leaving at most 64 MiB in `%TEMP%\songplayer-peer-probe-*`.
  - It does NOT take the per-peer transfer slot: its part is its own and
    small, and the gate must not wait behind the workers' queued transfers
    (a `tokio::sync::Mutex` serves them in arrival order, up to one per
    worker, a whole video among them). `fetch` takes the slot and calls
    `fetch_unslotted`; only the probe calls the latter directly.
  - Refused (per peer, `ok: false`) while this node's transfers are paused,
    with no catalog read either. One probe at a time: a second answers 409
    (`Exchange::transfer_probe`, a lock per node); settings that do not hold
    answer 409 too.
  - PP's post-deploy subset calls it (`transferFailures`, "PP deploy"
    below).
- Later lanes may ADD fields to `ExchangeStatus` / `PeerStatus`.

## Wiring

- `peer::Exchange { pool, cache_dir, board, client, transfer_probe }`
  (`client` and `transfer_probe` are `pub(crate)`; `cache_dir` holds the `{yt}_lyrics.json` the catalog names
  and the fetch parts dir `<cache>/peer/`; `board` = the jobs announced as
  running, below), built once by `lib.rs` right after `AppState`:
  `Exchange::new(pool, cache_dir) -> Arc<Exchange>` (it builds the board and
  the peer client). `lib.rs` spawns `peer::hasher::run` next to it and hands
  a clone to each asking worker (`with_peer`, "Ask first" below).
- `peer::router(exchange)` = `lan::router` (status, probe, transfer
  probe) merged with
  `api::router` (the peer API); `lib.rs` merges it into the app's router at
  "11. Axum HTTP server". `merge` is valid because only `api::router` (the
  dashboard's) carries a fallback (the SPA): axum panics when merging two
  routers that both have one, so an exchange router never gets a fallback.
  The peer routes get none of the dashboard's layers (no CORS).
- `Exchange::transfer_probe` (a `tokio::sync::Mutex<()>`) is held while a
  transfer probe runs: one at a time per node (`lan::probe_transfer`).
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
  decisions), and a node waits for either (`peer::decide`).
  `started_at` is set only for a running job (`null` for a queued one).
  `Catalog::announces(id, kinds)` = a running OR queued job (lane 7's
  Wait). The plan's lane-2 text calls these `RunningJob` / `Catalog::runs`:
  the code names them `CatalogJob` / `announces` (the plan's later lanes
  already use the new names).
- A RUNNING job is announced by the in-memory `board::JobBoard` while its
  `#[must_use]` `JobGuard` lives (`Exchange::announce(youtube_id, job)`); a
  crash takes the announcements along, never a DB row. `Exchange::ask`
  answers a job it runs here with `Local(JobGuard)`; the download, stem and
  lyrics hooks hold it until the job ends ("Ask first" below). A second
  guard of the same job keeps the first one's start; the last guard to drop
  ends it.
  `JobBoard::snapshot(node)` = the running entries, sorted by YouTube id,
  then by the kind's WIRE NAME (`stem_instrumental` before `stem_vocals`,
  not the enum's order).
- QUEUED entries are not on the board: the catalog adds them from the rows
  (`peer::queued`, below). A download in progress is still a queued row
  (`normalized = 0`) AND on the board, so `catalog::listed_jobs` lists each
  `(id, kind)` once: as running when the board holds it.
- The tie-break for queued jobs (design record 6035823568): a node waits
  only for a peer listed in its OWN `peers` (`Exchange::ask` reads only
  those catalogs). SNV lists none in phase 1, so SNV asks nobody and every
  job there runs at once; PP waits for SNV's queued and running jobs.
  OPEN for phase 2 (SNV listing PP): two nodes with the same song queued
  would each wait on the other's queued entry for the full 2 h, then both
  process it. A rule keyed on the names needs each node's configured name
  for a peer to equal that peer's own `node_name` (nothing checks that yet;
  `PeerRead.peer` vs the sanitized `Catalog.node` is where to compare).
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
- While the hasher runs (`hasher::should_hash`: serving, not paused), a
  file the rows name that is on disk but not hashed yet is announced as its
  job, QUEUED (`catalog::unhashed`, `Job::making`; #229 finding
  6036287850): a job that just ended here is otherwise neither announced nor
  listed until the next hash pass (up to ~70 s), and a peer waiting for it
  would read "nobody has it" and redo it. A file missing from disk is no
  job. Only the unhashed paths are stat'ed: in steady state, only a file
  the rows name that is missing from disk. A node that does not serve (PP
  in phase 1) announces none: nobody reads its catalog and its hasher never
  runs. The status's `catalog.queued` counts these entries too.
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
  - lyrics: `lyrics::queue_sql::queued_where`, buckets 1–3 of the lyrics
    queue (manual, null, stale; bucket 4, the full-mix upgrade, already
    serves lyrics at the current version), only while
    `lyrics_worker_enabled` is on, and never for a video the catalog will
    not serve lyrics of (the dub rule above: any row dub-requested or
    Live-Translate). Also, while `stem_worker_enabled` is on, a row those
    buckets take once its own recheck time has come
    (`queued_later_where`, its recheck ignored) whose stems are queued
    here (`STEM_ELIGIBLE_PRED` on the same row; a running stems job's row
    still matches it): a new song's lyrics wait for its stems
    (`WaitingForStems` puts the row back for 10 min,
    `record_lyrics_wait`), and the worker WILL take it once they are done.
    Before (PP audit), SNV listed no lyrics job for most of the stems' run
    and PP read "nobody has it". Stems that are done, unsupported, in a
    failure backoff or impossible (no audio) are no such wait. The window
    between the stems done and the lyrics' recheck (≤ 10 min) stays
    unlisted: PP waits through it because SNV has the song (below). A row
    whose recheck is a lyrics FAILURE backoff (up to 24 h) is listed too
    while its stems are queued (it will be taken, later): a peer then waits
    up to its 2 h bound. Two queries, merged and sorted, each id once;
  - stems: `db::models_stems_priority::STEM_ELIGIBLE_PRED`, only while
    `stem_worker_enabled` is on.
- A change to a worker's queue changes what the catalog announces: keep the
  shared predicate the one truth. The lyrics queue's predicates live in
  `lyrics/queue_sql.rs` (`reprocess.rs` is near the cap): ONE text builds
  `LYRICS_ELIGIBLE` and `LYRICS_DUE` (a `macro_rules!` + `concat!`),
  `LYRICS_NOT_PARKED`, and buckets 1–3's conditions written once for
  `queued_where` and `queued_later_where`; the selector's buckets read
  `LYRICS_DUE` / `LYRICS_NOT_PARKED` from there.
- One entry per kind the job makes (`Job::makes`), `started_at: null`.
- A node waits on a peer's queued entry exactly as on a running one
  (`peer::decide`, bounded at 2 h), and only on its own listed peers'.

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
  one, the served lyrics row with the catalog's dub rule. 404 unknown, 400
  a bad id. The download, lyrics and repair hooks adopt from it;
  `PeerClient::video` refuses a row of another video than the one asked.
- `artifact/{id}/{kind}`: 404 for an unknown or absent kind (a dubbed
  video's lyrics too), 400 a bad id; files through tower-http `ServeFile`
  (Range → 206, HEAD, 416) in a body throttled to `peer_serve_max_mbps`
  (`throttle::throttled`), per response: phase 1 has one asking peer, which
  sends one transfer at a time (plus, at a deploy, PP's small transfer
  probe); a node-wide cap is for a second peer
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
  does). `NotYet` is local: this node cannot tell yet whether a peer's copy
  fits it (`peer::audio`). No error text holds a key.
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
  transfer at a time per peer (`PeerClient::slot`; only the transfer probe
  skips it, `fetch_unslotted`); `Exchange::fetch` refuses while paused. The adopters rename the verified part into place
  and fetch one job's artifacts from ONE peer (`FetchPlan` = one peer):
  two fetches of the same video and kind from two peers would drop each
  other's part. A failed adoption keeps a verified part: the next attempt
  re-hashes it locally and does not transfer it again.
- The Cloudflare Access service token for PP is MAIN SESSION OPS (plan task
  5.3 step 8, pending the owner on 7.10.2026, #229 comment 6031719797):
  until it exists the probe of `https://sp.newlevel.media` answers
  `AccessRefused(302)`, and PP's post-deploy subset fails naming the
  missing token (below).

## PP deploy (`deploy-pp.yml`)

- PP gets main releases only: `workflow_run` of CI on `main` (green, push,
  and still main's tip) or an explicit `gh workflow run deploy-pp.yml -f
  ci_run_id=<CI run>` (a dev build, or an older release, at PP only on
  purpose). Both go through `scripts/pp_deploy_pick.py`: a completed, green
  `push` run of `CI` (`.github/workflows/ci.yml`) in this repository, else
  the `resolve` job fails and nothing touches PP. Both triggers need the
  workflow on `main` (the default branch).
- GitHub fires `workflow_run` `completed` for EVERY attempt of a run. A
  re-run of an OLDER main CI run (the SNV restart recipe, `gh run rerun
  --job <Deploy>`, ci-workflows.md) is skipped: its `head_sha` is not
  `github.sha` (for a workflow_run event, main's last commit). A re-run of
  the LATEST main run does re-deploy PP with the same build: that is also
  how a failed release attempt reaches PP. A re-run of an OLD deploy-pp
  run keeps its original event and `github.sha`, so it passes that test
  again: the deploy job's check step therefore reads main's LIVE tip
  (`GET /repos/…/git/ref/heads/main`, which builds no diff) before anything
  stops and fails when the release is no longer it (fail closed: every
  event but a dispatch is checked). Never re-run an old deploy-pp run;
  dispatch the build PP should get. Such a re-run is evaluated with its
  original payload, so it also takes the `deploy-pp` group and can push a
  release pending there out before it is refused: dispatch that release
  again then.
- Its own concurrency group `deploy-pp` (an offline PP runner never blocks
  CI), never cancelled half-way. Only a run that really deploys takes it:
  GitHub keeps one PENDING run per group and cancels the one already
  pending, so a CI run on main that failed, was cancelled or is no longer
  main's tip (each starts this workflow too, which then skips) gets a group
  of its own and can never replace a release waiting there. The group's
  condition is the `resolve` job's `if:`, pinned equal by the guard test.
  Real deploys do replace each other while pending: a newer release, or a
  dispatch, replaces the one waiting (the newest wins; dispatch the one you
  need again after it).
- Runner: label `resolume-pp` ONLY (`RUNNER_LABELS=self-hosted,windows,
  resolume-pp` for `scripts/setup-runner.ps1`, which refuses `resolume` on
  `RESOLUME-PP`); it must never carry `resolume` (SNV's jobs would land on
  PP). No other workflow names `resolume-pp`, and every other self-hosted
  job names `resolume` (the guard test), so none can land on PP.
- The deploy downloads the run's `tauri-installer` + `dist` and checks them
  (one installer, an `index.html`), the box (`COMPUTERNAME` must be
  `RESOLUME-PP`: a runner that carries `resolume-pp` by mistake never stops
  or installs anything), the release (still main's live tip, above) and
  the phase-0 `SongPlayer` task BEFORE it stops anything: each failure
  leaves PP running. The always() Start step and the `e2e-pp` job check
  the box too, so a wrong box is never touched at all. Then: stop (the task instance first, then the process,
  CLIProxyAPI, port 8920 free; its own 5 min step bound), install `/S` (exit
  code read; a 10 min step bound), copy `dist`, start the task, check the
  version. The step bounds turn a hung stop or installer into a plain step
  failure within minutes, so the Start step below runs.
- "Start SongPlayer" is `if: always()` (the one step that may be): after a
  failed install or a cancel it starts the task again (a no-op when nothing
  was stopped) and waits up to 90 s for the process and
  `/api/v1/status`, else it fails on its own ("did not come back - the wall
  may be dark"): "Health checks" is skipped once a step failed. Whether a
  job TIMEOUT runs `always()` steps is not verified on this runner. No
  deploy step writes PP's DB or settings, nor the task, the ACL or the
  firewall (phase 0 owns them). The server itself does: the new build's
  first start runs its migrations, the gate's presses and restore cut save
  the program source, and a song the pressed playlist starts is recorded in
  `play_history` (it weighs the next pick), as on any press.
- PP is often off. A job queued on an offline self-hosted runner fails after
  24 h; `ci.yml` keeps `dist` 5 days (as `tauri-installer`), so once PP is
  on, `gh workflow run deploy-pp.yml -f ci_run_id=<the newest green main CI
  run>` redoes it within 5 days (never the re-run button of the failed
  deploy-pp run: refused once main moved on); later, the next release
  brings PP up to date.
- Post-deploy at PP: `post-deploy-pp.config.ts` = `post-deploy-pp.spec.ts` +
  `post-deploy-max.spec.ts` (needs Node.js at PP). Not serial: a missing
  Cloudflare token must not hide the playback results.
  - the version in the DOM, a clean console;
  - PP's identity and its peer (`peer-probe-gate.ts::peerSetupFailures`):
    `node_name` `pp`, settings that hold, a peer `snv` with its key at
    `https://sp.newlevel.media` (that host exactly, TLS, the default port)
    with a Cloudflare Access token — a peer without one fails naming
    `cf_client_id + cf_client_secret`;
  - the live probe (`probeFailures`): the read worked, the catalog lists at
    least one artifact;
  - the real transfer (`transferFailures`, `POST
    /api/v1/exchange/probe/transfer`, a 6 min test bound): SNV's smallest
    file artifact arrived whole (the catalog's byte count) with the
    catalog's sha256, through `https://sp.newlevel.media`; a refusal, a
    metadata or empty artifact, a short transfer or another sha fails it,
    naming the cause;
  - a playlist pressed through the facade plays on program (health
    `Playing/Playing`) and SP-program has a receiver (an NDI receiver of
    `SP-program` must exist at PP, else this is red on every release). The
    scene comes from SongPlayer's own scene catalog
    (`pp-scenes.ts::pickPlaylistScene`: an active, not refused, not Dabing
    playlist with a normalized video; among those `pickBaselineScene`:
    `sp-slow`, else an `sp-*` but `sp-fast` / `sp-warmup`, else a non-`sp-`
    one, else the first — `sp-fast` or `sp-warmup` when only they are
    left), never from the facade's list: that list is cg OBS's, forwarded,
    and PP's cg OBS has no `sp-*` scene;
  - a manual scene pressed through the facade lands as "OBS manuál"
    (`manualCutLanded`: source -1, cut for that scene, `cg_forward ok`; a
    scene name is compared as the server records it, its first 64
    characters, `recordedScene`). The
    scene (`pickManualScene`) is the repo variable `PP_MANUAL_SCENE` when set
    (a manual cg OBS scene, else the test fails saying why), else the scene
    cg OBS already has on program (nothing changes on cg OBS). When cg OBS's
    program is not a manual scene, the test fails naming `PP_MANUAL_SCENE`:
    the gate never picks another cg OBS scene by itself. The spec reads cg
    OBS on `OBS_WS_URL` (:4455). Precondition (nothing checks it): that
    scene must not itself show `SP-program` (an NDI `… (SP-program)`
    source), or cutting to "OBS manuál" loops the program into itself on
    the wall.
  - `afterAll` puts SP-program back on the source it had before the tests
    with a dashboard cut (`POST /api/v1/program/cut`, which tells cg OBS
    nothing), only while the program is still what the gate left
    (`programRestoreTarget`): the latest recorded switch
    (`remote.last_remote_cut`) is the gate's own last press (its scene,
    recorded at or after the instant the gate sent it; the runner and
    SongPlayer share PP's clock) and the program is on that press's source
    — or, when that last press was refused (a keep: the NDI input inactive,
    cg OBS refusing), on the source of the press before it, so a refused
    manual press never leaves the test playlist on air. A playlist start
    source is re-kicked to its next song, as at SNV. cg OBS goes back on
    its own scene only when the manual press moved it and it is still there
    (`cgRestoreTarget`), through the facade when SP-program stays on "OBS
    manuál" and that scene is a manual one (so SongPlayer names the program
    by it again), else on cg OBS directly (`cgRestoreVia`). An operator's
    press meanwhile is kept on both. Nothing on program at the start
    (`source` null): nothing to cut back to, the gate's last cut stays on
    air and persisted.
  - The live probe reads SNV, and SNV's own pipeline may be restarting
    SongPlayer at that moment (main's E2E, then the post-merge dev push on
    SNV's single runner): a red probe right after a release is checked
    against SNV's deploy timeline before anything at PP.
- "produkcia beží" at PP:
  - `gh workflow disable deploy-pp.yml` first: no new run starts; also
    cancel any deploy-pp run already pending or queued (`gh run list
    --workflow deploy-pp.yml --status pending` / `--status queued`, then
    `gh run cancel <id>`: whether a disable stops a run already pending in
    the group is not verified);
  - a run in flight: cancel it while it is still in `resolve`, the
    downloads or "Check the build"; once "Stop SongPlayer" ran, let the
    deploy job finish (~2 min; a cancel there still starts SongPlayer, but
    may cut the installer half-way);
  - `e2e-pp` starts on the same runner within seconds. Its first scene press
    comes only after its checkout, `npm ci`, the Playwright install and the
    MAX test (~1–2 min): cancel in that window. A cancel mid-suite sends
    Playwright Ctrl+C, then a kill ~7.5 s later, so `afterAll` may run only
    in part or not at all: the program may stay on the test playlist (and
    it is persisted) until the operator's next press;
  - touch nothing else at PP. After "event skončil": `gh workflow enable
    deploy-pp.yml`, then dispatch the newest green main CI run if a release
    landed meanwhile.
- Security: PP and SNV are self-hosted runners of a PUBLIC repository. The
  repo's fork-PR approval policy must be `all_external_contributors`
  (`gh api -X PUT repos/zbynekdrlik/songplayer/actions/permissions/fork-pr-contributor-approval
  -f approval_policy=all_external_contributors`; it read
  `first_time_contributors` on 7.10.2026), set BEFORE the PP runner is
  registered (MAIN SESSION OPS, #229 lane 6), so no outside contributor's
  workflow runs on either box without approval.
- A DB copied SNV → PP carries SNV's `node_name` / `peer_api_key`: PATCH
  PP's `node_name=pp` and `peer_api_key=""` before that DB's first start.
- Guards: `scripts/tests/test_deploy_pp_workflow.py` (the triggers, the
  main-tip rule, the label both ways (every other `runs-on` a one-line
  literal), the group, the order, the step bounds, `always()` on Start only
  and its wait, nothing of PP's setup touched, `dist` 5 days, SNV ignoring
  the PP subset) and `scripts/tests/test_pp_deploy_pick.py` (Eval
  Checks pytest; the script is in both ruff lists); the pure e2e helpers in
  the mock suite (`peer-probe-gate.spec.ts`, `pp-scenes.spec.ts`).

## Ask first (`peer::decide`, `peer::ask`, V30)

- `decide` (pure), the first case that applies: a peer holding EVERY
  needed artifact (`Job::needs`) at a version this node takes → Fetch
  (even after 2 h); waited ≥ `MAX_PEER_WAIT` (2 h) → Local; a peer
  announcing a job (running or queued) that makes those kinds → Wait
  (`PeerRunsIt`); a job that waits while a peer has the song
  (`Job::waits_while_a_peer_has_the_song`: the lyrics alone) and the listed
  peer this node took the video's audio from (`decide::SongFrom`, the
  `peer_fetches` audio record's node and sha256, `Exchange::song_from`)
  still listing that very audio (`decide::song_holder`, review round 2:
  the sha too) → Wait (`PeerHasTheSong`, same bound); a peer whose catalog
  could not be read (an outage, a refused key or token, its API off) →
  Wait, same bound; else Local (`NobodyHasIt`: nobody has or makes it, and
  for the lyrics the peer they took the song from does not list it). No
  peer → Local, no read. `ask` reads `song_from` only for a lyrics job
  whose result does not already stand in for a peer's copy (below).
- `Exchange::ask(job, youtube_id)`: settings that do not hold → Local
  (WARN); not asking (no node name or no peer: SNV in phase 1) → Local with
  no network and no DB write; else the peers' catalogs (cached 60 s), the
  wait so far, `decide`. Local returns `Local(JobGuard)`: the job is
  announced in this node's catalog while the hook holds the guard. A Wait
  records its start (`peer_waits`, the FIRST start kept).
- Every job that runs here after a peer read goes through
  `Exchange::run_here`: it ENDS the job's wait, forgets the `peer_fetches`
  records of what the job makes (`forget_origins`, the audio guard below),
  drops the parts a fetch of it left, then announces it (a later ask never
  inherits an old start and its spent bound). The hooks' own Local paths use
  it too (an operator's lyrics ask, nothing newer — never for a track that
  stands in, below —, a fetch that kept failing, another audio). The
  no-peers and bad-settings paths only announce
  (`ask` writes nothing; the download hook then forgets the pair's
  records, below): a wait recorded before
  the settings went bad survives them, so the first ask after the fix may
  run the job here at once (its bound already spent). Accepted.
- A fetch refused by this node's own pause logs at DEBUG (it recurs for
  every row on every tick while the pause lasts); other fetch failures WARN.
- Rechecks back off (`recheck_after`): a quarter of the wait so far, 2–20
  min, never past the bound, at least 1 min. A failed fetch
  (`Exchange::fetch_failed`) counts as waiting and WARNs (`exchange: a
  peer's copy was not taken - asking again later`); a Fetch still
  wins after the bound, but a fetch that KEEPS failing does not: once the
  job has waited the bound (`decide::gives_up`), `fetch_failed` answers
  `None` and the hook runs the job here (`Exchange::after_failed_fetch`;
  a refused track, a peer that keeps answering badly). One exception
  (`decide::after_failure`): a fetch refused by THIS node's own pause
  (`peer_transfers_paused`) rechecks every 5 min (`PAUSED_RECHECK`) and
  never gives up: the pause is about this node's bandwidth, and heavy local
  work in its place would defeat it.
- A wait on a peer's announced job ignores versions: after a dev-only
  bump of `LYRICS_PIPELINE_VERSION` / `STEMS_VERSION` / `MEDIA_VERSION`
  at SNV, PP (on main) can wait up to 2 h for a job whose output it will
  not take (`kind::acceptable`), then runs it. Bounded, and such bumps are
  rare (the lyrics one owner-gated); carrying a version on `CatalogJob`
  is a follow-up candidate.
- A wait whose row was dropped meanwhile (a song removed from a playlist)
  stays in `peer_waits`; a much later ask of the same video inherits its
  spent bound and runs the job here at once. Rare; nothing expires it.
- `Exchange::fetched` ends the wait and records each artifact's origin in
  `peer_fetches` (node, version, sha256; `source = peer:<node>` in the INFO
  `exchange: done with a peer's copy`). The row's `metadata_source` /
  `lyrics_source` keep the PEER's real values (they drive
  `REPAIR_QUEUE_WHERE`, `alignment_model_for_source`, the ★ marker); never
  write `peer:<node>` into them.
- Each hook returns `PeerStep`: `Done` (the artifacts are in place and
  recorded), `Deferred` (the row's own recheck column moved, NO attempt
  counted, `Exchange::defer`: `defer_download` → `next_attempt_at`,
  `defer_stems` → `stem_next_attempt_at`, `record_lyrics_wait` →
  `lyrics_next_attempt_at`),
  or `Local(Option<JobGuard>)` (`None` = no exchange wired, a unit-test
  worker). A worker with no exchange (`with_peer` not called) runs as
  before.

## The download job (`peer::download`, the `DownloadWorker` hook)

- `process_next` → `peer::download::first` right after the row is picked:
  Done → `processed:<id>` + return true (no `downloading:` event); Deferred
  → return false; Local → the yt-dlp path, announced until `process_next`
  returns.
- The pair is fetched first, then named after THIS node's title: a local
  `manual` correction, else the peer's title when it is a provider's or an
  operator's (`adopted_title`: metadata version ≥ 1, a song, the label this
  node writes — `manual` stays `manual`, a peer's operator correction is
  final here too), else this node's providers (`download_title`). A failed
  fetch never calls a provider. Recorded by `record_download` (the local
  path's own, #136: it re-reads a correction made meanwhile, and answers
  the title it recorded); a peer's title is recorded in `peer_fetches` too
  (kind `metadata`, `download::record_title`, the repair's own), only when
  it is the title `record_download` wrote (`written_title`). Rows of one
  video share files by name (#136), so a final name may hold another row's
  file: the VIDEO is renamed first, and when it cannot take its name (that
  row's video open in a player) nothing is touched and both verified parts
  stay for the next ask (re-hashed, no transfer); when the audio then cannot
  take its name, a video this attempt placed goes back into its part, a
  video that was there stays.
- `Exchange::run_here` drops the parts of the job's needed kinds
  (`drop_job_parts`): a job that runs here (2 h bound, a fetch that kept
  failing) leaves no orphaned part in `<cache>/peer/`.
- The LOCAL download's "video rename failed" branch
  (`downloader/mod.rs::place_video`, the #229 follow-up lane) deletes the
  normalized audio only when no row records it (`cache::recorded_by_a_row`,
  read and deleted under `cache::SONG_FILES`; a failed read keeps it):
  the local download normalizes into the very name every row of the video
  records (`song-files.md`).
- Known limits (decision 6039426543 point 5: documented, revisit when PP
  runs live): `peer::stems::adopt` has no rollback when the instrumental
  cannot take its name after the vocals did (rare: the stem readers share
  delete); `models_peer::adopt_lyrics` is two UPDATEs, not one transaction
  (a failed second one leaves the row's old ★ and translation version); a
  failed catalog read is not cached (no negative cache), so while a peer
  is unreachable every ask reads it again, each bounded by the 10 s
  connect timeout. The audio guard's on-demand hash (below) has no
  single-flight: PP's stems and lyrics workers may hash the same audio
  at once (two reads at the hasher's rate), and on a serving node an
  entry it stores for an audio the catalog does not list (not the
  video's lowest row) is pruned by the next hasher pass, then taken
  again when asked. The waits' limits are under "Ask first" above.
- `downloader/` is out of the mutation gate: the logic stays in `peer/`,
  only the hook lives in `downloader/mod.rs`; its tests are `mod_tests.rs`
  (moved out for the cap) + `mod_tests_peer.rs` (tools missing on purpose:
  an attempt counted = the local path ran).

## Stems, lyrics, metadata repair (`peer::{stems, lyrics, repair}`)

- Stems: the hook runs after the kill switch, the dub and wall defers, the
  pick and the terminal skip, and BEFORE the venv's `return` (the plan's
  decisions): a node with no lyrics venv that ASKS its peers still takes a
  peer's stems (the venv WARN fires once); a row that would run here is then
  put back for `INPUT_MISSING_RECHECK` (10 min, no attempt), so the rows
  behind it reach their peer step too. A node that asks no peer (SNV) or a
  worker with no exchange stops at the missing venv as before #229
  (`no_venv_asks_peers`): no pick, no defer. Before a fetch, `song_input::job_input`
  re-reads the song's audio: none on disk → deferred with no attempt and
  nothing transferred; then the audio guard (below). The parts are renamed
  under `stem_paths(<the audio
  the row records AFTER the transfer>)`, read under `SONG_FILES`, then
  `mark_stems_done`. `song_input_tests.rs` pins the order after the hook.
- The audio guard (`peer::audio`, the #229 follow-up lane): a peer's stems
  and lyrics (★ or base tier: every track's line timings were measured on
  the peer's audio) are taken only when this node's audio IS the peer's,
  the pure `decide::same_audio` over the audio the peer lists
  (`FetchPlan::peer_audio`: its sha256 and size, read by `ask` from the
  same catalog as the decision, `decide::listed_audio`) and what this node
  has (`decide::OwnAudio`):
  - `peer_fetches` records the video's audio as fetched from that peer at
    that sha, AND the row's CURRENT audio file has that size (the record is
    per video, the audio per row: a row whose audio is a local encode under
    another title's name is not vouched for);
  - or this node's own hash of the row's current audio is that sha.

  The record is read FIRST: it reads no file, so an adopted song costs no
  audio read. Only when it does not vouch is the audio hashed
  (`Exchange::audio_sha`): its `peer_hashes` entry while that still holds
  (`HashEntry::holds`, trusted as the catalog trusts it), else hashed NOW
  at the hasher's rate and stored as the hasher would
  (`hasher::hash_unchanged`, shared with the passes). PP in phase 1 runs
  no hasher, and the audio phase 0 copied from SNV carries no fetch
  record: without the on-demand hash no copied song could take SNV's
  work.

  `Exchange::unless_peers_audio` runs it on `Ask::Fetch`, with the row's
  id, before any transfer (stems: after `job_input`, so a row with no audio
  on disk is still deferred with no attempt), and answers the step the
  hook returns instead:
  - another audio → the job runs here (`run_here`, INFO `exchange: the
    peer's copy is made from another audio than this node's`): a song this
    node downloaded itself (nobody had it within 2 h), or a peer that
    downloaded its song again since;
  - this node cannot tell yet → waited for like a failed fetch
    (`after_failed_fetch` with `PeerError::NotYet`, no attempt, within the
    2 h bound), never decided on a guess: the peer lists no audio of the
    video right now (a rename there not hashed again yet: the stems or
    `{yt}_lyrics.json` can still be listed), no audio of the row is on
    disk here (the lyrics hook; the stems hook defers that before), or
    the record does not vouch and the audio cannot be hashed now (it
    changed meanwhile, a rename here; or it cannot be read): only a
    computed sha that differs runs the job here;
  - this node's transfers are paused → checked FIRST: nothing is read or
    hashed, the job waits the pause out like a refused fetch (5 min
    rechecks, never gives up).

  Keeping the fetch record truthful:
  - the download hook forgets the pair's records
    (`Exchange::forget_origins`, `models_peer::forget_fetches`) on EVERY
    Local it answers, the no-peers and bad-settings paths included (they
    never go through `run_here`): a download here writes this node's own
    audio under the names a fetched pair had. On SNV (no peers) that is
    three no-op DELETEs per download (video, audio, metadata);
  - `run_here` forgets the records of what its job makes, for any job;
  - `peer::download::adopt` records the pair's origin BEFORE
    `record_download` makes the row playable (a stems or lyrics ask in
    between would otherwise find no record); `fetched` records the same
    rows again once done.

  A download that runs here and then fails leaves a fetched pair in place
  without its record: the guard then hashes that audio, finds the peer's
  bytes, and the peer's stems and lyrics are still taken (correctly: it IS
  the peer's audio). Test fixtures of a peer's stems or lyrics record PP's
  audio as fetched from SNV (`TestNode::audio_from`), the state a real PP
  has after its download; the guard's own tests change PP's bytes so that
  one branch alone decides (the record, a stored hash, a hash taken now).
- Lyrics: never for an operator's ask here (`lyrics_manual_priority`, a
  non-blank `lyrics_override_text`), never for a video whose
  `{yt}_lyrics.json` here is a dub's subtitles (any row of it here
  dub-requested or `gemini-live-translate`), a failed read → here. The
  peer's `/videos` row must match its catalog (pipeline version) and must
  not be `gemini-live-translate`; the same source at the same version as
  the row already serves = nothing newer → runs here, unless the track
  here stands in for the peer's copy (`standin_peer`: then it is taken
  whatever its source, review round 2) (the daily full-mix
  upgrade stays local, even while the peer runs or queues that same
  upgrade: `NothingNewer` does not look at the peer's announcements, a known
  limit — once a day per full-mix song both nodes may try it). A track over
  16 MiB is refused before the transfer (`MAX_LYRICS_BYTES`: the part is
  read whole). The JSON is parsed as a typed `LyricsTrack` whose
  `source` must equal the row's (a refused part is deleted), renamed into
  `{yt}_lyrics.json`; `adopt_lyrics` writes source, version and alignment
  model through the lyrics row's one writer (`mark_video_lyrics_complete`),
  then the ★ and the translation version (0 when this row asks another
  gender, so the retranslate pass redoes the SK lines; a row with no
  gender, auto, takes the peer's: the gender its SK lines are in). The
  worker then sends `LyricsCompleted`, as for its own lyrics. A refused
  track's error text is cut to `metadata::health::bounded_error`.
- Repair: `peer_title` at the top of `reprocess_one`, before the per-video
  backoff and the rate-limit cooldown (both are about the providers; a
  peer's title costs none). `process_all` no longer returns early in the
  cooldown, and a rate limit no longer stops the batch: the rest of it
  asks no provider but still takes the peers' titles. The peer's catalog
  must list the video's metadata at version ≥ 1 (a parser's title there
  costs no `/videos` request) and `/videos` must match that entry's sha256
  (`PeerMetadata::to_bytes`); `PeerTitle::of` (`adopted_title`) decides. `apply_title` is the
  ONE repair write (#136 locked re-check + rename + record), from a peer's
  title or the providers'; the origin goes to `peer_fetches` (kind
  `metadata`, `download::record_title`) only once it wrote the title (a row that
  left the queue meanwhile keeps no trace).
- The kill switches (`lyrics_worker_enabled` / `stem_worker_enabled`) stop
  the whole tick, the fetch included. The download and repair workers have
  none; `dub_worker_enabled` stays off at PP until the owner wants dabing
  there.

## The lyrics wait for a peer that has the song (`peer::standin`, V31; PP audit 6054582866)

The live case (8.10.2026): PP fetched `8ohdO2nINEI` from SNV, its stems
waited for SNV (`PeerRunsIt`), and its lyrics logged `exchange: no peer has
it - processing here … why=NobodyHasIt`. SNV listed no lyrics job (its
lyrics waited on its stems), PP's AI calls failed (no AI proxy reachable
from PP), and the degraded track at the current pipeline version was never
replaced: a local result leaves the lyrics queue for good (served, or
parked `no_source` by `fail_song`). Three rules:

- **SNV announces lyrics that wait on its own queued stems** ("Queued jobs"
  above).
- **A lyrics job waits while the peer it took the song from has it**
  (`decide`, above): that peer makes the lyrics from the very audio this
  node fetched, while a node's own track stays at its version for good (and
  PP's is degraded). Only THAT peer (review round 1): a node whose audio is
  its own download or a copy (no `peer_fetches` audio record) waits on
  nobody and processes its lyrics at once (a peer's lyrics would not fit
  its audio), and two nodes listing each other (phase 2) never wait on
  each other's audio. And only while it lists that very audio (review
  round 2, the record's sha256): a source that downloaded the song again
  lists another audio, whose lyrics the audio guard would refuse. The
  stems do not wait on the song (the same model on every node: a local
  separation is not degraded), nor does the download (it fetches the song
  itself).
- **A track made here while a peer had the song STANDS IN for the peer's
  copy** (`peer_standins` `(youtube_id, job, peer, made_at_ms,
  next_check_ms)`, `models_peer::{record,due,recheck,forget}_standin`):
  - recorded (`Exchange::stand_in`, INFO `exchange: made here while a peer
    has the song - its copy replaces this one once it has it`, first looked
    at 10 min later) when the lyrics job runs here after asking while the
    peer it took the song's audio from had it: `ask` →
    `Local(WaitedLongEnough)` for the listed peer `song_from` names, its
    catalog read now or not (`decide::song_source`; review round 2: an
    outage at the bound kept the degraded track for good), or
    `after_failed_fetch` giving up on that peer's copy (a peer the audio
    did not come from: no stand-in);
  - kept while the job runs here again (`ask` → any `Local` while it
    stands in: recorded again for its peer). Such an ask does not read
    `song_from`, so a local run put back after the bound (for its stems,
    memory, the wall) never starts a new 2 h `PeerHasTheSong` wait (review
    round 2: one per putting back); a Fetch still takes the peer's copy and
    a peer's announced job is still waited for;
  - over (`Exchange::drop_standin`) in `run_here` (an operator's ask,
    another audio: the job's own result is final) and `fetched` (the
    peer's copy is in place: the supersede, or the hook's adoption, which
    takes it even of the source the track here already has — "nothing
    newer" never applies to a track that stands in). The stand-in path
    calls `run_here` first, then records;
  - V31 back-filled it once: a video whose audio a peer gave (a
    `peer_fetches` audio record) with a lyrics result made here (a
    `lyrics_source`, no lyrics record), no operator text or ask and no dub /
    Live-Translate on any row, due at once (PP's `8ohdO2nINEI`; SNV fetches
    nothing). Known limit: an operator's reprocess made at PP before V31
    has the same shape (the manual flag is cleared at completion) and is
    superseded once;
  - the lyrics worker's tick, right after its kill switch
    (`supersede_next`, then `LyricsCompleted` for each row it adopted):
    ONE due stand-in (the one due longest), RESCHEDULED FIRST to a quarter
    of its age, 10 min to 6 h (`standin_recheck`: a peer that never makes
    the lyrics costs four catalog lookups a day). Then:
    - every row of the video read; none left → dropped. The audio check
      reads a row that records an audio (rows share it), else the lowest
      (review round 1: a lowest row with none kept the stand-in waiting
      forever);
    - an operator's text or ask, a dub or a Live-Translate row on the video
      (`OPERATOR_OR_DUB`) → dropped, no peer asked;
    - the listed peers' catalogs (cached 60 s): the first holding the
      lyrics at this node's version (`holds`) → the audio guard's verdict
      (`Exchange::audio_verdict`, the side-effect-free half of
      `unless_peers_audio`): another audio → dropped; this node's pause, or
      it cannot tell yet → asked again at the recheck;
    - `lyrics::peer_row` (the size bound, the peer's row against its
      catalog, never the Live-Translate track) and `lyrics::place` (the
      fetch, the typed parse, the source check, the rename into
      `{yt}_lyrics.json`): a failure WARNs (`exchange: a peer's copy of a
      stand-in was not taken - asking again later`) and waits for the
      recheck;
    - every row of the video takes the peer's lyrics columns
      (`adopt_lyrics`: the file is the video's), then `fetched` (the
      origin recorded, the wait ended, the stand-in dropped, INFO
      `exchange: done with a peer's copy`) — only once EVERY row took it
      (review round 2): a row whose write failed (WARNed) keeps the
      stand-in, rescheduled, and its next look places the copy into every
      row again;
    - settings that do not hold WARN (`exchange: a stand-in waits - the
      settings do not hold`) and wait for the recheck.

  The peer's copy replaces the stand-in whatever its source: unlike the
  hook's `NothingNewer`, no same-source check (two runs of the same tier
  are not the same track). INFO `exchange: a stand-in is kept for good -
  the peer's copy is not taken` (`why`) for a dropped one; DEBUG `exchange:
  a stand-in waits - …` for one asked again at its recheck. Known limit
  (the hooks' too): an operator's Reprocess clicked during the transfer is
  cleared by the adoption (`mark_video_lyrics_complete` sets the manual
  flag to 0); the window is the transfer, a few seconds. The supersede and
  a local lyrics run never overlap: both run inside the lyrics worker's one
  tick (the supersede first).
  OPEN for phase 2 (SNV listing PP): a stand-in at SNV whose song came from
  PP would take PP's copy whatever its tier, and PP's track is the degraded
  one (no AI proxy at PP); which node's lyrics may replace which is to be
  decided with the phase-2 tie-break ("Kinds, versions, jobs").
- **The same gap elsewhere, checked (design record 6056986290):** the
  metadata repair has none (a failed provider call keeps the row in the
  repair queue, and `peer_title` is asked first on every pass; a provider
  title made here has the peer's rank, 1); the translation is part of the
  lyrics track (adopted and superseded with it; `retry_missing_translations`
  / the retranslate pass rewrite only this node's own track); the dub is
  not exchanged (`dub_worker_enabled` off at PP, a dubbed video's lyrics
  local by design).
- Tests: `queued_tests.rs::lyrics_waiting_on_their_queued_stems_are_queued`,
  `decide_tests.rs::{a_lyrics_job_waits_while_a_peer_has_the_song,
  a_lyrics_job_waits_only_on_the_peer_it_took_the_song_from,
  a_lyrics_job_waits_only_for_the_audio_it_took}`,
  `kind_tests.rs::only_the_lyrics_wait_while_a_peer_has_the_song`,
  `lyrics_tests.rs::{lyrics_wait_while_a_peer_has_the_song,
  lyrics_of_a_song_downloaded_here_do_not_wait_for_a_peer,
  a_stand_in_of_the_same_source_is_replaced_by_the_peers_copy}`,
  `standin_tests.rs` (the supersede over two real nodes: every row, a
  parked track, the check row with an audio, the recheck, another audio,
  an operator or a dub, no row, the order, the recheck curve, every row or
  none over; the stand-in recorded after the bound — its source read or
  unreachable — and after a failing fetch from that source only, kept when
  the run is put back, ended by `run_here` / `fetched`),
  `db/mod_tests_v31.rs` (the back-fill's cases),
  `lyrics/worker_tests_peer.rs::the_worker_replaces_a_stand_in_with_the_peers_copy`.

## PP's workers on (MAIN SESSION OPS, after the release with lanes 7–9 reaches PP)

1. Check PP runs it (`/api/v1/status` version) and its exchange holds:
   `GET /api/v1/exchange/status` → `node_name: pp`, `config_error: null`,
   peer `snv` with `has_key` + `cf_access`; `POST /api/v1/exchange/probe`
   → `ok`.
2. PATCH PP `{"lyrics_worker_enabled":"true","stem_worker_enabled":"true"}`
   (phase 0 had them `false`); leave `dub_worker_enabled` as it is.
3. Watch PP's log for `exchange:` lines and the status's `peers[0].last_read`
   / `jobs`; `SELECT * FROM peer_fetches` / `peer_waits` on PP's DB copy.
4. Live check: a new video in a playlist both sites sync; once SNV has
   processed it, PP logs `exchange: done with a peer's copy` for its
   download, stems and lyrics, and no `stem worker: separating` for it.
5. The PP lyrics lane (after the release that carries it reaches PP): while
   SNV runs the new song's stems and lyrics, PP's lyrics log `exchange: a
   peer will have it, or could not be asked - waiting` with `job=lyrics`
   and `why=PeerRunsIt` (SNV announces lyrics waiting on its stems) or
   `why=PeerHasTheSong`, and NEVER `exchange: no peer has it - processing
   here … job=lyrics` for a song PP fetched from SNV. Then `exchange: done
   with a peer's copy` with `job=lyrics source=peer:snv`. The stand-in V31
   back-filled for `8ohdO2nINEI` is replaced at PP's first lyrics tick
   after the start (`exchange: done with a peer's copy`, `job=lyrics`, for
   that id; `SELECT * FROM peer_standins` empty after it).

## Tests

- `peer::rig::TestNode` = a node with its own in-memory DB, cache dir and a
  real 127.0.0.1 port serving `peer::router`; `give_song` / `give_stems` /
  `give_lyrics` / `hash_now`; `SNV_KEY` / `OTHER_KEY` (fixtures contain
  `example`, never a real key); `counting_chain` = a one-provider metadata
  chain that counts its calls. Two nodes talk over real HTTP
  (`client_tests.rs`, `fetch_tests.rs`, `lan_tests.rs`, `ask_tests.rs`,
  `download_tests.rs`, `stems_tests.rs`, `lyrics_tests.rs`,
  `repair_tests.rs`, `audio_tests.rs`, `transfer_probe_tests.rs`, and the
  workers' `*_tests_peer.rs`). To prove a write happens BEFORE a later
  step, make that step fail: a test trigger `CREATE TRIGGER … BEFORE
  UPDATE ON videos BEGIN SELECT RAISE(ABORT, '…'); END` fails
  `record_download`, and the origin must already be recorded
  (`download_tests::an_adopted_pair_records_its_origin_before_the_row_plays`).
  A guard with two branches needs a test where ONE branch alone decides
  (change PP's bytes so the hash says no while the record says yes, and
  the reverse); a fixture where both agree tests neither.
  `TestNode::audio_from(yt, peer)` records
  this node's audio as fetched from `peer` at the sha every node's
  `give_song` audio has (`rig::song_audio_sha`).
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
