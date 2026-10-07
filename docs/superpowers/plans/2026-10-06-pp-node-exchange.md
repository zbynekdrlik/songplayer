# PP Site Node Exchange (phase 1 + PP deploy) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every SongPlayer node (SNV, PP) serves what it has processed through a key-protected peer API and asks its peers before a heavy job (download+normalize+metadata, lyrics, stems, metadata repair), so no node redoes what another already did; PP gets main releases through its own CI deploy with a post-deploy subset.

**Architecture:** A new `crates/sp-server/src/peer/` module holds one `Exchange` per process (DB pool, cache dir, job board, peer client). The serving side lists this node's artifacts from its own rows plus a persistent sha256 cache (`peer_hashes`, filled by a rate-limited background hasher) and serves files through tower-http `ServeFile` (HTTP Range) behind an `X-SP-Peer-Key` guard and an upload cap. The asking side reads peer catalogs (Cloudflare Access service-token headers, redirects never followed, 60 s cache), decides Fetch / Wait (≤ 2 h, recorded in `peer_waits`) / Local with a pure `decide()`, fetches Range-resumed sha-checked parts into `<cache>/peer/`, and hands them to small per-job adopters that name files from the node's OWN row under `cache::SONG_FILES` (#136). Each worker gets one hook line; with no peers configured (SNV today) the hook answers `Local` with no network and no DB write.

**Tech Stack:** Rust 2024 / tokio / axum 0.8 / tower-http 0.6 `ServeFile` / reqwest 0.12 (rustls, `Response::chunk`) / sqlx sqlite (manual migrations) / sha2 / wiremock (tests) / Playwright (e2e) / GitHub Actions (`workflow_run`, self-hosted Windows runner) / pytest+ruff (workflow checks).

**Spec:** `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md` (approved by the owner 6.10.2026, commit `9715338e`). Owner decisions: issue #229 comments (`gh issue view 229 --comments`). Executors read both.

## Decisions taken after the plan was written (main session, #229, 6.10.2026)

These override the tasks below wherever they differ:
- The catalog lists a peer's QUEUED jobs as well as its running ones (lanes 2/3). `Exchange::ask` waits, bounded at ~2 h, for a peer that has the job queued OR running. Both sites sync the same playlists, so without this PP would process new songs itself.
- An unreachable or refusing peer means wait up to 2 h, then process locally, with a WARN.
- The origin of fetched content goes into `peer_fetches`; the row keeps the peer's real source values.
- The stem fetch runs before the lyrics-venv check (lane 9).
- Phase 0 resets `node_name` and the peer key on PP's copied DB before its first start.
- Lane 2 as built (lane worker, 7.10.2026, #229 comment 6030334867): a catalog's job entry is `wire::CatalogJob { youtube_id, kind, node, state: running|queued, started_at: Option<String> }` and `Catalog::runs` is `Catalog::announces` (a running OR queued job). The later lanes' code below uses these names. `kind::metadata_version` ranks only the chain's `gemini` label with `gemini_failed = 0` as a provider (1): a `regex` title written with no provider configured is a parser's (0).
- Lane 1 masks EVERY secret-class setting in `GET /api/v1/settings` (the existing `gemini_api_key`, `obs_websocket_password`, `remote_ws_password`, the Genius token, and the new peer secrets). One list in `sp_core::config`; a masked PATCH keeps the stored value (#229, gap 10).

## Global Constraints

- **Tier-0 (no local cargo compile).** Locally only `cargo fmt --all` / `cargo fmt --all --check` (then `git checkout -- crates/sp-server/src/db/models.rs` if fmt reordered it, `.claude/rules/rust-workspace.md`), python (`ruff`, `pytest`) and node tooling. Every "Run" step below names the CI test filter; the lane's ONE push is the run. Never `cargo build/test/check/clippy` locally (hook-enforced).
- **Lane start, every lane:** `git fetch origin && git merge origin/dev`; FIRST commit = VERSION bump: next `-dev.N` in `VERSION` (strictly above `git show origin/main:VERSION`), `./scripts/sync-version.sh`, commit `chore(#229): bump version to <X>`; then post the lane's design comment on #229 (`gh issue comment 229 --body-file <file>`: the lane's approach + the rejected alternative + `Architektúra:` line, given in each lane header) BEFORE the first code commit (the design-gate hook reads it).
- **One push per lane** after `cargo fmt --all --check` is clean and `wc -l` of every touched `.rs` is ≤ 1000; then monitor CI to terminal state (all jobs, incl. Deploy to win-resolume + E2E).
- **1000-line cap per `.rs`** (`ci.yml` file-size). Files already near it: `downloader/mod.rs` 993 (Lane 8 moves its tests out first), `stems/worker.rs` 970 (Lane 9 moves its tests out first), `db/models.rs` 982 (never add to it — new queries go to `db/models_peer.rs`), `api/routes.rs` 958 (Lane 1 adds ≤ 10 lines), `lyrics/worker.rs` 901, `lib.rs` 879.
- **Untrusted JSON (a peer's answer, a settings PATCH) only into typed structs** with no `serde_json::Value` inside (`rust-workspace.md` "Untrusted input never goes through serde_json::Value's own Deserialize"). A serde error text is never echoed (it can contain the input): report `line`/`column` only.
- **Never bump `LYRICS_PIPELINE_VERSION`** (owner approval only, CLAUDE.md). This plan only READS it.
- **Secrets** (`peer_api_key`, each peer's `key` and `cf_client_secret`) travel only through the secret channel (`airuleset.py secret …`), are never logged, never in an error text, never in `Debug`, and `GET /api/v1/settings` shows `********`.
- **Never touch win-resolume (SNV) or resolume-pp (PP) from a lane worker.** Every step marked **MAIN SESSION OPS** is done by the main session only, before/after the lane's push as stated. CI's own deploy to SNV is fine.
- **Clippy `-D warnings` on Linux (`--all-targets`)**: no unused test helpers (add a rig helper in the lane that first uses it), `#[must_use]` on the TYPE only, let-chains instead of nested `if let`, `.contains(&x)` not `.iter().any(|y| y == x)`, test-only serial locks are `tokio::sync::Mutex`, no `serde_json::Value` parse of untrusted input.
- **Mutation gate (diff-scoped, `peer/` is NOT excluded):** every strict comparison gets an exact-boundary test; clamps via `.min()/.max()`; no unkillable shapes (no XOR-fold key compare: compare sha256 digests with `==`); a timer loop gets `#[cfg_attr(test, mutants::skip)]` with a reason line.
- **Commits:** `test(#229): …` (tests first, RED) then `feat(#229): …` / `refactor(#229): …` / `docs(#229): …` / `ci(#229): …`; every commit message ends with the session's attribution trailer. Never amend/rebase/force.
- **`peer/mod.rs` module list stays at the TOP and sorted**; `#[cfg(test)] pub(crate) mod rig;` stays the LAST item (never insert a `mod` directly above a `#[cfg(test)]` line, `rust-workspace.md` #192 r5).
- **No behaviour change when no peers are configured** (SNV today, every dev push): the hooks answer `Local` with no network, no `peer_waits` row, no defer.
- **Playlists stay per node** (each node syncs its own membership); only processed content is exchanged (spec "Playlists").

## Review Focus

1. **Cloudflare Access refusing PP's service token** (expired, revoked, missing): the Access app has `auto_redirect_to_identity: true`, so the refusal is a **302 to the login page** (or a 403), never a 401. Expected: `PeerError::AccessRefused`, the redirect never followed, the login HTML never parsed as a catalog, the peer counted as unreadable (jobs wait, bounded), the probe names it. → test in Task 5.1 (`a_cloudflare_login_redirect_is_access_refused_and_never_followed`).
2. **A secret saved back by a client that only ever saw the mask** (GET settings → change one field → PATCH the whole map, the dashboard's own save pattern). Expected: the stored key / Cloudflare secret is kept, never overwritten with `********`. → test in Task 1.3 (`a_settings_map_saved_back_with_masks_keeps_the_secrets`).
3. **A catalog from a newer peer**: an unknown kind (`dub`, phase 2), an entry with a bad YouTube id or a non-hex sha. Expected: those entries skipped, the rest of the catalog used, never a parse failure of the whole catalog. → tests in Task 2.2 (`sanitized_keeps_only_what_this_node_can_use`) and Task 5.1 (`a_newer_peers_catalog_keeps_what_this_node_knows`).
4. **A song renamed on the receiving node while its artifact was in transfer** (an operator's title correction, the metadata repair). Expected: the files land under the row's CURRENT name, read under `cache::SONG_FILES` after the transfer. → test in Task 9.1 (`stems_fetched_during_a_rename_land_under_the_new_name`); a fetched pair is recorded through `record_download`, whose re-read under the lock is pinned by `metadata::manual`'s existing `a_correction_made_during_the_download_names_the_fresh_pair`, and Task 8.1's `an_operator_correction_here_names_the_pair` pins the title choice.
5. **A dub-requested video at the serving node** (its `{yt}_lyrics.json` is the Live-Translate subtitle track, `dabing/subtitles.rs`). Expected: never listed as lyrics, never served as lyrics, never adopted as lyrics. → tests in Task 3.3 (`a_dub_track_is_never_listed_as_lyrics`), Task 4.3 (`the_lyrics_of_a_dubbed_video_are_not_served`), Task 9.2 (`a_track_whose_source_differs_from_the_row_is_refused`).

---

## File Structure

**New (Rust, `crates/sp-server/src/`):**

| File | Responsibility | Lane |
|---|---|---|
| `peer/mod.rs` | `Exchange` (pool, cache dir, board, client), `router()` merging LAN + peer routes, re-exports | 1 (+2,3,4,5,7,8,9) |
| `peer/config.rs` (+`config_tests.rs`) | `PeerConfig`, `NodeConfig` from settings, validation, masking (`shown`, `prepare_settings`, `unmask_peers`), redacting `Debug` | 1 |
| `peer/lan.rs` (+`lan_tests.rs`) | LAN routes: `GET /api/v1/exchange/status`, `POST /api/v1/exchange/probe` | 1 (+3,5) |
| `peer/kind.rs` (+`kind_tests.rs`) | `ArtifactKind`, `Job`, `MEDIA_VERSION`, `STEMS_VERSION`, metadata versions, `acceptable()` | 2 |
| `peer/wire.rs` (+`wire_tests.rs`) | `Artifact`, `CatalogJob` + `JobState` (was `RunningJob`), `Catalog` (+`sanitized`, `announces`, was `runs`), `PeerMetadata`, `PeerLyrics`, `PeerVideo`, time helpers | 2 (+4) |
| `peer/board.rs` (+`board_tests.rs`) | `JobBoard` + `JobGuard` (running jobs, announced while the guard lives) | 2 |
| `peer/hash.rs`, `peer/throttle.rs` (+tests) | sha256 of bytes/files; rate math `wait_for`, `mbps_to_bytes`, `throttled` body | 3 (+4) |
| `peer/catalog.rs` (+`catalog_tests.rs`) | this node's artifact files from its rows, metadata rows, lyrics row, `build()` the catalog, `counts()` | 3 (+4) |
| `peer/hasher.rs` (+`hasher_tests.rs`) | `hash_pass()` (one file at a time, rate-limited, stat-hash-stat) + the 60 s `run()` loop | 3 |
| `peer/rig.rs` | `#[cfg(test)]` two-node rig: `TestNode` = own DB + cache dir + real HTTP port | 3 (+5,9) |
| `peer/api.rs` (+`api_tests.rs`) | `/api/v1/peer/{catalog,videos/{id},artifact/{id}/{kind}}`, key guard, Range via `ServeFile`, upload cap, pause → 503 | 4 |
| `peer/client.rs` (+`client_tests.rs`) | `PeerClient`: catalog/video reads (CF headers, no redirects, bounded body, 60 s cache), `PeerError`, last reads | 5 |
| `peer/fetch.rs` (+`fetch_tests.rs`) | artifact → `<cache>/peer/*.part`, Range resume, sha check, one transfer per peer, `Exchange::fetch` (pause) | 5 |
| `peer/decide.rs` (+`decide_tests.rs`) | pure `decide()` Fetch/Wait/Local, `recheck_after()` | 7 |
| `peer/ask.rs` (+`ask_tests.rs`) | `Exchange::ask/fetch_failed/fetched`, `Ask`, `FetchPlan`, `PeerStep` | 7 |
| `peer/download.rs` (+`download_tests.rs`) | the download job's ask-first + adopt (title choice, pair, `record_download`) | 8 |
| `peer/stems.rs`, `peer/lyrics.rs`, `peer/repair.rs` (+tests) | stems / lyrics / metadata-repair ask-first + adopt | 9 |
| `db/models_peer.rs` (+`models_peer_tests.rs`) | `peer_hashes`, `peer_waits`, `peer_fetches` queries, job defers, `adopt_lyrics` | 3 (+7,9) |
| `db/mod_tests_v29.rs`, `db/mod_tests_v30.rs` | migration tests | 3, 7 |
| `downloader/mod_tests.rs`, `downloader/mod_tests_peer.rs` | downloader tests moved out (cap) + the hook test | 8 |
| `stems/worker_tests.rs`, `stems/worker_tests_peer.rs` | stem worker tests moved out (cap) + the hook test | 9 |
| `lyrics/worker_tests_peer.rs`, `reprocess/tests_peer.rs` | lyrics / repair hook tests | 9 |

**Modified:** `crates/sp-core/src/config.rs` (setting keys), `crates/sp-server/src/lib.rs` (mod, `Exchange`, router merge, hasher spawn, `with_peer` on 4 workers), `api/routes.rs` (settings masking), `db/mod.rs` (V29, V30), `downloader/mod.rs`, `stems/worker.rs`, `lyrics/worker.rs`, `reprocess/mod.rs` (one hook each), `metadata/manual.rs` (`manual_title` → `pub(crate)`).

**New/modified (CI, e2e, docs):** `.github/workflows/deploy-pp.yml` (new), `.github/actionlint.yaml`, `scripts/setup-runner.ps1`, `scripts/tests/test_deploy_pp_workflow.py` (new), `e2e/post-deploy-peer-serving.spec.ts` (new, SNV), `e2e/peer-probe-gate.ts` + `.spec.ts` (new), `e2e/post-deploy-pp.spec.ts` + `e2e/post-deploy-pp.config.ts` (new), `e2e/post-deploy.config.ts` (exclude PP spec), `scripts/cloudflare/README.md` (service token section), `.claude/rules/peer-exchange.md` (new), `CLAUDE.md` (router line).

## Lanes (dispatch serially, in this order)

| Lane | Deliverable | Prod LoC | Depends on |
|---|---|---|---|
| 1 | Node identity + peer settings (masked secrets, `GET /api/v1/exchange/status`, rules file) | ~280 | — |
| 2 | Catalog model (kinds/versions, wire types, job board) | ~230 | 1 |
| 3 | Own catalog + sha256 cache (V29, hasher, status counts, test rig) | ~300 | 2 |
| 4 | Peer API (key guard, catalog/videos/artifact + Range + upload cap) + SNV serving gate | ~230 | 3, ops: SNV key |
| 5 | Peer client (CF Access headers, no redirects, Range-resumed sha-checked fetch, probe) | ~300 | 4, ops: CF token |
| 6 | PP deploy (runner label, `deploy-pp.yml` main-only, PP post-deploy subset) | YAML/TS/PS | 5, ops: PP runner + settings |
| 7 | Ask-first core (pure decision, V30 waits + provenance, announcements) | ~260 | 5 |
| 8 | Download job asks first (fetch-instead-of-process) | ~150 | 7 |
| 9 | Stems, lyrics, metadata repair ask first + enable PP workers | ~290 | 8 |

Lane 6 comes before 7–9 because it only needs the probe (Lane 5) and puts PP under CI early; the ask-first lanes then reach PP with the next main release.

---

## Lane 1 — Node identity + peer settings

Design comment for #229 (lane start): *Approach:* three settings — `node_name`, `peer_api_key` (the key this node's peer API accepts) and `peers` (a JSON list of `{name, base_url, key, cf_client_id?, cf_client_secret?}`) — parsed once into a validated `NodeConfig` read live from the settings table (no restart needed); `GET /api/v1/settings` masks the two secret kinds and a PATCH that sends the mask back keeps the stored value; `GET /api/v1/exchange/status` shows the node, whether it serves, and its peers without secrets. *Rejected:* a separate `peers` table with its own CRUD API — the spec says "stored like other settings", the list is tiny and set by ops through the secret channel; a table would add a migration, routes and a UI for no reader. *Architektúra:* settings table (existing) + `sp_core::config` keys + `peer::config` (pure, serde typed).

### Task 1.1: Exchange setting keys in sp-core

**Files:**
- Modify: `crates/sp-core/src/config.rs` (after `video_hw_decode`, ~line 122; tests appended to its `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS, SETTING_PEER_TRANSFERS_PAUSED, SETTING_PEER_SERVE_MAX_MBPS, DEFAULT_PEER_SERVE_MAX_MBPS: u32 = 20, MAX_PEER_SERVE_MAX_MBPS: u32 = 10_000}`, `fn peer_transfers_paused(raw: Option<&str>) -> bool`, `fn peer_serve_max_mbps(raw: Option<&str>) -> u32`.

- [ ] **Step 1: Write the failing tests** (append inside `mod tests` of `crates/sp-core/src/config.rs`)

```rust
    #[test]
    fn exchange_setting_keys() {
        assert_eq!(SETTING_NODE_NAME, "node_name");
        assert_eq!(SETTING_PEER_API_KEY, "peer_api_key");
        assert_eq!(SETTING_PEERS, "peers");
        assert_eq!(SETTING_PEER_TRANSFERS_PAUSED, "peer_transfers_paused");
        assert_eq!(SETTING_PEER_SERVE_MAX_MBPS, "peer_serve_max_mbps");
    }

    /// #229: peer transfers pause only on an explicit "true"; a missing or
    /// mangled value keeps them running.
    #[test]
    fn peer_transfers_pause_only_on_true() {
        assert!(!peer_transfers_paused(None));
        assert!(peer_transfers_paused(Some("true")));
        assert!(peer_transfers_paused(Some(" true\n")), "trimmed");
        assert!(!peer_transfers_paused(Some("TRUE")), "only the exact word");
        assert!(!peer_transfers_paused(Some("1")));
        assert!(!peer_transfers_paused(Some("")));
    }

    /// #229: what this node sends to peers is capped at 1..=10000 Mbit/s;
    /// anything else reads as the 20 Mbit/s default.
    #[test]
    fn peer_serve_cap_is_1_to_10000_mbps_else_20() {
        assert_eq!(peer_serve_max_mbps(None), 20);
        assert_eq!(peer_serve_max_mbps(Some("50")), 50);
        assert_eq!(peer_serve_max_mbps(Some(" 7 ")), 7);
        assert_eq!(peer_serve_max_mbps(Some("1")), 1);
        assert_eq!(peer_serve_max_mbps(Some("10000")), 10_000);
        assert_eq!(peer_serve_max_mbps(Some("10001")), 20);
        assert_eq!(peer_serve_max_mbps(Some("0")), 20);
        assert_eq!(peer_serve_max_mbps(Some("-1")), 20);
        assert_eq!(peer_serve_max_mbps(Some("fast")), 20);
    }
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-core config::tests` — Expected now: FAIL to compile (`cannot find value SETTING_NODE_NAME`). Locally: `cargo fmt --all --check`.

- [ ] **Step 3: Implement** (insert after the `video_hw_decode` fn)

```rust
// #229: the node exchange — SongPlayer sites (SNV, PP) share processed content.
/// This node's name in the exchange (`snv`, `pp`); empty = the exchange is off.
pub const SETTING_NODE_NAME: &str = "node_name";
/// The key this node's peer API accepts (`X-SP-Peer-Key`); empty = not serving.
pub const SETTING_PEER_API_KEY: &str = "peer_api_key";
/// The peers this node asks before a heavy job: a JSON list (sp-server `peer::config`).
pub const SETTING_PEERS: &str = "peers";
/// "true" stops new peer transfers both ways (an operator's pause, later #230's).
pub const SETTING_PEER_TRANSFERS_PAUSED: &str = "peer_transfers_paused";
/// The most this node SENDS to its peers, in Mbit/s (its uplink also carries the live stream).
pub const SETTING_PEER_SERVE_MAX_MBPS: &str = "peer_serve_max_mbps";
pub const DEFAULT_PEER_SERVE_MAX_MBPS: u32 = 20;
pub const MAX_PEER_SERVE_MAX_MBPS: u32 = 10_000;

/// #229: transfers pause only when the setting says exactly "true".
pub fn peer_transfers_paused(raw: Option<&str>) -> bool {
    raw.map(str::trim) == Some("true")
}

/// #229: the upload cap in Mbit/s: a whole number in 1..=10000, else the default.
pub fn peer_serve_max_mbps(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| (1..=MAX_PEER_SERVE_MAX_MBPS).contains(v))
        .unwrap_or(DEFAULT_PEER_SERVE_MAX_MBPS)
}
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-core config::tests` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sp-core/src/config.rs
git commit -m "feat(#229): exchange setting keys (node_name, peer_api_key, peers, pause, upload cap)"
```

### Task 1.2: `peer::config` — validated node config, masking, redacting Debug

**Files:**
- Create: `crates/sp-server/src/peer/mod.rs` (module skeleton; `Exchange` comes in Task 1.4)
- Create: `crates/sp-server/src/peer/config.rs`
- Create: `crates/sp-server/src/peer/config_tests.rs`
- Modify: `crates/sp-server/src/lib.rs` (add `pub mod peer;` between `pub mod panic_hook;`/`pub mod playback;`/`pub mod playlist;` and `pub mod presenter;`, alphabetical)

**Interfaces:**
- Consumes: Task 1.1 keys; `crate::db::models::{get_setting, set_setting}`.
- Produces (`crate::peer::config`): `MASK: &str = "********"`, `MIN_KEY_LEN: usize = 32`, `MAX_NAME_LEN: usize = 32`; `struct PeerConfig { name: String, base_url: String, key: String, cf_client_id: Option<String>, cf_client_secret: Option<String> }` (+ `fn masked(&self) -> Self`, redacting `Debug`, serde); `struct NodeConfig { node_name: Option<String>, serve_key: Option<String>, peers: Vec<PeerConfig> }` with `fn from_settings(Option<&str>, Option<&str>, Option<&str>) -> Result<Self, String>`, `async fn load(&SqlitePool) -> Result<Self, String>`, `fn serving(&self) -> bool`, `fn asking(&self) -> bool`, `fn peer(&self, name: &str) -> Option<&PeerConfig>`; `fn shown(key: &str, value: String) -> String`; `async fn prepare_settings(&SqlitePool, &HashMap<String, String>) -> Result<Vec<(String, String)>, String>`; `fn unmask_peers(Vec<PeerConfig>, &[PeerConfig]) -> Result<Vec<PeerConfig>, String>`.

- [ ] **Step 1: Create the module skeleton** `crates/sp-server/src/peer/mod.rs`

```rust
//! #229: the node exchange. Every SongPlayer node (SNV, PP) serves what it has
//! processed and asks its peers before a heavy job, so no node redoes what
//! another already did (`.claude/rules/peer-exchange.md`, spec
//! `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`).

pub mod config;
```

and in `crates/sp-server/src/lib.rs` add (alphabetical, after `pub mod panic_hook;`/`pub mod playback;`/`pub mod playlist;`):

```rust
pub mod peer; // #229: the node exchange (serve what this node has, ask peers first)
```

- [ ] **Step 2: Write the failing tests** `crates/sp-server/src/peer/config_tests.rs`

```rust
//! #229 `peer::config`: validation, masking, the redacting Debug.
//! (Repo convention: names the parent only imports are imported explicitly.)

use super::*;
use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS};

/// Exactly `MIN_KEY_LEN` (32) characters.
const KEY: &str = "0123456789abcdef0123456789abcdef";

fn peer(name: &str) -> PeerConfig {
    PeerConfig {
        name: name.into(),
        base_url: "https://sp.newlevel.media".into(),
        key: KEY.into(),
        cf_client_id: Some("client-id.access".into()),
        cf_client_secret: Some("cf-secret-value".into()),
    }
}

fn list(peers: &[PeerConfig]) -> String {
    serde_json::to_string(peers).unwrap()
}

#[test]
fn no_settings_is_an_exchange_that_is_off() {
    let c = NodeConfig::from_settings(None, None, None).unwrap();
    assert_eq!(c, NodeConfig::default());
    assert!(!c.serving());
    assert!(!c.asking());
}

#[test]
fn a_named_node_with_its_key_serves_and_with_peers_asks() {
    let c = NodeConfig::from_settings(Some(" pp "), Some(KEY), Some(&list(&[peer("snv")]))).unwrap();
    assert_eq!(c.node_name.as_deref(), Some("pp"));
    assert_eq!(c.serve_key.as_deref(), Some(KEY));
    assert!(c.serving());
    assert!(c.asking());
    assert_eq!(c.peers, vec![peer("snv")]);
    assert_eq!(c.peer("snv"), Some(&peer("snv")));
    assert_eq!(c.peer("xx"), None);
}

#[test]
fn without_a_node_name_nothing_serves_or_asks() {
    let c = NodeConfig::from_settings(Some(""), Some(KEY), Some(&list(&[peer("snv")]))).unwrap();
    assert!(!c.serving());
    assert!(!c.asking());
}

#[test]
fn a_node_with_a_name_and_no_peers_does_not_ask() {
    let c = NodeConfig::from_settings(Some("snv"), None, Some("[]")).unwrap();
    assert!(!c.asking());
    assert!(!c.serving());
}

#[test]
fn a_name_is_1_to_32_of_lowercase_digits_and_dashes() {
    assert!(valid_name("pp"));
    assert!(valid_name("snv-2"));
    assert!(valid_name(&"a".repeat(MAX_NAME_LEN)));
    assert!(!valid_name(""));
    assert!(!valid_name(&"a".repeat(MAX_NAME_LEN + 1)));
    assert!(!valid_name("PP"));
    assert!(!valid_name("p p"));
    assert!(!valid_name("p_p"));
    assert!(!valid_name("../x"));
    assert!(NodeConfig::from_settings(Some("PP"), None, None).is_err());
}

#[test]
fn a_serving_key_has_at_least_32_characters() {
    let short = &KEY[..MIN_KEY_LEN - 1];
    let err = NodeConfig::from_settings(Some("snv"), Some(short), None).unwrap_err();
    assert!(!err.contains(short), "the key never appears in an error: {err}");
    assert!(NodeConfig::from_settings(Some("snv"), Some(KEY), None).unwrap().serving());
}

#[test]
fn a_base_url_is_http_or_https_with_a_host() {
    assert!(valid_base_url("https://sp.newlevel.media"));
    assert!(valid_base_url("http://10.77.9.201:8920"));
    assert!(valid_base_url("https://host/sub/"));
    assert!(!valid_base_url("ftp://host"));
    assert!(!valid_base_url("https://"));
    assert!(!valid_base_url("https:///path"));
    assert!(!valid_base_url("https://host?x=1"));
    assert!(!valid_base_url("https://host#f"));
    assert!(!valid_base_url("https://ho st"));
}

#[test]
fn every_peer_list_rule_is_enforced() {
    let own = Some("pp");
    let bad_name = PeerConfig { name: "SNV".into(), ..peer("snv") };
    let bad_url = PeerConfig { base_url: "sp.newlevel.media".into(), ..peer("snv") };
    let short_key = PeerConfig { key: KEY[..MIN_KEY_LEN - 1].into(), ..peer("snv") };
    let half_cf = PeerConfig { cf_client_secret: None, ..peer("snv") };
    assert!(validate_peers(&[bad_name], own).is_err());
    assert!(validate_peers(&[peer("pp")], own).is_err(), "a peer named like this node");
    assert!(validate_peers(&[peer("snv"), peer("snv")], own).is_err(), "a peer twice");
    assert!(validate_peers(&[bad_url], own).is_err());
    assert!(validate_peers(&[short_key], own).is_err());
    assert!(validate_peers(&[half_cf], own).is_err());
    let no_cf = PeerConfig { cf_client_id: None, cf_client_secret: None, ..peer("snv") };
    assert!(validate_peers(&[peer("snv"), no_cf], own).is_err(), "still twice");
    assert!(validate_peers(&[peer("snv")], own).is_ok());
    assert!(validate_peers(&[peer("snv")], None).is_ok());
}

#[test]
fn a_peer_list_error_never_echoes_the_setting() {
    let raw = "\"secret-value-that-must-not-leak\"";
    let err = NodeConfig::from_settings(Some("pp"), None, Some(raw)).unwrap_err();
    assert!(!err.contains("secret-value"), "{err}");
    assert!(err.contains("line 1"), "{err}");
}

#[test]
fn debug_never_prints_a_secret() {
    let d = format!("{:?}", peer("snv"));
    assert!(!d.contains(KEY), "{d}");
    assert!(!d.contains("cf-secret-value"), "{d}");
    assert!(d.contains("client-id.access"), "the client id is not a secret: {d}");
    assert!(d.contains(MASK), "{d}");
    let c = NodeConfig::from_settings(Some("snv"), Some(KEY), None).unwrap();
    assert!(!format!("{c:?}").contains(KEY));
}

#[test]
fn shown_masks_the_secrets_and_nothing_else() {
    assert_eq!(shown(SETTING_PEER_API_KEY, KEY.into()), MASK);
    assert_eq!(shown(SETTING_PEER_API_KEY, String::new()), "");
    assert_eq!(shown("gemini_model", "m-1".into()), "m-1");
    assert_eq!(shown(SETTING_PEERS, String::new()), "");
    assert_eq!(shown(SETTING_PEERS, "not json".into()), MASK);
    let back: Vec<PeerConfig> = serde_json::from_str(&shown(SETTING_PEERS, list(&[peer("snv")]))).unwrap();
    assert_eq!(back[0].key, MASK);
    assert_eq!(back[0].cf_client_secret.as_deref(), Some(MASK));
    assert_eq!(back[0].cf_client_id.as_deref(), Some("client-id.access"));
    assert_eq!(back[0].base_url, "https://sp.newlevel.media");
}

#[test]
fn unmask_takes_the_stored_secret_of_the_same_peer() {
    let stored = vec![peer("snv")];
    let sent = PeerConfig { base_url: "https://other.example".into(), ..peer("snv").masked() };
    let merged = unmask_peers(vec![sent], &stored).unwrap();
    assert_eq!(merged[0].key, KEY);
    assert_eq!(merged[0].cf_client_secret.as_deref(), Some("cf-secret-value"));
    assert_eq!(merged[0].base_url, "https://other.example");
    assert!(unmask_peers(vec![peer("pp").masked()], &stored).is_err(), "no stored peer of that name");
    let fresh = unmask_peers(vec![peer("snv")], &[]).unwrap();
    assert_eq!(fresh[0].key, KEY, "a key sent in clear is taken as sent");
}
```

and at the bottom of `config.rs` (Step 4) the hook `#[cfg(test)] #[path = "config_tests.rs"] mod tests;`.

- [ ] **Step 3: Run (CI only):** `cargo test -p sp-server peer::config` — Expected now: FAIL to compile (`cannot find type PeerConfig`).

- [ ] **Step 4: Implement** `crates/sp-server/src/peer/config.rs`

```rust
//! #229: this node's place in the node exchange, read live from its settings.
//!
//! - `node_name` (`snv`, `pp`): empty = the exchange is off (no serving, no asking).
//! - `peer_api_key`: the key this node's peer API accepts (`X-SP-Peer-Key`);
//!   empty = the peer API answers 404.
//! - `peers`: a JSON list of [`PeerConfig`] — the nodes asked first.
//!
//! The secrets (keys, a Cloudflare Access client secret) never leave the node
//! in clear: `GET /api/v1/settings` shows [`MASK`] ([`shown`]), a PATCH that
//! sends the mask back keeps the stored value ([`prepare_settings`]), `Debug`
//! prints the mask, and no error text echoes a setting.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS};
use sqlx::SqlitePool;

/// What a secret reads as outside the node.
pub const MASK: &str = "********";
/// The shortest key a node serves with or sends (`openssl rand -hex 32` gives 64).
pub const MIN_KEY_LEN: usize = 32;
/// The longest node or peer name.
pub const MAX_NAME_LEN: usize = 32;

/// One peer this node asks first.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerConfig {
    /// The peer's `node_name` (`snv`).
    pub name: String,
    /// The peer's SongPlayer, e.g. `https://sp.newlevel.media`.
    pub base_url: String,
    /// The peer's `peer_api_key`, sent as `X-SP-Peer-Key`.
    #[serde(default)]
    pub key: String,
    /// A Cloudflare Access service token for a peer behind Access (both or neither).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_client_secret: Option<String>,
}

impl PeerConfig {
    /// This peer with its secrets shown as [`MASK`].
    pub fn masked(&self) -> Self {
        Self {
            key: mask(&self.key).to_string(),
            cf_client_secret: self.cf_client_secret.as_deref().map(|s| mask(s).to_string()),
            ..self.clone()
        }
    }
}

impl fmt::Debug for PeerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.masked();
        f.debug_struct("PeerConfig")
            .field("name", &m.name)
            .field("base_url", &m.base_url)
            .field("key", &m.key)
            .field("cf_client_id", &m.cf_client_id)
            .field("cf_client_secret", &m.cf_client_secret)
            .finish()
    }
}

/// `""` stays `""`; any other secret reads as [`MASK`].
fn mask(secret: &str) -> &'static str {
    if secret.is_empty() { "" } else { MASK }
}

/// This node's exchange settings, validated.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct NodeConfig {
    pub node_name: Option<String>,
    /// The key this node's peer API accepts; `None` = not serving.
    pub serve_key: Option<String>,
    pub peers: Vec<PeerConfig>,
}

impl fmt::Debug for NodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeConfig")
            .field("node_name", &self.node_name)
            .field("serve_key", &self.serve_key.as_deref().map(mask))
            .field("peers", &self.peers)
            .finish()
    }
}

impl NodeConfig {
    /// The config the three settings hold, or why they do not hold.
    pub fn from_settings(
        node_name: Option<&str>,
        serve_key: Option<&str>,
        peers: Option<&str>,
    ) -> Result<Self, String> {
        let node_name = match node_name.map(str::trim).filter(|n| !n.is_empty()) {
            None => None,
            Some(n) if valid_name(n) => Some(n.to_string()),
            Some(n) => return Err(format!("node_name {n:?} must be 1-32 of a-z, 0-9 and -")),
        };
        let serve_key = match serve_key.map(str::trim).filter(|k| !k.is_empty()) {
            None => None,
            Some(k) if k.len() >= MIN_KEY_LEN => Some(k.to_string()),
            Some(_) => return Err(format!("peer_api_key must have at least {MIN_KEY_LEN} characters")),
        };
        let peers = parse_peers(peers)?;
        validate_peers(&peers, node_name.as_deref())?;
        Ok(Self { node_name, serve_key, peers })
    }

    /// The config as the settings table holds it now.
    pub async fn load(pool: &SqlitePool) -> Result<Self, String> {
        let node_name = setting(pool, SETTING_NODE_NAME).await?;
        let serve_key = setting(pool, SETTING_PEER_API_KEY).await?;
        let peers = setting(pool, SETTING_PEERS).await?;
        Self::from_settings(node_name.as_deref(), serve_key.as_deref(), peers.as_deref())
    }

    /// The peer API answers (a name and a key).
    pub fn serving(&self) -> bool {
        self.node_name.is_some() && self.serve_key.is_some()
    }

    /// A heavy job asks peers first (a name and at least one peer).
    pub fn asking(&self) -> bool {
        self.node_name.is_some() && !self.peers.is_empty()
    }

    pub fn peer(&self, name: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| p.name == name)
    }
}

async fn setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, String> {
    crate::db::models::get_setting(pool, key)
        .await
        .map_err(|e| format!("reading {key} failed: {e}"))
}

/// A node or peer name: 1-32 of `a-z`, `0-9`, `-`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `http(s)://host[/path]`, no query, no fragment, no whitespace.
fn valid_base_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty()
        && !rest.contains('?')
        && !rest.contains('#')
        && !url.contains(char::is_whitespace)
}

/// The peer list's rules; an error names the peer, never a secret.
fn validate_peers(peers: &[PeerConfig], own: Option<&str>) -> Result<(), String> {
    let mut seen = HashSet::new();
    for p in peers {
        if !valid_name(&p.name) {
            return Err(format!("peer name {:?} must be 1-32 of a-z, 0-9 and -", p.name));
        }
        if own == Some(p.name.as_str()) {
            return Err(format!("peer {} has this node's own name", p.name));
        }
        if !seen.insert(p.name.as_str()) {
            return Err(format!("peer {} is listed twice", p.name));
        }
        if !valid_base_url(&p.base_url) {
            return Err(format!("peer {}: base_url must be http(s)://host[/path]", p.name));
        }
        if p.key.len() < MIN_KEY_LEN {
            return Err(format!("peer {}: key must have at least {MIN_KEY_LEN} characters", p.name));
        }
        if p.cf_client_id.is_some() != p.cf_client_secret.is_some() {
            return Err(format!("peer {}: cf_client_id and cf_client_secret go together", p.name));
        }
    }
    Ok(())
}

/// The `peers` setting as a list; an empty setting is no peer. A parse error
/// names only the position (serde's text can quote the input).
fn parse_peers(raw: Option<&str>) -> Result<Vec<PeerConfig>, String> {
    match raw.map(str::trim).filter(|r| !r.is_empty()) {
        None => Ok(Vec::new()),
        Some(r) => serde_json::from_str(r).map_err(|e| {
            format!("peers is not a peer list (line {}, column {})", e.line(), e.column())
        }),
    }
}

/// The value `GET /api/v1/settings` shows for `key`: a secret masked, the rest as stored.
pub fn shown(key: &str, value: String) -> String {
    if value.trim().is_empty() {
        return value;
    }
    match key {
        SETTING_PEER_API_KEY => MASK.to_string(),
        SETTING_PEERS => match parse_peers(Some(&value)) {
            Ok(peers) => {
                let masked: Vec<PeerConfig> = peers.iter().map(PeerConfig::masked).collect();
                serde_json::to_string(&masked).unwrap_or_else(|_| MASK.to_string())
            }
            Err(_) => MASK.to_string(),
        },
        _ => value,
    }
}

/// `incoming` with every masked secret taken from the stored peer of the same name.
pub fn unmask_peers(incoming: Vec<PeerConfig>, stored: &[PeerConfig]) -> Result<Vec<PeerConfig>, String> {
    incoming
        .into_iter()
        .map(|mut p| {
            let old = stored.iter().find(|s| s.name == p.name);
            if p.key == MASK {
                p.key = old
                    .map(|o| o.key.clone())
                    .ok_or_else(|| format!("peer {}: a masked key, but no stored peer of that name", p.name))?;
            }
            if p.cf_client_secret.as_deref() == Some(MASK) {
                let secret = old.and_then(|o| o.cf_client_secret.clone());
                p.cf_client_secret = Some(secret.ok_or_else(|| {
                    format!("peer {}: a masked cf_client_secret, but none stored", p.name)
                })?);
            }
            Ok(p)
        })
        .collect()
}

/// The writes a settings PATCH makes, `(key, value)` in key order. A secret
/// sent back as [`MASK`] keeps the stored one (it is not written); an exchange
/// setting that does not hold refuses the whole PATCH (nothing is written).
/// Every other key passes as sent.
pub async fn prepare_settings(
    pool: &SqlitePool,
    incoming: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut keys: Vec<&String> = incoming.keys().collect();
    keys.sort();
    let mut writes = Vec::new();
    for key in keys {
        let value = &incoming[key];
        match key.as_str() {
            SETTING_PEER_API_KEY if value == MASK => continue,
            SETTING_PEER_API_KEY => {
                let k = value.trim();
                if !k.is_empty() && k.len() < MIN_KEY_LEN {
                    return Err(format!("peer_api_key must have at least {MIN_KEY_LEN} characters"));
                }
                writes.push((key.clone(), k.to_string()));
            }
            SETTING_NODE_NAME => {
                let n = value.trim();
                if !n.is_empty() && !valid_name(n) {
                    return Err(format!("node_name {n:?} must be 1-32 of a-z, 0-9 and -"));
                }
                if !incoming.contains_key(SETTING_PEERS) {
                    let stored = parse_peers(setting(pool, SETTING_PEERS).await?.as_deref())
                        .unwrap_or_default();
                    if stored.iter().any(|p| p.name == n) {
                        return Err(format!("node_name {n} is a peer's name"));
                    }
                }
                writes.push((key.clone(), n.to_string()));
            }
            SETTING_PEERS => {
                let stored =
                    parse_peers(setting(pool, SETTING_PEERS).await?.as_deref()).unwrap_or_default();
                let merged = unmask_peers(parse_peers(Some(value))?, &stored)?;
                let own = match incoming.get(SETTING_NODE_NAME) {
                    Some(n) => Some(n.trim().to_string()),
                    None => setting(pool, SETTING_NODE_NAME).await?,
                };
                validate_peers(&merged, own.as_deref().filter(|n| !n.is_empty()))?;
                let json = serde_json::to_string(&merged).map_err(|e| e.to_string())?;
                writes.push((key.clone(), json));
            }
            _ => writes.push((key.clone(), value.clone())),
        }
    }
    Ok(writes)
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
```

- [ ] **Step 5: Run (CI only):** `cargo test -p sp-server peer::config` — Expected: PASS (13 tests).

- [ ] **Step 6: Commit** — first `git add crates/sp-server/src/peer/config_tests.rs && git commit -m "test(#229): peer settings validation, masking and redaction"`, then `git add crates/sp-server/src/peer crates/sp-server/src/lib.rs && git commit -m "feat(#229): peer::config — validated node config, masked secrets"`.

### Task 1.3: `GET/PATCH /api/v1/settings` mask the peer secrets

**Files:**
- Modify: `crates/sp-server/src/api/routes.rs` (`get_settings` ~line 650, `update_settings` ~line 664)
- Test: `crates/sp-server/src/peer/config_tests.rs` (append; uses the real router `crate::api::routes::tests::{test_state, app}`, which is `pub(crate)`)

**Interfaces:**
- Consumes: `peer::config::{shown, prepare_settings, MASK, PeerConfig}` (Task 1.2).
- Produces: `GET /api/v1/settings` never returns a peer secret in clear; `PATCH /api/v1/settings` answers **400 + reason text** when an exchange setting does not hold, and writes nothing then.

- [ ] **Step 1: Write the failing tests** (append to `config_tests.rs`)

```rust
mod settings_route {
    use super::*;
    use crate::api::routes::tests::{app, test_state};
    use std::collections::HashMap;
    use sqlx::SqlitePool;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn get_all(state: crate::AppState) -> HashMap<String, String> {
        let req = Request::builder().uri("/api/v1/settings").body(Body::empty()).unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn patch(state: crate::AppState, map: &HashMap<String, String>) -> StatusCode {
        let req = Request::builder()
            .method("PATCH")
            .uri("/api/v1/settings")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(map).unwrap()))
            .unwrap();
        app(state).oneshot(req).await.unwrap().status()
    }

    async fn stored(pool: &SqlitePool, key: &str) -> Option<String> {
        crate::db::models::get_setting(pool, key).await.unwrap()
    }

    async fn with_secrets() -> crate::AppState {
        let state = test_state().await;
        crate::db::models::set_setting(&state.pool, SETTING_NODE_NAME, "pp").await.unwrap();
        crate::db::models::set_setting(&state.pool, SETTING_PEER_API_KEY, KEY).await.unwrap();
        crate::db::models::set_setting(&state.pool, SETTING_PEERS, &list(&[peer("snv")]))
            .await
            .unwrap();
        state
    }

    #[tokio::test]
    async fn get_settings_masks_the_peer_secrets() {
        let state = with_secrets().await;
        let all = get_all(state).await;
        let text = serde_json::to_string(&all).unwrap();
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("cf-secret-value"), "{text}");
        assert_eq!(all[SETTING_PEER_API_KEY], MASK);
        assert!(all[SETTING_PEERS].contains("sp.newlevel.media"));
        assert_eq!(all[SETTING_NODE_NAME], "pp");
    }

    /// Review Focus 2: the dashboard's save pattern — read the map, change
    /// one field, send the whole map back with the masks.
    #[tokio::test]
    async fn a_settings_map_saved_back_with_masks_keeps_the_secrets() {
        let state = with_secrets().await;
        let mut all = get_all(state.clone()).await;
        all.insert("gemini_model".into(), "model-x".into());
        assert_eq!(patch(state.clone(), &all).await, StatusCode::NO_CONTENT);
        assert_eq!(stored(&state.pool, SETTING_PEER_API_KEY).await.as_deref(), Some(KEY));
        let peers: Vec<PeerConfig> =
            serde_json::from_str(&stored(&state.pool, SETTING_PEERS).await.unwrap()).unwrap();
        assert_eq!(peers, vec![peer("snv")]);
        assert_eq!(stored(&state.pool, "gemini_model").await.as_deref(), Some("model-x"));
    }

    #[tokio::test]
    async fn a_bad_peer_list_is_refused_and_nothing_is_written() {
        let state = test_state().await;
        let mut map = HashMap::new();
        map.insert("gemini_model".to_string(), "model-y".to_string());
        map.insert(SETTING_PEERS.to_string(), "[{\"name\":\"SNV\"}]".to_string());
        assert_eq!(patch(state.clone(), &map).await, StatusCode::BAD_REQUEST);
        assert_eq!(stored(&state.pool, "gemini_model").await, None);
        assert_eq!(stored(&state.pool, SETTING_PEERS).await, None);
    }

    #[tokio::test]
    async fn plain_settings_pass_as_before() {
        let state = test_state().await;
        let mut map = HashMap::new();
        map.insert("cache_dir".to_string(), "/tmp/cache".to_string());
        assert_eq!(patch(state.clone(), &map).await, StatusCode::NO_CONTENT);
        assert_eq!(get_all(state).await["cache_dir"], "/tmp/cache");
    }
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::config::tests::settings_route` — Expected before Step 3: `get_settings_masks_the_peer_secrets` FAILS (the key comes back in clear), `a_bad_peer_list_is_refused…` FAILS (204).

- [ ] **Step 3: Implement** in `crates/sp-server/src/api/routes.rs`

In `get_settings`, replace `map.insert(key, serde_json::Value::String(value));` with:

```rust
                // #229: a peer secret is shown masked (`peer::config::shown`).
                let value = crate::peer::config::shown(&key, value);
                map.insert(key, serde_json::Value::String(value));
```

Replace the body of `update_settings` with:

```rust
    // #229: a secret sent back masked keeps the stored one; an exchange
    // setting that does not hold refuses the whole PATCH (nothing written).
    let writes = match crate::peer::config::prepare_settings(&state.pool, &body.settings).await {
        Ok(writes) => writes,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    for (key, value) in &writes {
        if let Err(e) = crate::db::models::set_setting(&state.pool, key, value).await {
            warn!("update_settings error for key {key}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::NO_CONTENT.into_response()
```

Run `cargo fmt --all` then `wc -l crates/sp-server/src/api/routes.rs` (must be ≤ 1000; expected ~966).

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::config` and `cargo test -p sp-server api::routes::tests::settings` — Expected: PASS (the existing `settings_patch_and_get` / `settings_get_empty` unchanged).

- [ ] **Step 5: Commit** — `test(#229): settings route masks peer secrets, keeps them on a masked save` (tests) then `feat(#229): GET/PATCH /api/v1/settings mask the peer secrets` (routes.rs).

### Task 1.4: `Exchange` + `GET /api/v1/exchange/status` + rules file

**Files:**
- Modify: `crates/sp-server/src/peer/mod.rs`
- Create: `crates/sp-server/src/peer/lan.rs`, `crates/sp-server/src/peer/lan_tests.rs`
- Modify: `crates/sp-server/src/lib.rs` (construct `Exchange` after `let state = AppState {…};`, merge the router at "11. Axum HTTP server")
- Create: `.claude/rules/peer-exchange.md`
- Modify: `CLAUDE.md` (router line)

**Interfaces:**
- Consumes: `NodeConfig::load` (Task 1.2), `sp_core::config::{SETTING_PEER_TRANSFERS_PAUSED, peer_transfers_paused}`.
- Produces: `crate::peer::Exchange { pub(crate) pool: SqlitePool, pub(crate) cache_dir: PathBuf }`, `Exchange::new(SqlitePool, PathBuf) -> Arc<Exchange>`, `crate::peer::router(Arc<Exchange>) -> axum::Router`; `crate::peer::lan::{ExchangeStatus { node_name: Option<String>, serving: bool, transfers_paused: bool, config_error: Option<String>, peers: Vec<PeerStatus> }, PeerStatus { name, base_url, has_key: bool, cf_access: bool }, router, status}`. Later lanes ADD fields to both structs.

- [ ] **Step 1: Write the failing tests** `crates/sp-server/src/peer/lan_tests.rs`

```rust
//! #229 `GET /api/v1/exchange/status`.

use super::*;
use crate::peer::Exchange;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::config::{SETTING_NODE_NAME, SETTING_PEERS, SETTING_PEER_TRANSFERS_PAUSED};
use std::sync::Arc;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

async fn exchange() -> (Arc<Exchange>, tempfile::TempDir) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    (Exchange::new(pool, dir.path().to_path_buf()), dir)
}

async fn get_status(ex: &Arc<Exchange>) -> (StatusCode, String) {
    let req = Request::builder().uri("/api/v1/exchange/status").body(Body::empty()).unwrap();
    let resp = crate::peer::router(ex.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn status_shows_the_node_and_its_peers_without_secrets() {
    let (ex, _dir) = exchange().await;
    let peers = format!(
        "[{{\"name\":\"snv\",\"base_url\":\"https://sp.newlevel.media\",\"key\":\"{KEY}\",\
         \"cf_client_id\":\"id.access\",\"cf_client_secret\":\"cf-secret-value\"}}]"
    );
    crate::db::models::set_setting(&ex.pool, SETTING_NODE_NAME, "pp").await.unwrap();
    crate::db::models::set_setting(&ex.pool, SETTING_PEERS, &peers).await.unwrap();
    crate::db::models::set_setting(&ex.pool, SETTING_PEER_TRANSFERS_PAUSED, "true").await.unwrap();
    let (code, body) = get_status(&ex).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.contains(KEY) && !body.contains("cf-secret-value"), "{body}");
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert_eq!(s.node_name.as_deref(), Some("pp"));
    assert!(!s.serving, "no peer_api_key here");
    assert!(s.transfers_paused);
    assert_eq!(s.config_error, None);
    assert_eq!(
        s.peers,
        vec![PeerStatus {
            name: "snv".into(),
            base_url: "https://sp.newlevel.media".into(),
            has_key: true,
            cf_access: true,
        }]
    );
}

#[tokio::test]
async fn status_names_a_setting_that_does_not_hold() {
    let (ex, _dir) = exchange().await;
    crate::db::models::set_setting(&ex.pool, SETTING_NODE_NAME, "PP").await.unwrap();
    let (_, body) = get_status(&ex).await;
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert!(s.config_error.is_some());
    assert_eq!(s.node_name, None);
    assert!(!s.serving);
    assert!(!s.transfers_paused);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::lan` — Expected now: FAIL to compile (`cannot find Exchange`).

- [ ] **Step 3: Implement.** `crates/sp-server/src/peer/mod.rs` becomes:

```rust
//! #229: the node exchange. Every SongPlayer node (SNV, PP) serves what it has
//! processed and asks its peers before a heavy job, so no node redoes what
//! another already did (`.claude/rules/peer-exchange.md`, spec
//! `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`).

pub mod config;
pub mod lan;

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;

/// This node in the exchange: its DB and cache dir (later: its job board and
/// its peer client). One per process, built by `lib.rs`.
pub struct Exchange {
    pub(crate) pool: SqlitePool,
    pub(crate) cache_dir: PathBuf,
}

impl Exchange {
    pub fn new(pool: SqlitePool, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { pool, cache_dir })
    }
}

/// Every route of the exchange; `lib.rs` merges it into the app's router.
pub fn router(ex: Arc<Exchange>) -> axum::Router {
    lan::router(ex)
}
```

`crates/sp-server/src/peer/lan.rs`:

```rust
//! #229: this node's exchange on the LAN API (no peer key):
//! `GET /api/v1/exchange/status`.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sp_core::config::{SETTING_PEER_TRANSFERS_PAUSED, peer_transfers_paused};

use super::Exchange;
use super::config::NodeConfig;

/// `GET /api/v1/exchange/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeStatus {
    pub node_name: Option<String>,
    /// The peer API answers (`node_name` + `peer_api_key` set).
    pub serving: bool,
    pub transfers_paused: bool,
    /// Why the exchange settings do not hold (the exchange then acts as off).
    pub config_error: Option<String>,
    pub peers: Vec<PeerStatus>,
}

/// One configured peer, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub base_url: String,
    pub has_key: bool,
    /// A Cloudflare Access service token is configured for it.
    pub cf_access: bool,
}

pub fn router(ex: Arc<Exchange>) -> Router {
    Router::new()
        .route("/api/v1/exchange/status", get(status))
        .with_state(ex)
}

pub async fn status(State(ex): State<Arc<Exchange>>) -> Json<ExchangeStatus> {
    let paused = crate::db::models::get_setting(&ex.pool, SETTING_PEER_TRANSFERS_PAUSED)
        .await
        .ok()
        .flatten();
    let (cfg, config_error) = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => (cfg, None),
        Err(e) => (NodeConfig::default(), Some(e)),
    };
    Json(ExchangeStatus {
        node_name: cfg.node_name.clone(),
        serving: cfg.serving(),
        transfers_paused: peer_transfers_paused(paused.as_deref()),
        config_error,
        peers: cfg
            .peers
            .iter()
            .map(|p| PeerStatus {
                name: p.name.clone(),
                base_url: p.base_url.clone(),
                has_key: !p.key.is_empty(),
                cf_access: p.cf_client_id.is_some(),
            })
            .collect(),
    })
}

#[cfg(test)]
#[path = "lan_tests.rs"]
mod tests;
```

`crates/sp-server/src/lib.rs` — after the closing `};` of `let state = AppState { … };` add:

```rust
    // #229: this node in the exchange; its routes merge into the router below.
    let exchange = peer::Exchange::new(pool.clone(), config.cache_dir.clone());
```

and at "11. Axum HTTP server" replace `let router = api::router(state, config.dist_dir);` with:

```rust
    let router = api::router(state, config.dist_dir).merge(peer::router(exchange.clone()));
```

(`api::router` may carry the SPA fallback; the exchange router has none, so `merge` is valid.) `cargo fmt --all`; `wc -l crates/sp-server/src/lib.rs` ≤ 1000.

- [ ] **Step 4: Write the rules file** `.claude/rules/peer-exchange.md`

```markdown
---
paths:
  - "crates/sp-server/src/peer/**"
  - "crates/sp-server/src/db/models_peer*.rs"
  - "crates/sp-core/src/config.rs"
  - "crates/sp-server/src/downloader/mod.rs"
  - "crates/sp-server/src/stems/worker.rs"
  - "crates/sp-server/src/lyrics/worker.rs"
  - "crates/sp-server/src/reprocess/mod.rs"
  - ".github/workflows/deploy-pp.yml"
  - "e2e/post-deploy-pp*"
  - "e2e/post-deploy-peer-serving.spec.ts"
  - "e2e/peer-probe-gate*"
  - "scripts/setup-runner.ps1"
---

# The node exchange (#229): serve what you have, ask peers first

Spec: `docs/superpowers/specs/2026-10-06-pp-site-node-exchange-design.md`.
Plan: `docs/superpowers/plans/2026-10-06-pp-node-exchange.md`.

## Settings (`peer::config`, live — no restart)

- `node_name` (`snv`, `pp`; 1-32 of a-z 0-9 -): empty = the exchange is OFF.
- `peer_api_key` (≥ 32 chars): the key THIS node's peer API accepts; empty =
  the peer API answers 404.
- `peers`: JSON list `[{name, base_url, key, cf_client_id?, cf_client_secret?}]`
  — the peers asked first (Cloudflare Access token: both or neither).
- `peer_transfers_paused` = "true": no new transfer either way (the hasher
  pauses too). `peer_serve_max_mbps` (1..=10000, default 20): what this node
  sends.
- Secrets never leave the node in clear: `GET /api/v1/settings` shows
  `********`; a PATCH that sends `********` back keeps the stored value; a
  PATCH whose exchange setting does not hold answers 400 and writes NOTHING;
  `Debug` masks; no error text echoes a setting (serde errors → line/column).
- Set them only through the secret channel (`airuleset.py secret exec` +
  `curl -X PATCH …/api/v1/settings`), never in chat or a commit.
- `GET /api/v1/exchange/status` (LAN, no key): node, serving, paused, config
  error, peers without secrets.
```

Add to `CLAUDE.md`'s "Playbook router" list (after the "DB migrations" line):

```markdown
- node exchange (#229: `node_name` / `peer_api_key` / `peers` settings, secrets masked in GET /api/v1/settings and kept on a masked PATCH; every node serves `/api/v1/peer/{catalog,videos,artifact}` behind `X-SP-Peer-Key` and asks its peers before a heavy job — fetch (sha256-checked, Range-resumed, named from the OWN row), wait (≤ 2 h) or process here and announce; `GET /api/v1/exchange/status`, `POST /api/v1/exchange/probe`; PP deploys main releases only through `deploy-pp.yml`) → `.claude/rules/peer-exchange.md` (auto-loads on `peer/**`, `models_peer*`, the worker hooks, `deploy-pp.yml`, `e2e/post-deploy-pp*`)
```

- [ ] **Step 5: Run (CI only):** `cargo test -p sp-server peer::` and the whole `cargo test -p sp-server api::` — Expected: PASS.

- [ ] **Step 6: Commit** — `test(#229): exchange status route` (lan_tests.rs) then `feat(#229): Exchange + GET /api/v1/exchange/status, merged into the app router` (mod.rs, lan.rs, lib.rs) then `docs(#229): peer-exchange rules + CLAUDE.md router line`.

- [ ] **Step 7: Push the lane** (`git push origin dev`), monitor CI to terminal state. On the box nothing changes until the settings are set.

---

## Lane 2 — Catalog model (kinds, versions, wire types, job board)

Design comment for #229: *Approach:* the exchange's vocabulary as pure types — six artifact kinds (`video`, `audio`, `stem_vocals`, `stem_instrumental`, `lyrics`, `metadata`) with a per-kind version a node accepts (media/stems: a format constant; lyrics: `LYRICS_PIPELINE_VERSION`; metadata: 0 parser / 1 provider / 2 operator), three jobs (download, lyrics, stems) that NEED and MAKE kinds, typed wire structs (an unknown kind deserializes as `Unknown` and is dropped by `Catalog::sanitized`), and an in-memory job board whose RAII guard announces a running job. *Rejected:* announcing running jobs in a DB table — a crash would leave a job "running" forever and stall peers for the full 2 h; the in-memory board dies with the process. *Architektúra:* serde (existing), no new dependency.

### Task 2.1: `peer::kind` — kinds, jobs, versions

**Files:**
- Create: `crates/sp-server/src/peer/kind.rs`, `crates/sp-server/src/peer/kind_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (module list: `pub mod config; pub mod kind; pub mod lan;`)

**Interfaces:**
- Consumes: `crate::lyrics::LYRICS_PIPELINE_VERSION: u32`, `crate::metadata::manual::MANUAL_SOURCE`.
- Produces: `MEDIA_VERSION: u32 = 1`, `STEMS_VERSION: u32 = 1`, `METADATA_PARSER = 0`, `METADATA_PROVIDER = 1`, `METADATA_MANUAL = 2`; `enum ArtifactKind { Video, Audio, StemVocals, StemInstrumental, Lyrics, Metadata, Unknown }` (serde snake_case, `#[serde(other)] Unknown`) with `fn as_str(self) -> &'static str`, `fn parse(&str) -> Option<Self>`; `enum Job { Download, Lyrics, Stems }` with `fn as_str(self) -> &'static str`, `fn needs(self) -> &'static [ArtifactKind]`, `fn makes(self) -> &'static [ArtifactKind]`; `fn acceptable(ArtifactKind, u32) -> bool`; `fn metadata_version(Option<&str>, bool) -> u32`.

- [ ] **Step 1: Write the failing tests** `kind_tests.rs`

```rust
//! #229 `peer::kind`.

use super::*;
use crate::lyrics::LYRICS_PIPELINE_VERSION;

const ALL: [ArtifactKind; 6] = [
    ArtifactKind::Video,
    ArtifactKind::Audio,
    ArtifactKind::StemVocals,
    ArtifactKind::StemInstrumental,
    ArtifactKind::Lyrics,
    ArtifactKind::Metadata,
];

#[test]
fn every_kind_has_its_wire_name_both_ways() {
    let names = ["video", "audio", "stem_vocals", "stem_instrumental", "lyrics", "metadata"];
    for (kind, name) in ALL.iter().zip(names) {
        assert_eq!(kind.as_str(), name);
        assert_eq!(ArtifactKind::parse(name), Some(*kind));
        assert_eq!(serde_json::to_string(kind).unwrap(), format!("\"{name}\""));
        assert_eq!(serde_json::from_str::<ArtifactKind>(&format!("\"{name}\"")).unwrap(), *kind);
    }
    assert_eq!(ArtifactKind::Unknown.as_str(), "unknown");
    assert_eq!(ArtifactKind::parse("unknown"), None);
    assert_eq!(ArtifactKind::parse("dub"), None);
}

/// Review Focus 3: a newer peer's kind (`dub`) reads as Unknown, never an error.
#[test]
fn a_kind_this_node_does_not_know_reads_as_unknown() {
    assert_eq!(serde_json::from_str::<ArtifactKind>("\"dub\"").unwrap(), ArtifactKind::Unknown);
}

#[test]
fn each_job_needs_and_makes_its_kinds() {
    use ArtifactKind::*;
    assert_eq!(Job::Download.needs(), &[Video, Audio]);
    assert_eq!(Job::Download.makes(), &[Video, Audio, Metadata]);
    assert_eq!(Job::Lyrics.needs(), &[Lyrics]);
    assert_eq!(Job::Lyrics.makes(), &[Lyrics]);
    assert_eq!(Job::Stems.needs(), &[StemVocals, StemInstrumental]);
    assert_eq!(Job::Stems.makes(), &[StemVocals, StemInstrumental]);
    assert_eq!(Job::Download.as_str(), "download");
    assert_eq!(Job::Lyrics.as_str(), "lyrics");
    assert_eq!(Job::Stems.as_str(), "stems");
}

#[test]
fn a_node_takes_the_current_format_of_each_kind() {
    use ArtifactKind::*;
    assert_eq!((MEDIA_VERSION, STEMS_VERSION), (1, 1));
    for k in [Video, Audio] {
        assert!(acceptable(k, MEDIA_VERSION));
        assert!(!acceptable(k, MEDIA_VERSION + 1));
        assert!(!acceptable(k, MEDIA_VERSION - 1));
    }
    for k in [StemVocals, StemInstrumental] {
        assert!(acceptable(k, STEMS_VERSION));
        assert!(!acceptable(k, STEMS_VERSION + 1));
        assert!(!acceptable(k, STEMS_VERSION - 1));
    }
    assert!(acceptable(Lyrics, LYRICS_PIPELINE_VERSION));
    assert!(!acceptable(Lyrics, LYRICS_PIPELINE_VERSION - 1), "an older pipeline");
    assert!(!acceptable(Lyrics, LYRICS_PIPELINE_VERSION + 1), "a newer pipeline (a dev peer)");
    assert!(!acceptable(Metadata, METADATA_PARSER));
    assert!(acceptable(Metadata, METADATA_PROVIDER));
    assert!(acceptable(Metadata, METADATA_MANUAL));
    assert!(!acceptable(Unknown, 1));
}

#[test]
fn metadata_version_ranks_parser_provider_operator() {
    assert_eq!(metadata_version(Some("manual"), false), METADATA_MANUAL);
    assert_eq!(metadata_version(Some("manual"), true), METADATA_MANUAL);
    assert_eq!(metadata_version(Some("gemini"), false), METADATA_PROVIDER);
    assert_eq!(metadata_version(Some("gemini"), true), METADATA_PARSER);
    assert_eq!(metadata_version(Some("regex"), true), METADATA_PARSER);
    assert_eq!(metadata_version(None, false), METADATA_PARSER);
    assert_eq!((METADATA_PARSER, METADATA_PROVIDER, METADATA_MANUAL), (0, 1, 2));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::kind` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/kind.rs`

```rust
//! #229: what the exchange moves (artifact kinds), the jobs that make them,
//! and the version of each kind this node takes from a peer.

use serde::{Deserialize, Serialize};

use crate::metadata::manual::MANUAL_SOURCE;

/// The format of a song's video + audio pair: the split layout (video stream
/// copied, audio loudnorm -14 LUFS FLAC, CLAUDE.md "Split-file audio layout").
/// Bump when the download / normalize output changes.
pub const MEDIA_VERSION: u32 = 1;
/// The format of the karaoke stems (`stem_worker.py`). Bump when the
/// separation output changes.
pub const STEMS_VERSION: u32 = 1;
/// Metadata versions: a title parser's guess, a provider's answer, an operator's correction.
pub const METADATA_PARSER: u32 = 0;
pub const METADATA_PROVIDER: u32 = 1;
pub const METADATA_MANUAL: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Video,
    Audio,
    StemVocals,
    StemInstrumental,
    Lyrics,
    Metadata,
    /// A kind this node does not know (a newer peer's `dub`): dropped.
    #[serde(other)]
    Unknown,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::StemVocals => "stem_vocals",
            Self::StemInstrumental => "stem_instrumental",
            Self::Lyrics => "lyrics",
            Self::Metadata => "metadata",
            Self::Unknown => "unknown",
        }
    }

    /// The kind a URL segment names; `None` for anything else (`unknown` too).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            "stem_vocals" => Some(Self::StemVocals),
            "stem_instrumental" => Some(Self::StemInstrumental),
            "lyrics" => Some(Self::Lyrics),
            "metadata" => Some(Self::Metadata),
            _ => None,
        }
    }
}

/// A heavy job a node asks its peers about before it runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Job {
    /// Download + normalize + metadata (`DownloadWorker::process_next`).
    Download,
    Lyrics,
    Stems,
}

impl Job {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Lyrics => "lyrics",
            Self::Stems => "stems",
        }
    }

    /// What a peer must hold for this job to be fetched instead of run.
    pub fn needs(self) -> &'static [ArtifactKind] {
        match self {
            Self::Download => &[ArtifactKind::Video, ArtifactKind::Audio],
            Self::Lyrics => &[ArtifactKind::Lyrics],
            Self::Stems => &[ArtifactKind::StemVocals, ArtifactKind::StemInstrumental],
        }
    }

    /// What this job announces in its node's catalog while it runs.
    pub fn makes(self) -> &'static [ArtifactKind] {
        match self {
            Self::Download => &[ArtifactKind::Video, ArtifactKind::Audio, ArtifactKind::Metadata],
            Self::Lyrics => &[ArtifactKind::Lyrics],
            Self::Stems => &[ArtifactKind::StemVocals, ArtifactKind::StemInstrumental],
        }
    }
}

/// Whether a peer's `version` of `kind` is one this node takes: exactly its
/// own current format, or for metadata any provider's or operator's title.
pub fn acceptable(kind: ArtifactKind, version: u32) -> bool {
    match kind {
        ArtifactKind::Video | ArtifactKind::Audio => version == MEDIA_VERSION,
        ArtifactKind::StemVocals | ArtifactKind::StemInstrumental => version == STEMS_VERSION,
        ArtifactKind::Lyrics => version == crate::lyrics::LYRICS_PIPELINE_VERSION,
        ArtifactKind::Metadata => version >= METADATA_PROVIDER,
        ArtifactKind::Unknown => false,
    }
}

/// A row's metadata version from its `metadata_source` and `gemini_failed`.
pub fn metadata_version(source: Option<&str>, gemini_failed: bool) -> u32 {
    match source {
        Some(MANUAL_SOURCE) => METADATA_MANUAL,
        Some(_) if !gemini_failed => METADATA_PROVIDER,
        _ => METADATA_PARSER,
    }
}

#[cfg(test)]
#[path = "kind_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::kind` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): artifact kinds, jobs and versions` then `feat(#229): peer::kind — kinds, jobs, the versions a node takes`.

### Task 2.2: `peer::wire` — the peer API's typed JSON

**Files:**
- Create: `crates/sp-server/src/peer/wire.rs`, `crates/sp-server/src/peer/wire_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (add `pub mod wire;`, sorted)

**Interfaces:**
- Consumes: `kind::{ArtifactKind, metadata_version}`, `crate::downloader::cache::is_valid_video_id`.
- Produces: `struct Artifact { youtube_id: String, kind: ArtifactKind, version: u32, size: u64, sha256: String, updated_at: Option<String> }`; `struct RunningJob { youtube_id, kind: ArtifactKind, node: String, started_at: String }`; `struct Catalog { node: String, artifacts: Vec<Artifact>, jobs: Vec<RunningJob> }` with `fn sanitized(self) -> Self`, `fn runs(&self, youtube_id: &str, kinds: &[ArtifactKind]) -> bool`; `struct PeerMetadata { youtube_id, song, artist, metadata_source: Option<String>, gemini_failed: bool }` with `fn version(&self) -> u32`, `fn to_bytes(&self) -> Vec<u8>`; `fn is_sha256_hex(&str) -> bool`; `fn now_ms() -> i64`; `fn ms_to_rfc3339(i64) -> String`; `fn rfc3339_to_ms(&str) -> Option<i64>`. (Lane 4 adds `PeerLyrics`, `PeerVideo`.)

- [ ] **Step 1: Write the failing tests** `wire_tests.rs`

```rust
//! #229 `peer::wire`.

use super::*;
use crate::peer::kind::ArtifactKind;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn artifact(id: &str, kind: ArtifactKind, sha: &str) -> Artifact {
    Artifact { youtube_id: id.into(), kind, version: 1, size: 10, sha256: sha.into(), updated_at: None }
}

#[test]
fn a_sha256_is_64_lowercase_hex_digits() {
    assert!(is_sha256_hex(SHA));
    assert!(!is_sha256_hex(&SHA[..63]));
    assert!(!is_sha256_hex(&format!("{SHA}0")));
    assert!(!is_sha256_hex(&SHA.to_uppercase()));
    assert!(!is_sha256_hex(&SHA.replace('0', "g")));
}

/// Review Focus 3: a newer peer's catalog — an unknown kind, a bad id, a bad
/// sha — keeps every entry this node can use.
#[test]
fn sanitized_keeps_only_what_this_node_can_use() {
    let json = format!(
        r#"{{"node":"snv","artifacts":[
            {{"youtube_id":"aaaaaaaaaaa","kind":"audio","version":1,"size":10,"sha256":"{SHA}"}},
            {{"youtube_id":"aaaaaaaaaaa","kind":"dub","version":1,"size":10,"sha256":"{SHA}"}},
            {{"youtube_id":"../../x","kind":"video","version":1,"size":10,"sha256":"{SHA}"}},
            {{"youtube_id":"bbbbbbbbbbb","kind":"video","version":1,"size":10,"sha256":"nothex"}}],
          "jobs":[
            {{"youtube_id":"ccccccccccc","kind":"lyrics","node":"snv","started_at":"t"}},
            {{"youtube_id":"ccccccccccc","kind":"dub","node":"snv","started_at":"t"}},
            {{"youtube_id":"bad","kind":"lyrics","node":"snv","started_at":"t"}}],
          "later_field":true}}"#
    );
    let c: Catalog = serde_json::from_str::<Catalog>(&json).unwrap().sanitized();
    assert_eq!(c.node, "snv");
    assert_eq!(c.artifacts, vec![artifact("aaaaaaaaaaa", ArtifactKind::Audio, SHA)]);
    assert_eq!(c.jobs.len(), 1);
    assert_eq!(c.jobs[0].kind, ArtifactKind::Lyrics);
}

#[test]
fn a_catalog_without_lists_reads_as_empty() {
    let c: Catalog = serde_json::from_str(r#"{"node":"snv"}"#).unwrap();
    assert!(c.artifacts.is_empty() && c.jobs.is_empty());
}

#[test]
fn runs_matches_the_video_and_any_of_the_kinds() {
    let c = Catalog {
        node: "snv".into(),
        artifacts: vec![],
        jobs: vec![RunningJob {
            youtube_id: "aaaaaaaaaaa".into(),
            kind: ArtifactKind::StemVocals,
            node: "snv".into(),
            started_at: "t".into(),
        }],
    };
    use ArtifactKind::*;
    assert!(c.runs("aaaaaaaaaaa", &[StemVocals, StemInstrumental]));
    assert!(!c.runs("aaaaaaaaaaa", &[Lyrics]));
    assert!(!c.runs("bbbbbbbbbbb", &[StemVocals]));
}

#[test]
fn metadata_bytes_are_canonical_and_carry_the_version() {
    let m = PeerMetadata {
        youtube_id: "aaaaaaaaaaa".into(),
        song: "Way Maker".into(),
        artist: "Sinach".into(),
        metadata_source: Some("gemini".into()),
        gemini_failed: false,
    };
    assert_eq!(m.version(), 1);
    assert_eq!(m.to_bytes(), m.clone().to_bytes());
    let back: PeerMetadata = serde_json::from_slice(&m.to_bytes()).unwrap();
    assert_eq!(back, m);
    let gf = PeerMetadata { gemini_failed: true, ..m };
    assert_eq!(gf.version(), 0);
}

#[test]
fn times_go_both_ways_at_millisecond_precision() {
    let ms = 1_791_302_400_123;
    let text = ms_to_rfc3339(ms);
    assert_eq!(text, "2026-10-06T16:00:00.123Z");
    assert_eq!(rfc3339_to_ms(&text), Some(ms));
    assert_eq!(rfc3339_to_ms("2026-10-06T18:00:00.123+02:00"), Some(ms));
    assert_eq!(rfc3339_to_ms("yesterday"), None);
    assert!(now_ms() > ms - 86_400_000 * 365);
}
```

(`1_791_302_400_123` = `2026-10-06T16:00:00.123Z`, checked with `python3 -c "import datetime;print(datetime.datetime.fromtimestamp(1791302400.123, datetime.timezone.utc))"`.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::wire` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/wire.rs`

```rust
//! #229: the peer API's JSON, typed both ways. A peer's answer is untrusted:
//! no `serde_json::Value` anywhere inside (`rust-workspace.md`), unknown
//! fields are skipped, unknown kinds read as `Unknown` and are dropped by
//! [`Catalog::sanitized`].

use serde::{Deserialize, Serialize};

use super::kind::{ArtifactKind, metadata_version};
use crate::downloader::cache::is_valid_video_id;

/// One artifact a node holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub youtube_id: String,
    pub kind: ArtifactKind,
    pub version: u32,
    pub size: u64,
    /// 64 lowercase hex digits.
    pub sha256: String,
    /// When the serving node listed (hashed) it; `None` for metadata.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// One job a node runs now (one entry per kind the job makes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningJob {
    pub youtube_id: String,
    pub kind: ArtifactKind,
    pub node: String,
    pub started_at: String,
}

/// `GET /api/v1/peer/catalog`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub node: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    #[serde(default)]
    pub jobs: Vec<RunningJob>,
}

impl Catalog {
    /// Only the entries this node can use: a known kind, a real YouTube id,
    /// a sha256 as 64 lowercase hex digits.
    pub fn sanitized(mut self) -> Self {
        self.artifacts.retain(|a| {
            a.kind != ArtifactKind::Unknown
                && is_valid_video_id(&a.youtube_id)
                && is_sha256_hex(&a.sha256)
        });
        self.jobs
            .retain(|j| j.kind != ArtifactKind::Unknown && is_valid_video_id(&j.youtube_id));
        self
    }

    /// A job for `youtube_id` making any of `kinds` runs on this node.
    pub fn runs(&self, youtube_id: &str, kinds: &[ArtifactKind]) -> bool {
        self.jobs
            .iter()
            .any(|j| j.youtube_id == youtube_id && kinds.contains(&j.kind))
    }
}

/// A video's title as a node holds it: the `metadata` artifact's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMetadata {
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub metadata_source: Option<String>,
    pub gemini_failed: bool,
}

impl PeerMetadata {
    pub fn version(&self) -> u32 {
        metadata_version(self.metadata_source.as_deref(), self.gemini_failed)
    }

    /// The canonical bytes: the catalog's metadata sha256 is over these.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// 64 lowercase hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `2026-10-06T16:00:00.123Z`.
pub fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

pub fn rfc3339_to_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::wire` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): the peer API's wire types` then `feat(#229): peer::wire — catalog, artifact, job, metadata types`.

### Task 2.3: `peer::board` — running jobs, announced while a guard lives

**Files:**
- Create: `crates/sp-server/src/peer/board.rs`, `crates/sp-server/src/peer/board_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (add `pub mod board;`; `Exchange` gains `pub(crate) board: Arc<board::JobBoard>` built in `new()`, and `pub fn announce(&self, youtube_id: &str, job: kind::Job) -> board::JobGuard`)

**Interfaces:**
- Consumes: `kind::Job`, `wire::{RunningJob, ms_to_rfc3339, now_ms}`.
- Produces: `struct JobBoard` (`Default`) with `fn announce(self: &Arc<Self>, youtube_id: &str, job: Job) -> JobGuard`, `fn snapshot(&self, node: &str) -> Vec<RunningJob>` (sorted by youtube id, then kind); `#[must_use] struct JobGuard` (Drop removes the announcement); `Exchange::announce`.

- [ ] **Step 1: Write the failing tests** `board_tests.rs`

```rust
//! #229 `peer::board`.

use super::*;
use crate::peer::kind::{ArtifactKind, Job};
use std::sync::Arc;

#[test]
fn a_job_is_listed_while_its_guard_lives() {
    let board = Arc::new(JobBoard::default());
    assert!(board.snapshot("pp").is_empty());
    let guard = board.announce("aaaaaaaaaaa", Job::Stems);
    let jobs = board.snapshot("pp");
    let kinds: Vec<ArtifactKind> = jobs.iter().map(|j| j.kind).collect();
    assert_eq!(kinds, vec![ArtifactKind::StemInstrumental, ArtifactKind::StemVocals]);
    assert!(jobs.iter().all(|j| j.youtube_id == "aaaaaaaaaaa" && j.node == "pp"));
    assert!(crate::peer::wire::rfc3339_to_ms(&jobs[0].started_at).is_some());
    drop(guard);
    assert!(board.snapshot("pp").is_empty());
}

#[test]
fn a_second_guard_of_the_same_job_keeps_it_listed_until_both_end() {
    let board = Arc::new(JobBoard::default());
    let first = board.announce("aaaaaaaaaaa", Job::Download);
    let second = board.announce("aaaaaaaaaaa", Job::Download);
    drop(first);
    assert_eq!(board.snapshot("pp").len(), 3, "video, audio, metadata");
    drop(second);
    assert!(board.snapshot("pp").is_empty());
}

#[test]
fn jobs_are_listed_in_youtube_id_order() {
    let board = Arc::new(JobBoard::default());
    let _b = board.announce("bbbbbbbbbbb", Job::Lyrics);
    let _a = board.announce("aaaaaaaaaaa", Job::Lyrics);
    let ids: Vec<String> = board.snapshot("snv").into_iter().map(|j| j.youtube_id).collect();
    assert_eq!(ids, vec!["aaaaaaaaaaa".to_string(), "bbbbbbbbbbb".to_string()]);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::board` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/board.rs`

```rust
//! #229: the jobs this node runs now, listed in its catalog while they run.
//! In memory on purpose: a crash takes its announcements with it, so a peer
//! never waits on a job that died (it would wait the full 2 h otherwise).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::kind::Job;
use super::wire::{RunningJob, ms_to_rfc3339, now_ms};

/// `(youtube id, job)` → `(started ms, live guards)`.
type Running = HashMap<(String, Job), (i64, usize)>;

#[derive(Default)]
pub struct JobBoard {
    running: Mutex<Running>,
}

/// The announcement of one running job; dropping it ends the announcement.
#[must_use = "a job is announced only while its guard lives"]
pub struct JobGuard {
    board: Arc<JobBoard>,
    key: (String, Job),
}

impl JobBoard {
    pub fn announce(self: &Arc<Self>, youtube_id: &str, job: Job) -> JobGuard {
        let key = (youtube_id.to_string(), job);
        let mut running = self.lock();
        running.entry(key.clone()).or_insert((now_ms(), 0)).1 += 1;
        JobGuard { board: Arc::clone(self), key }
    }

    /// Every running job as catalog entries of `node`, one per kind it makes.
    pub fn snapshot(&self, node: &str) -> Vec<RunningJob> {
        let mut jobs: Vec<RunningJob> = self
            .lock()
            .iter()
            .flat_map(|((youtube_id, job), (started, _))| {
                job.makes().iter().map(move |kind| RunningJob {
                    youtube_id: youtube_id.clone(),
                    kind: *kind,
                    node: node.to_string(),
                    started_at: ms_to_rfc3339(*started),
                })
            })
            .collect();
        jobs.sort_by(|a, b| {
            (a.youtube_id.as_str(), a.kind.as_str()).cmp(&(b.youtube_id.as_str(), b.kind.as_str()))
        });
        jobs
    }

    fn lock(&self) -> MutexGuard<'_, Running> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        let mut running = self.board.lock();
        if let Some(entry) = running.get_mut(&self.key) {
            entry.1 -= 1;
            if entry.1 == 0 {
                running.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
#[path = "board_tests.rs"]
mod tests;
```

In `peer/mod.rs`: module list `pub mod board; pub mod config; pub mod kind; pub mod lan; pub mod wire;`; `Exchange`:

```rust
pub struct Exchange {
    pub(crate) pool: SqlitePool,
    pub(crate) cache_dir: PathBuf,
    /// The jobs this node runs now (its catalog lists them).
    pub(crate) board: Arc<board::JobBoard>,
}

impl Exchange {
    pub fn new(pool: SqlitePool, cache_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            pool,
            cache_dir,
            board: Arc::new(board::JobBoard::default()),
        })
    }

    /// Announce `job` for `youtube_id` in this node's catalog while the guard lives.
    pub fn announce(&self, youtube_id: &str, job: kind::Job) -> board::JobGuard {
        self.board.announce(youtube_id, job)
    }
}
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): job board announcements` then `feat(#229): peer::board — running jobs announced while their guard lives`.
- [ ] **Step 6: Append to `.claude/rules/peer-exchange.md`** and commit `docs(#229): …`:

```markdown
## Kinds, versions, jobs (`peer::kind`, `peer::wire`, `peer::board`)

- Kinds: `video`, `audio`, `stem_vocals`, `stem_instrumental`, `lyrics`,
  `metadata`; an unknown kind (a newer peer's `dub`) reads as `Unknown` and
  `Catalog::sanitized` drops it with any entry whose id or sha is malformed.
- A node takes a peer's artifact only at ITS OWN current format:
  `MEDIA_VERSION` (video/audio), `STEMS_VERSION`, `LYRICS_PIPELINE_VERSION`
  (equality — a dev peer's newer lyrics are not taken). Metadata: parser 0,
  provider 1, operator 2; ≥ 1 is taken. Bump `MEDIA_VERSION` /
  `STEMS_VERSION` in the same change that alters that output.
- Jobs: Download (needs video+audio, makes video+audio+metadata), Lyrics,
  Stems. A running job is announced by an in-memory `JobGuard` (a crash
  takes it along — never a DB row).
```

- [ ] **Step 7: Push the lane**, monitor CI.

---

## Lane 3 — Own catalog + sha256 cache

Design comment for #229: *Approach:* the catalog is built from this node's own rows (one representative row per YouTube id: the lowest id with an audio; stems when `stem_status = 'done'`, named after the CURRENT audio; `{yt}_lyrics.json` when no row of the video is dub-requested or carries the Live-Translate track) joined with a persistent sha256 cache `peer_hashes(path, size, mtime_ms, sha256, hashed_at_ms)`. A background hasher fills it one file at a time at ≤ 40 MiB/s, stat → hash → stat (a file that changed meanwhile is skipped), only while this node serves; a file not yet hashed is simply not listed. A catalog request touches no file. *Rejected:* hashing per request (115 GB at SNV), and a hash keyed only by path (a re-download under the same name would keep a stale sha — the key is path + size + mtime). *Architektúra:* sha2 (existing), `spawn_blocking` reader, settings + V29 table.

### Task 3.1: V29 `peer_hashes` + `db::models_peer`

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs` (`(29, MIGRATION_V29),` after `(28, MIGRATION_V28),`; the const after `MIGRATION_V28`; `pub mod models_peer; // #229 …` in the module list; test hook after `tests_v28`)
- Create: `crates/sp-server/src/db/mod_tests_v29.rs`, `crates/sp-server/src/db/models_peer.rs`, `crates/sp-server/src/db/models_peer_tests.rs`

**Interfaces:**
- Produces: table `peer_hashes`; `crate::db::models_peer::{HashEntry { path: String, size: i64, mtime_ms: i64, sha256: String, hashed_at_ms: i64 }, all_hashes(&SqlitePool) -> Result<HashMap<String, HashEntry>, sqlx::Error>, put_hash(&SqlitePool, &HashEntry) -> Result<(), sqlx::Error>, remove_hash(&SqlitePool, &str) -> Result<u64, sqlx::Error>, prune_hashes(&SqlitePool, &HashSet<String>) -> Result<u64, sqlx::Error>}`.

- [ ] **Step 1: Write the failing tests.** `crates/sp-server/src/db/mod_tests_v29.rs`:

```rust
//! V29 (#229): the node exchange's sha256 cache.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn migration_v29_creates_the_peer_hash_cache_one_entry_per_path() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 28).await;
    apply_upto(&pool, 29).await;
    let insert = "INSERT INTO peer_hashes (path, size, mtime_ms, sha256, hashed_at_ms) \
                  VALUES ('/c/a_audio.flac', 3, 4, 'ab', 5)";
    sqlx::query(insert).execute(&pool).await.unwrap();
    assert!(sqlx::query(insert).execute(&pool).await.is_err(), "one entry per path");
    assert_eq!(current_schema_version(&pool).await.unwrap(), 29);
}
```

`crates/sp-server/src/db/models_peer_tests.rs`:

```rust
//! #229 `db::models_peer` — the hash cache.

use super::*;
use std::collections::HashSet;

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

fn entry(path: &str, sha: &str) -> HashEntry {
    HashEntry { path: path.into(), size: 3, mtime_ms: 4, sha256: sha.into(), hashed_at_ms: 5 }
}

#[tokio::test]
async fn a_hash_is_stored_replaced_and_removed_by_path() {
    let pool = pool().await;
    put_hash(&pool, &entry("/c/a", "s1")).await.unwrap();
    put_hash(&pool, &HashEntry { size: 9, mtime_ms: 8, hashed_at_ms: 7, ..entry("/c/a", "s2") })
        .await
        .unwrap();
    let all = all_hashes(&pool).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        all["/c/a"],
        HashEntry { path: "/c/a".into(), size: 9, mtime_ms: 8, sha256: "s2".into(), hashed_at_ms: 7 }
    );
    assert_eq!(remove_hash(&pool, "/c/a").await.unwrap(), 1);
    assert_eq!(remove_hash(&pool, "/c/a").await.unwrap(), 0);
    assert!(all_hashes(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn prune_keeps_only_the_named_paths() {
    let pool = pool().await;
    for p in ["/c/a", "/c/b", "/c/c"] {
        put_hash(&pool, &entry(p, "s")).await.unwrap();
    }
    let keep: HashSet<String> = ["/c/b".to_string()].into_iter().collect();
    assert_eq!(prune_hashes(&pool, &keep).await.unwrap(), 2);
    let left: Vec<String> = all_hashes(&pool).await.unwrap().into_keys().collect();
    assert_eq!(left, vec!["/c/b".to_string()]);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server db::` — Expected now: compile FAIL.

- [ ] **Step 3: Implement.** In `crates/sp-server/src/db/mod.rs`:

```rust
// V29 (#229): the node exchange's sha256 cache. The hasher (`peer/hasher.rs`)
// hashes each artifact file once per (path, size, mtime); the catalog lists
// only hashed files, so a catalog request never reads a file.
const MIGRATION_V29: &str = "
CREATE TABLE peer_hashes (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime_ms INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    hashed_at_ms INTEGER NOT NULL
);
";
```

module list: `pub mod models_peer; // #229 the node exchange's tables (own module, models.rs is at the cap)`; test hook (after the `tests_v28` hook):

```rust
#[path = "mod_tests_v29.rs"]
#[cfg(test)]
mod tests_v29;
```

`crates/sp-server/src/db/models_peer.rs`:

```rust
//! #229: the node exchange's tables. V29 `peer_hashes`: this node's sha256
//! cache, keyed by path; an entry holds while the file's size and mtime match.

use std::collections::{HashMap, HashSet};

use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct HashEntry {
    pub path: String,
    pub size: i64,
    pub mtime_ms: i64,
    pub sha256: String,
    pub hashed_at_ms: i64,
}

/// Every cached hash, by path.
pub async fn all_hashes(pool: &SqlitePool) -> Result<HashMap<String, HashEntry>, sqlx::Error> {
    let rows: Vec<HashEntry> =
        sqlx::query_as("SELECT path, size, mtime_ms, sha256, hashed_at_ms FROM peer_hashes")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|h| (h.path.clone(), h)).collect())
}

pub async fn put_hash(pool: &SqlitePool, e: &HashEntry) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO peer_hashes (path, size, mtime_ms, sha256, hashed_at_ms) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(path) DO UPDATE SET size = excluded.size, \
             mtime_ms = excluded.mtime_ms, sha256 = excluded.sha256, \
             hashed_at_ms = excluded.hashed_at_ms",
    )
    .bind(&e.path)
    .bind(e.size)
    .bind(e.mtime_ms)
    .bind(&e.sha256)
    .bind(e.hashed_at_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop the entry of `path` (its file is gone). Returns the rows removed.
pub async fn remove_hash(pool: &SqlitePool, path: &str) -> Result<u64, sqlx::Error> {
    let done = sqlx::query("DELETE FROM peer_hashes WHERE path = ?")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

/// Drop every entry whose path is not in `keep` (a renamed or removed song).
pub async fn prune_hashes(pool: &SqlitePool, keep: &HashSet<String>) -> Result<u64, sqlx::Error> {
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM peer_hashes")
        .fetch_all(pool)
        .await?;
    let mut pruned = 0;
    for path in paths.iter().filter(|p| !keep.contains(*p)) {
        pruned += remove_hash(pool, path).await?;
    }
    Ok(pruned)
}

#[cfg(test)]
#[path = "models_peer_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server db::` — Expected: PASS (every earlier `mod_tests_vN` still passes: V29 touches no existing table).
- [ ] **Step 5: Commit** — `test(#229): V29 peer hash cache` then `feat(#229): V29 peer_hashes + db::models_peer`.

### Task 3.2: `peer::hash` + `peer::throttle` (rate math)

**Files:**
- Create: `crates/sp-server/src/peer/hash.rs`, `crates/sp-server/src/peer/throttle.rs`, `crates/sp-server/src/peer/hash_tests.rs`, `crates/sp-server/src/peer/throttle_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (add `pub mod hash;`, `pub mod throttle;`, sorted)

**Interfaces:**
- Produces: `throttle::wait_for(done: u64, elapsed: Duration, rate: u64) -> Duration` (rate = bytes/s, 0 = no limit), `throttle::mbps_to_bytes(u32) -> u64`; `hash::sha256_hex(&[u8]) -> String`, `async hash::sha256_file(&Path, max_bytes_per_s: u64) -> std::io::Result<String>`.

- [ ] **Step 1: Write the failing tests.** `throttle_tests.rs`:

```rust
//! #229 `peer::throttle`.

use super::*;
use std::time::Duration;

#[test]
fn the_wait_keeps_the_average_at_the_rate() {
    let ms = Duration::from_millis;
    assert_eq!(wait_for(0, ms(0), 1_000), ms(0));
    assert_eq!(wait_for(1_000, ms(0), 1_000), ms(1_000));
    assert_eq!(wait_for(250, ms(0), 1_000), ms(250));
    assert_eq!(wait_for(1_000, ms(400), 1_000), ms(600));
    assert_eq!(wait_for(1_000, ms(1_000), 1_000), ms(0));
    assert_eq!(wait_for(1_000, ms(2_000), 1_000), ms(0), "behind never waits");
    assert_eq!(wait_for(3, ms(0), 2), ms(1_500));
}

#[test]
fn rate_zero_is_no_limit_and_huge_counts_do_not_overflow() {
    assert_eq!(wait_for(u64::MAX, Duration::ZERO, 0), Duration::ZERO);
    let long = wait_for(u64::MAX, Duration::ZERO, 1);
    assert!(long > Duration::from_secs(1 << 40), "{long:?}");
}

#[test]
fn mbit_per_second_in_bytes() {
    assert_eq!(mbps_to_bytes(1), 125_000);
    assert_eq!(mbps_to_bytes(20), 2_500_000);
    assert_eq!(mbps_to_bytes(10_000), 1_250_000_000);
}
```

`hash_tests.rs`:

```rust
//! #229 `peer::hash`.

use super::*;

/// sha256("abc"), FIPS 180-2 test vector.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[test]
fn sha256_of_bytes_is_lowercase_hex() {
    assert_eq!(sha256_hex(b"abc"), ABC);
}

#[tokio::test]
async fn sha256_of_a_file_matches_its_bytes_across_read_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    let bytes: Vec<u8> = (0..(CHUNK * 2 + 7)).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(sha256_file(&path, 0).await.unwrap(), sha256_hex(&bytes));
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(sha256_file(&path, 1 << 30).await.unwrap(), ABC);
}

#[tokio::test]
async fn a_missing_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(sha256_file(&dir.path().join("none"), 0).await.is_err());
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::hash peer::throttle` — Expected now: compile FAIL.

- [ ] **Step 3: Implement.** `crates/sp-server/src/peer/throttle.rs`:

```rust
//! #229: transfers and hashing at a bounded rate — the serving node's uplink
//! also carries the live stream, its disk also feeds the wall.

use std::time::Duration;

/// How long to pause after `done` bytes in `elapsed` so the average stays at
/// or under `rate` bytes/s. `rate` 0 = no limit.
pub fn wait_for(done: u64, elapsed: Duration, rate: u64) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    let due_us = u128::from(done) * 1_000_000 / u128::from(rate);
    let due = Duration::from_micros(u64::try_from(due_us).unwrap_or(u64::MAX));
    due.saturating_sub(elapsed)
}

/// Mbit/s as bytes/s.
pub fn mbps_to_bytes(mbps: u32) -> u64 {
    u64::from(mbps) * 125_000
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;
```

`crates/sp-server/src/peer/hash.rs`:

```rust
//! #229: sha256 as 64 lowercase hex digits — of bytes, or of a file read off
//! the async runtime at a bounded rate.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use sha2::{Digest, Sha256};

/// One read while hashing a file.
pub(crate) const CHUNK: usize = 1 << 20;

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The sha256 of the file at `path`, reading at most `max_bytes_per_s` (0 = no limit).
pub async fn sha256_file(path: &Path, max_bytes_per_s: u64) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let started = Instant::now();
        let mut done: u64 = 0;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            done += n as u64;
            std::thread::sleep(super::throttle::wait_for(done, started.elapsed(), max_bytes_per_s));
        }
        Ok(hex(&hasher.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::hash peer::throttle` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): sha256 + rate math` then `feat(#229): peer::hash + peer::throttle`.

### Task 3.3: test rig + `peer::catalog`

**Files:**
- Create: `crates/sp-server/src/peer/rig.rs` (`#[cfg(test)]`), `crates/sp-server/src/peer/catalog.rs`, `crates/sp-server/src/peer/catalog_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (add `pub mod catalog;` sorted in the list; add `#[cfg(test)] pub(crate) mod rig;` as the LAST item of the file; add `Exchange::transfers_paused`)

**Interfaces:**
- Consumes: `models_peer::all_hashes`, `kind::*`, `wire::{Artifact, Catalog, PeerMetadata, ms_to_rfc3339}`, `hash::sha256_hex`, `crate::stems::stem_paths`, `crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE`, `crate::downloader::cache::is_valid_video_id`.
- Produces: `catalog::{ArtifactFile { youtube_id, kind, version, path: PathBuf }, CatalogCounts { files: usize, listed: usize }, artifact_files(&SqlitePool, &Path, Option<&str>) -> Result<Vec<ArtifactFile>, sqlx::Error>, metadata_for(&SqlitePool, Option<&str>) -> Result<Vec<PeerMetadata>, sqlx::Error>, build(&Exchange, node: &str, since_ms: Option<i64>) -> Result<Catalog, sqlx::Error>, counts(&Exchange) -> Result<CatalogCounts, sqlx::Error>, path_key(&Path) -> String}`; `Exchange::transfers_paused(&self) -> bool` (async); rig: `TestNode { name, ex, base_url }` with `start(name, Option<&str>)`, `pool()`, `cache()`, `as_peer(key) -> PeerConfig`, `set_peers(&[PeerConfig])`, `add_video(yt) -> i64`, `add_video_to(playlist_id, yt) -> i64`, `give_song(id, yt, song, artist) -> (PathBuf, PathBuf)` (video 2 000 B, audio 3 000 B), `give_stems(id) -> (PathBuf, PathBuf)` (1 500 B, 1 700 B), `give_lyrics(id, yt, source) -> Vec<u8>`; `rig::{SNV_KEY, bytes(len, seed)}`.

- [ ] **Step 1: Write the rig** `crates/sp-server/src/peer/rig.rs`

```rust
//! #229 tests: a real node of the exchange — its own in-memory DB, cache dir
//! and HTTP port serving `peer::router` — so two nodes talk over real HTTP.
#![allow(dead_code)] // shared by the tests of every exchange lane; a lane uses some helpers only

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sp_core::config::{SETTING_NODE_NAME, SETTING_PEER_API_KEY, SETTING_PEERS};
use sqlx::SqlitePool;

use super::Exchange;
use super::config::PeerConfig;
use crate::downloader::cache::{audio_filename, video_filename};

/// SNV's peer key in the tests (≥ 32 characters).
pub(crate) const SNV_KEY: &str = "snv-test-peer-key-0123456789abcdef";

/// `len` deterministic bytes; `seed` tells two files apart.
pub(crate) fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

pub(crate) struct TestNode {
    pub(crate) name: String,
    pub(crate) ex: Arc<Exchange>,
    pub(crate) base_url: String,
    dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl TestNode {
    /// A node named `name` (serving with `serve_key` when given) with one
    /// active playlist (id 1), listening on a free 127.0.0.1 port.
    pub(crate) async fn start(name: &str, serve_key: Option<&str>) -> Self {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();
        set(&pool, SETTING_NODE_NAME, name).await;
        if let Some(key) = serve_key {
            set(&pool, SETTING_PEER_API_KEY, key).await;
        }
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'p1', 'u1', 'SP-p1', 1), (2, 'p2', 'u2', 'SP-p2', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ex = Exchange::new(pool, dir.path().to_path_buf());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let app = super::router(ex.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { name: name.to_string(), ex, base_url, dir, server }
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.ex.pool
    }

    pub(crate) fn cache(&self) -> &Path {
        self.dir.path()
    }

    /// This node as another node's peer, sending `key`.
    pub(crate) fn as_peer(&self, key: &str) -> PeerConfig {
        PeerConfig {
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            key: key.into(),
            cf_client_id: None,
            cf_client_secret: None,
        }
    }

    pub(crate) async fn set_peers(&self, peers: &[PeerConfig]) {
        set(self.pool(), SETTING_PEERS, &serde_json::to_string(peers).unwrap()).await;
    }

    pub(crate) async fn add_video(&self, youtube_id: &str) -> i64 {
        self.add_video_to(1, youtube_id).await
    }

    pub(crate) async fn add_video_to(&self, playlist_id: i64, youtube_id: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, normalized) \
             VALUES (?, ?, 'A YouTube title', 0) RETURNING id",
        )
        .bind(playlist_id)
        .bind(youtube_id)
        .fetch_one(self.pool())
        .await
        .unwrap()
    }

    /// Row `id` downloaded as `song` / `artist` (source `gemini`): a 2 000-byte
    /// video and a 3 000-byte audio under the download worker's names.
    pub(crate) async fn give_song(&self, id: i64, youtube_id: &str, song: &str, artist: &str) -> (PathBuf, PathBuf) {
        let video = self.cache().join(video_filename(song, artist, youtube_id, false));
        let audio = self.cache().join(audio_filename(song, artist, youtube_id, false));
        std::fs::write(&video, bytes(2_000, 1)).unwrap();
        std::fs::write(&audio, bytes(3_000, 2)).unwrap();
        crate::db::models::mark_video_processed_pair(
            self.pool(),
            id,
            song,
            artist,
            "gemini",
            false,
            &video.to_string_lossy(),
            &audio.to_string_lossy(),
        )
        .await
        .unwrap();
        (video, audio)
    }

    /// Row `id`'s stems, done, named after its current audio.
    pub(crate) async fn give_stems(&self, id: i64) -> (PathBuf, PathBuf) {
        let audio: String = sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
            .bind(id)
            .fetch_one(self.pool())
            .await
            .unwrap();
        let (vocals, instrumental) = crate::stems::stem_paths(Path::new(&audio));
        std::fs::write(&vocals, bytes(1_500, 3)).unwrap();
        std::fs::write(&instrumental, bytes(1_700, 4)).unwrap();
        crate::db::models_stems::mark_stems_done(
            self.pool(),
            id,
            &vocals.to_string_lossy(),
            &instrumental.to_string_lossy(),
        )
        .await
        .unwrap();
        (vocals, instrumental)
    }

    /// Row `id`'s lyrics at the current pipeline version, from `source`;
    /// returns the `{yt}_lyrics.json` bytes.
    pub(crate) async fn give_lyrics(&self, id: i64, youtube_id: &str, source: &str) -> Vec<u8> {
        let track = sp_core::lyrics::LyricsTrack {
            version: 1,
            source: source.into(),
            language_source: "en".into(),
            language_translation: "sk".into(),
            lines: vec![sp_core::lyrics::LyricsLine {
                start_ms: 1_000,
                end_ms: 4_000,
                en: "Way maker".into(),
                sk: Some("Cestu robíš".into()),
                words: None,
            }],
        };
        let json = serde_json::to_vec(&track).unwrap();
        std::fs::write(self.cache().join(format!("{youtube_id}_lyrics.json")), &json).unwrap();
        crate::db::models::mark_video_lyrics_complete(
            self.pool(),
            id,
            source,
            crate::lyrics::LYRICS_PIPELINE_VERSION,
            None,
            None,
        )
        .await
        .unwrap();
        json
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn set(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value).await.unwrap();
}
```

In `peer/mod.rs` add `use sp_core::config::SETTING_PEER_TRANSFERS_PAUSED;` and to `impl Exchange`:

```rust
    /// `peer_transfers_paused` = "true": no new transfer either way, no hashing.
    pub(crate) async fn transfers_paused(&self) -> bool {
        let raw = crate::db::models::get_setting(&self.pool, SETTING_PEER_TRANSFERS_PAUSED)
            .await
            .ok()
            .flatten();
        sp_core::config::peer_transfers_paused(raw.as_deref())
    }
```

and as the very last lines of `peer/mod.rs`:

```rust
#[cfg(test)]
pub(crate) mod rig;
```

- [ ] **Step 2: Write the failing tests** `crates/sp-server/src/peer/catalog_tests.rs`

```rust
//! #229 `peer::catalog`: what this node lists, from its own rows + the hash cache.

use super::*;
use crate::db::models_peer::{HashEntry, put_hash};
use crate::peer::hash::sha256_hex;
use crate::peer::kind::{ArtifactKind, Job, MEDIA_VERSION, STEMS_VERSION};
use crate::peer::rig::TestNode;
use crate::peer::wire::Catalog;
use std::path::{Path, PathBuf};
use ArtifactKind::{Audio, Lyrics, Metadata, StemInstrumental, StemVocals, Video};

const YT: &str = "aaaaaaaaaaa";

/// Cache `path`'s real sha256, as hashed at `at` ms.
async fn hash(node: &TestNode, path: &Path, at: i64) {
    let bytes = std::fs::read(path).unwrap();
    let entry = HashEntry {
        path: path_key(path),
        size: bytes.len() as i64,
        mtime_ms: 1,
        sha256: sha256_hex(&bytes),
        hashed_at_ms: at,
    };
    put_hash(node.pool(), &entry).await.unwrap();
}

fn kinds(c: &Catalog, youtube_id: &str) -> Vec<ArtifactKind> {
    let mut k: Vec<ArtifactKind> =
        c.artifacts.iter().filter(|a| a.youtube_id == youtube_id).map(|a| a.kind).collect();
    k.sort_by_key(|k| k.as_str());
    k
}

#[tokio::test]
async fn a_file_is_listed_once_hashed_with_its_size_sha_and_version() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let before = build(&node.ex, "snv", None).await.unwrap();
    assert_eq!(kinds(&before, YT), vec![Metadata], "nothing hashed yet");
    hash(&node, &video, 100).await;
    hash(&node, &audio, 100).await;
    let c = build(&node.ex, "snv", None).await.unwrap();
    assert_eq!(c.node, "snv");
    assert_eq!(kinds(&c, YT), vec![Audio, Metadata, Video]);
    let a = c.artifacts.iter().find(|a| a.kind == Audio).unwrap();
    assert_eq!(a.size, 3_000);
    assert_eq!(a.sha256, sha256_hex(&std::fs::read(&audio).unwrap()));
    assert_eq!(a.version, MEDIA_VERSION);
    assert_eq!(a.updated_at.as_deref(), Some("1970-01-01T00:00:00.100Z"));
    assert_eq!(counts(&node.ex).await.unwrap(), CatalogCounts { files: 2, listed: 2 });
}

#[tokio::test]
async fn stems_are_listed_when_done_under_the_audios_name() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (_, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let (vocals, instrumental) = node.give_stems(id).await;
    assert_eq!((vocals.clone(), instrumental.clone()), crate::stems::stem_paths(&audio));
    let files = artifact_files(node.pool(), node.cache(), None).await.unwrap();
    let stems: Vec<(ArtifactKind, u32, PathBuf)> = files
        .iter()
        .filter(|f| f.kind == StemVocals || f.kind == StemInstrumental)
        .map(|f| (f.kind, f.version, f.path.clone()))
        .collect();
    assert_eq!(
        stems,
        vec![(StemVocals, STEMS_VERSION, vocals), (StemInstrumental, STEMS_VERSION, instrumental)]
    );
}

#[tokio::test]
async fn lyrics_are_listed_with_the_rows_pipeline_version() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.give_lyrics(id, YT, "mtl+g35t").await;
    let files = artifact_files(node.pool(), node.cache(), None).await.unwrap();
    let lyrics = files.iter().find(|f| f.kind == Lyrics).unwrap();
    assert_eq!(lyrics.version, crate::lyrics::LYRICS_PIPELINE_VERSION);
    assert_eq!(lyrics.path, node.cache().join(format!("{YT}_lyrics.json")));
}

/// Review Focus 5: a dubbed video's `{yt}_lyrics.json` is the Live-Translate
/// subtitle track, never lyrics.
#[tokio::test]
async fn a_dub_track_is_never_listed_as_lyrics() {
    let node = TestNode::start("snv", None).await;
    let dubbed = node.add_video("ddddddddddd").await;
    node.give_lyrics(dubbed, "ddddddddddd", "mtl+g35t").await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(dubbed)
        .execute(node.pool())
        .await
        .unwrap();
    let track = node.add_video("ttttttttttt").await;
    node.give_lyrics(track, "ttttttttttt", crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE).await;
    let plain = node.add_video(YT).await;
    node.give_lyrics(plain, YT, "mtl+g35t").await;
    let files = artifact_files(node.pool(), node.cache(), None).await.unwrap();
    let with_lyrics: Vec<&str> =
        files.iter().filter(|f| f.kind == Lyrics).map(|f| f.youtube_id.as_str()).collect();
    assert_eq!(with_lyrics, vec![YT]);
}

#[tokio::test]
async fn a_video_in_two_playlists_is_listed_once_and_a_bad_id_never() {
    let node = TestNode::start("snv", None).await;
    let first = node.add_video(YT).await;
    let (video, audio) = node.give_song(first, YT, "Way Maker", "Sinach").await;
    let second = node.add_video_to(2, YT).await;
    crate::db::models::mark_video_processed_pair(
        node.pool(), second, "Way Maker", "Sinach", "gemini", false,
        &video.to_string_lossy(), &audio.to_string_lossy(),
    )
    .await
    .unwrap();
    let odd = node.add_video("not an id").await;
    node.give_song(odd, "not an id", "Odd", "Odd").await;
    let files = artifact_files(node.pool(), node.cache(), None).await.unwrap();
    assert_eq!(files.iter().filter(|f| f.kind == Video).count(), 1);
    assert!(files.iter().all(|f| f.youtube_id == YT));
    let only = artifact_files(node.pool(), node.cache(), Some("bbbbbbbbbbb")).await.unwrap();
    assert!(only.is_empty(), "the id filter");
}

#[tokio::test]
async fn since_lists_only_files_hashed_after_it() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    hash(&node, &video, 100).await;
    hash(&node, &audio, 200).await;
    let at = |since| {
        let ex = node.ex.clone();
        async move { kinds(&build(&ex, "snv", Some(since)).await.unwrap(), YT) }
    };
    assert_eq!(at(99).await, vec![Audio, Metadata, Video]);
    assert_eq!(at(100).await, vec![Audio, Metadata], "hashed exactly at `since` is not after it");
    assert_eq!(at(199).await, vec![Audio, Metadata]);
    assert_eq!(at(200).await, vec![Metadata], "metadata has no time: always listed");
}

#[tokio::test]
async fn metadata_carries_its_version_and_the_sha_of_its_bytes() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    sqlx::query("UPDATE videos SET metadata_source = 'manual' WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let meta = metadata_for(node.pool(), Some(YT)).await.unwrap();
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].song, "Way Maker");
    assert_eq!(meta[0].version(), crate::peer::kind::METADATA_MANUAL);
    let c = build(&node.ex, "snv", None).await.unwrap();
    let a = c.artifacts.iter().find(|a| a.kind == Metadata).unwrap();
    assert_eq!(a.sha256, sha256_hex(&meta[0].to_bytes()));
    assert_eq!(a.size, meta[0].to_bytes().len() as u64);
    assert_eq!(a.version, 2);
    assert_eq!(a.updated_at, None);
}

#[tokio::test]
async fn the_running_jobs_are_listed_under_the_node_name() {
    let node = TestNode::start("snv", None).await;
    let _job = node.ex.announce(YT, Job::Lyrics);
    let c = build(&node.ex, "snv", None).await.unwrap();
    assert_eq!(c.jobs.len(), 1);
    assert_eq!((c.jobs[0].youtube_id.as_str(), c.jobs[0].kind, c.jobs[0].node.as_str()), (YT, Lyrics, "snv"));
}
```

- [ ] **Step 3: Run (CI only):** `cargo test -p sp-server peer::catalog` — Expected now: compile FAIL.

- [ ] **Step 4: Implement** `crates/sp-server/src/peer/catalog.rs`

```rust
//! #229: what this node has, from its own rows. The catalog and the hasher
//! read the same list ([`artifact_files`]); the catalog lists a file only once
//! the hasher holds its sha256, so building it reads no file.
//!
//! - A video's pair and stems: the representative row = the lowest id of the
//!   YouTube id with an audio (rows of one video share files, #136). Stems
//!   only when `stem_status = 'done'`, named after the CURRENT audio
//!   (`stems::stem_paths`, `.claude/rules/song-files.md`).
//! - Lyrics: `{yt}_lyrics.json` at the highest pipeline version of the
//!   video's rows — never when a row of the video is dub-requested or carries
//!   the Live-Translate track (that file is then the dub's subtitles).
//! - Metadata: the representative row with a song; its bytes are
//!   `PeerMetadata::to_bytes`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::Exchange;
use super::hash::sha256_hex;
use super::kind::{ArtifactKind, MEDIA_VERSION, STEMS_VERSION};
use super::wire::{Artifact, Catalog, PeerMetadata, ms_to_rfc3339};
use crate::db::models_peer;
use crate::downloader::cache::is_valid_video_id;

/// One file artifact this node's rows name (not checked on disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    pub youtube_id: String,
    pub kind: ArtifactKind,
    pub version: u32,
    pub path: PathBuf,
}

/// For the status: the files the rows name, and how many the catalog lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCounts {
    pub files: usize,
    pub listed: usize,
}

/// `?1` = an optional YouTube id filter.
const MEDIA_ROWS: &str = "SELECT youtube_id, COALESCE(file_path, ''), audio_file_path, stem_status \
     FROM videos WHERE id IN (SELECT MIN(id) FROM videos WHERE normalized = 1 \
         AND audio_file_path IS NOT NULL AND audio_file_path != '' \
         AND (?1 IS NULL OR youtube_id = ?1) GROUP BY youtube_id) \
     ORDER BY youtube_id";

/// `?1` = an optional YouTube id filter, `?2` = the Live-Translate source.
const LYRICS_ROWS: &str = "SELECT youtube_id, MAX(lyrics_pipeline_version) FROM videos \
     WHERE has_lyrics = 1 AND (?1 IS NULL OR youtube_id = ?1) GROUP BY youtube_id \
     HAVING SUM(CASE WHEN dub_requested = 1 OR lyrics_source = ?2 THEN 1 ELSE 0 END) = 0 \
     ORDER BY youtube_id";

/// `?1` = an optional YouTube id filter.
const METADATA_ROWS: &str = "SELECT youtube_id, song, COALESCE(artist, ''), metadata_source, gemini_failed \
     FROM videos WHERE id IN (SELECT MIN(id) FROM videos WHERE TRIM(COALESCE(song, '')) != '' \
         AND (?1 IS NULL OR youtube_id = ?1) GROUP BY youtube_id) \
     ORDER BY youtube_id";

/// The file artifacts this node's rows name (all, or one video's).
pub async fn artifact_files(
    pool: &SqlitePool,
    cache_dir: &Path,
    youtube_id: Option<&str>,
) -> Result<Vec<ArtifactFile>, sqlx::Error> {
    let mut files = Vec::new();
    let media: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as(MEDIA_ROWS).bind(youtube_id).fetch_all(pool).await?;
    for (yt, video, audio, stem_status) in media {
        if !is_valid_video_id(&yt) {
            continue;
        }
        let audio = PathBuf::from(audio);
        if !video.is_empty() {
            files.push(file(&yt, ArtifactKind::Video, MEDIA_VERSION, PathBuf::from(video)));
        }
        if stem_status.as_deref() == Some("done") {
            let (vocals, instrumental) = crate::stems::stem_paths(&audio);
            files.push(file(&yt, ArtifactKind::StemVocals, STEMS_VERSION, vocals));
            files.push(file(&yt, ArtifactKind::StemInstrumental, STEMS_VERSION, instrumental));
        }
        files.push(file(&yt, ArtifactKind::Audio, MEDIA_VERSION, audio));
    }
    let lyrics: Vec<(String, i64)> = sqlx::query_as(LYRICS_ROWS)
        .bind(youtube_id)
        .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
        .fetch_all(pool)
        .await?;
    for (yt, version) in lyrics {
        if !is_valid_video_id(&yt) {
            continue;
        }
        let path = cache_dir.join(format!("{yt}_lyrics.json"));
        files.push(file(&yt, ArtifactKind::Lyrics, u32::try_from(version).unwrap_or(0), path));
    }
    Ok(files)
}

fn file(youtube_id: &str, kind: ArtifactKind, version: u32, path: PathBuf) -> ArtifactFile {
    ArtifactFile { youtube_id: youtube_id.to_string(), kind, version, path }
}

/// The titles this node holds (all videos, or one).
pub async fn metadata_for(pool: &SqlitePool, youtube_id: Option<&str>) -> Result<Vec<PeerMetadata>, sqlx::Error> {
    let rows: Vec<(String, String, String, Option<String>, i64)> =
        sqlx::query_as(METADATA_ROWS).bind(youtube_id).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter(|row| is_valid_video_id(&row.0))
        .map(|(youtube_id, song, artist, metadata_source, gemini_failed)| PeerMetadata {
            youtube_id,
            song,
            artist,
            metadata_source,
            gemini_failed: gemini_failed != 0,
        })
        .collect())
}

/// This node's catalog as `node`: every hashed file (hashed after `since_ms`
/// when given), every title, every running job.
pub async fn build(ex: &Exchange, node: &str, since_ms: Option<i64>) -> Result<Catalog, sqlx::Error> {
    let files = artifact_files(&ex.pool, &ex.cache_dir, None).await?;
    let hashes = models_peer::all_hashes(&ex.pool).await?;
    let mut artifacts = Vec::new();
    for f in files {
        let Some(h) = hashes.get(&path_key(&f.path)) else {
            continue;
        };
        if since_ms.is_some_and(|since| h.hashed_at_ms <= since) {
            continue;
        }
        artifacts.push(Artifact {
            youtube_id: f.youtube_id,
            kind: f.kind,
            version: f.version,
            size: u64::try_from(h.size).unwrap_or(0),
            sha256: h.sha256.clone(),
            updated_at: Some(ms_to_rfc3339(h.hashed_at_ms)),
        });
    }
    for m in metadata_for(&ex.pool, None).await? {
        let bytes = m.to_bytes();
        artifacts.push(Artifact {
            youtube_id: m.youtube_id.clone(),
            kind: ArtifactKind::Metadata,
            version: m.version(),
            size: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
            updated_at: None,
        });
    }
    Ok(Catalog { node: node.to_string(), artifacts, jobs: ex.board.snapshot(node) })
}

/// The files the rows name, and how many of them are hashed (listed).
pub async fn counts(ex: &Exchange) -> Result<CatalogCounts, sqlx::Error> {
    let files = artifact_files(&ex.pool, &ex.cache_dir, None).await?;
    let hashes = models_peer::all_hashes(&ex.pool).await?;
    let listed = files.iter().filter(|f| hashes.contains_key(&path_key(&f.path))).count();
    Ok(CatalogCounts { files: files.len(), listed })
}

/// A path as the hash cache keys it.
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
```

- [ ] **Step 5: Run (CI only):** `cargo test -p sp-server peer::catalog` — Expected: PASS (8 tests).
- [ ] **Step 6: Commit** — `test(#229): this node's catalog from its rows and the hash cache` then `feat(#229): peer::catalog + test rig`.

### Task 3.4: `peer::hasher`

**Files:**
- Create: `crates/sp-server/src/peer/hasher.rs`, `crates/sp-server/src/peer/hasher_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod hasher;`), `crates/sp-server/src/peer/rig.rs` (add `hash_now`)

**Interfaces:**
- Consumes: `catalog::{artifact_files, path_key}`, `hash::sha256_file`, `models_peer::{all_hashes, put_hash, remove_hash, prune_hashes}`, `NodeConfig`, `Exchange::transfers_paused`.
- Produces: `hasher::{HASH_BYTES_PER_S: u64 = 40 MiB, HashPass { hashed, fresh, missing, changed: usize, pruned: u64, paused: bool }, hash_pass(&Exchange, rate: u64) -> Result<HashPass, sqlx::Error>, should_hash(&Exchange) -> bool (async, pub(crate)), run(Arc<Exchange>, broadcast::Receiver<()>)}`; rig `TestNode::hash_now(&self) -> HashPass`.

- [ ] **Step 1: Add to the rig** (`impl TestNode`):

```rust
    /// One hashing pass, no rate limit.
    pub(crate) async fn hash_now(&self) -> super::hasher::HashPass {
        super::hasher::hash_pass(&self.ex, 0).await.unwrap()
    }
```

- [ ] **Step 2: Write the failing tests** `hasher_tests.rs`

```rust
//! #229 `peer::hasher`.

use super::*;
use crate::db::models_peer::all_hashes;
use crate::peer::catalog::path_key;
use crate::peer::hash::sha256_hex;
use crate::peer::rig::{SNV_KEY, TestNode, bytes};
use std::time::{Duration, UNIX_EPOCH};

const YT: &str = "aaaaaaaaaaa";

fn set_mtime(path: &std::path::Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

#[tokio::test]
async fn a_pass_hashes_each_file_once_then_finds_it_fresh() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    let first = node.hash_now().await;
    assert_eq!(first, HashPass { hashed: 2, ..HashPass::default() });
    let all = all_hashes(node.pool()).await.unwrap();
    assert_eq!(all[&path_key(&audio)].sha256, sha256_hex(&bytes(3_000, 2)));
    assert_eq!(all[&path_key(&video)].size, 2_000);
    let second = node.hash_now().await;
    assert_eq!(second, HashPass { fresh: 2, ..HashPass::default() });
}

#[tokio::test]
async fn a_changed_file_is_hashed_again() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (_, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    std::fs::write(&audio, bytes(3_000, 9)).unwrap();
    set_mtime(&audio, 1_900_000_000);
    let pass = node.hash_now().await;
    assert_eq!((pass.hashed, pass.fresh), (1, 1));
    let all = all_hashes(node.pool()).await.unwrap();
    assert_eq!(all[&path_key(&audio)].sha256, sha256_hex(&bytes(3_000, 9)));
}

#[tokio::test]
async fn a_missing_file_loses_its_entry() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, _) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    std::fs::remove_file(&video).unwrap();
    let pass = node.hash_now().await;
    assert_eq!((pass.missing, pass.fresh), (1, 1));
    assert!(!all_hashes(node.pool()).await.unwrap().contains_key(&path_key(&video)));
}

#[tokio::test]
async fn a_renamed_song_drops_the_old_paths() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    let (video, audio) = node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    let new_video = node.cache().join("Renamed_Sinach_aaaaaaaaaaa_normalized_video.mp4");
    let new_audio = node.cache().join("Renamed_Sinach_aaaaaaaaaaa_normalized_audio.flac");
    std::fs::rename(&video, &new_video).unwrap();
    std::fs::rename(&audio, &new_audio).unwrap();
    sqlx::query("UPDATE videos SET file_path = ?, audio_file_path = ? WHERE id = ?")
        .bind(new_video.to_string_lossy().to_string())
        .bind(new_audio.to_string_lossy().to_string())
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let pass = node.hash_now().await;
    assert_eq!((pass.hashed, pass.pruned), (2, 2));
}

#[tokio::test]
async fn a_paused_node_hashes_nothing() {
    let node = TestNode::start("snv", None).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    crate::db::models::set_setting(node.pool(), "peer_transfers_paused", "true").await.unwrap();
    let pass = node.hash_now().await;
    assert_eq!((pass.hashed, pass.paused), (0, true));
    assert!(all_hashes(node.pool()).await.unwrap().is_empty());
}

#[tokio::test]
async fn only_a_serving_node_that_is_not_paused_hashes() {
    let off = TestNode::start("snv", None).await;
    assert!(!should_hash(&off.ex).await, "not serving");
    let on = TestNode::start("snv", Some(SNV_KEY)).await;
    assert!(should_hash(&on.ex).await);
    crate::db::models::set_setting(on.pool(), "peer_transfers_paused", "true").await.unwrap();
    assert!(!should_hash(&on.ex).await, "paused");
}
```

- [ ] **Step 3: Run (CI only):** `cargo test -p sp-server peer::hasher` — Expected now: compile FAIL.

- [ ] **Step 4: Implement** `crates/sp-server/src/peer/hasher.rs`

```rust
//! #229: the sha256 cache behind the catalog. A file is hashed once per
//! (path, size, mtime), one file at a time, at ≤ [`HASH_BYTES_PER_S`], stat →
//! hash → stat (a file that changed meanwhile is skipped until the next pass),
//! only while this node serves and transfers are not paused. A file whose
//! row is gone or renamed loses its entry; a missing file too. The first pass
//! over SNV's ~115 GB takes ~50 min; later passes hash only new files.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::broadcast;
use tracing::{info, warn};

use super::Exchange;
use super::catalog::{artifact_files, path_key};
use super::config::NodeConfig;
use super::hash::sha256_file;
use super::wire::now_ms;
use crate::db::models_peer::{self, HashEntry};

/// The hasher's read rate: the wall reads its video from the same disk.
pub const HASH_BYTES_PER_S: u64 = 40 * 1024 * 1024;
/// Between passes (the first pass waits too: the 60 s startup quiet, #167).
const PASS_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HashPass {
    pub hashed: usize,
    pub fresh: usize,
    pub missing: usize,
    pub changed: usize,
    pub pruned: u64,
    /// The pass stopped at a pause.
    pub paused: bool,
}

/// One pass over this node's artifact files at `rate` bytes/s (0 = no limit).
pub async fn hash_pass(ex: &Exchange, rate: u64) -> Result<HashPass, sqlx::Error> {
    let files = artifact_files(&ex.pool, &ex.cache_dir, None).await?;
    let hashes = models_peer::all_hashes(&ex.pool).await?;
    let keep: HashSet<String> = files.iter().map(|f| path_key(&f.path)).collect();
    let mut pass = HashPass::default();
    for f in &files {
        let key = path_key(&f.path);
        let Some((size, mtime_ms)) = stat(&f.path).await else {
            pass.missing += 1;
            models_peer::remove_hash(&ex.pool, &key).await?;
            continue;
        };
        if hashes.get(&key).is_some_and(|h| h.size == size && h.mtime_ms == mtime_ms) {
            pass.fresh += 1;
            continue;
        }
        if ex.transfers_paused().await {
            pass.paused = true;
            break;
        }
        let Ok(sha256) = sha256_file(&f.path, rate).await else {
            pass.missing += 1;
            continue;
        };
        if stat(&f.path).await != Some((size, mtime_ms)) {
            pass.changed += 1;
            continue;
        }
        let entry = HashEntry { path: key, size, mtime_ms, sha256, hashed_at_ms: now_ms() };
        models_peer::put_hash(&ex.pool, &entry).await?;
        pass.hashed += 1;
    }
    pass.pruned = models_peer::prune_hashes(&ex.pool, &keep).await?;
    Ok(pass)
}

/// `(size, mtime ms)` of a file, `None` when it cannot be read.
async fn stat(path: &Path) -> Option<(i64, i64)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((i64::try_from(meta.len()).ok()?, i64::try_from(mtime.as_millis()).ok()?))
}

/// The hasher runs only while this node serves and transfers are not paused.
pub(crate) async fn should_hash(ex: &Exchange) -> bool {
    NodeConfig::load(&ex.pool).await.is_ok_and(|c| c.serving()) && !ex.transfers_paused().await
}

/// The hasher's loop: a pass every [`PASS_EVERY`] while [`should_hash`].
#[cfg_attr(test, mutants::skip)] // the 60 s timer loop around hash_pass + should_hash
// (both tested); a terminating test can only watch it exit — same class as ReprocessWorker::run.
pub async fn run(ex: Arc<Exchange>, mut shutdown: broadcast::Receiver<()>) {
    info!("exchange: hasher started");
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = tokio::time::sleep(PASS_EVERY) => {}
        }
        if !should_hash(&ex).await {
            continue;
        }
        tokio::select! {
            _ = shutdown.recv() => break,
            pass = hash_pass(&ex, HASH_BYTES_PER_S) => match pass {
                Ok(p) if p.hashed + p.missing + p.changed > 0 || p.pruned > 0 => info!(
                    hashed = p.hashed, fresh = p.fresh, missing = p.missing,
                    changed = p.changed, pruned = p.pruned, paused = p.paused,
                    "exchange: hashing pass"
                ),
                Ok(_) => {}
                Err(e) => warn!(%e, "exchange: hashing pass failed"),
            },
        }
    }
    info!("exchange: hasher stopped");
}

#[cfg(test)]
#[path = "hasher_tests.rs"]
mod tests;
```

- [ ] **Step 5: Run (CI only):** `cargo test -p sp-server peer::hasher` — Expected: PASS (6 tests).
- [ ] **Step 6: Commit** — `test(#229): the hasher's passes` then `feat(#229): peer::hasher — sha256 cache, one file at a time, rate-limited`.

### Task 3.5: status counts + jobs, spawn the hasher, docs, push

**Files:**
- Modify: `crates/sp-server/src/peer/lan.rs` (+ `lan_tests.rs`), `crates/sp-server/src/lib.rs`, `.claude/rules/peer-exchange.md`

**Interfaces:**
- Produces: `ExchangeStatus.catalog: Option<CatalogCounts>`, `ExchangeStatus.jobs: Vec<CatalogJob>`; `lan::status` reads the pause through `Exchange::transfers_paused`.

- [ ] **Step 1: Write the failing tests** (append to `lan_tests.rs`)

```rust
#[tokio::test]
async fn status_counts_the_catalog_and_lists_the_running_jobs() {
    use crate::peer::kind::{ArtifactKind, Job};
    let node = crate::peer::rig::TestNode::start("snv", Some(crate::peer::rig::SNV_KEY)).await;
    let id = node.add_video("aaaaaaaaaaa").await;
    node.give_song(id, "aaaaaaaaaaa", "Way Maker", "Sinach").await;
    let _job = node.ex.announce("bbbbbbbbbbb", Job::Lyrics);
    let (_, body) = get_status(&node.ex).await;
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert!(s.serving);
    assert_eq!(s.catalog, Some(crate::peer::catalog::CatalogCounts { files: 2, listed: 0 }));
    assert_eq!(s.jobs.len(), 1);
    assert_eq!((s.jobs[0].kind, s.jobs[0].node.as_str()), (ArtifactKind::Lyrics, "snv"));
}

/// The rig's node answers over real HTTP (axum::serve on its port).
#[tokio::test]
async fn a_node_answers_its_status_over_real_http() {
    let node = crate::peer::rig::TestNode::start("snv", None).await;
    let url = format!("{}/api/v1/exchange/status", node.base_url);
    let s: ExchangeStatus = reqwest::get(url).await.unwrap().json().await.unwrap();
    assert_eq!(s.node_name.as_deref(), Some(node.name.as_str()));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::lan` — Expected now: compile FAIL (no `catalog` field).

- [ ] **Step 3: Implement.** In `lan.rs` add the imports `use super::catalog::{self, CatalogCounts};` and `use super::wire::CatalogJob;`, drop the now-unused `SETTING_PEER_TRANSFERS_PAUSED`/`peer_transfers_paused` import, add the fields

```rust
    /// The files this node's rows name and how many its catalog lists
    /// (hashed); `None` when the rows cannot be read.
    pub catalog: Option<CatalogCounts>,
    /// The jobs this node runs now (its catalog lists them).
    pub jobs: Vec<CatalogJob>,
```

and in `status` read `transfers_paused: ex.transfers_paused().await,`, `catalog: catalog::counts(&ex).await.ok(),`, `jobs: ex.board.snapshot(cfg.node_name.as_deref().unwrap_or("")),` (compute `jobs` before `cfg` is moved into the struct literal).

In `crates/sp-server/src/lib.rs`, right after `let exchange = peer::Exchange::new(…);`:

```rust
    tokio::spawn(peer::hasher::run(exchange.clone(), shutdown_tx.subscribe())); // #229
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::` — Expected: PASS.

- [ ] **Step 5: Append to `.claude/rules/peer-exchange.md`**

```markdown
## The catalog and its sha256 cache (`peer::catalog`, `peer::hasher`, V29)

- The catalog comes from this node's OWN rows: per YouTube id the lowest row
  with an audio (rows share files, #136); stems only when `done`, named after
  the CURRENT audio; `{yt}_lyrics.json` never when any row of the video is
  dub-requested or carries `gemini-live-translate` (the file is then the
  dub's subtitles); metadata from the lowest row with a song.
- A file is listed only once `peer_hashes` holds it (path + size + mtime).
  The hasher (`hasher::run`, every 60 s, first pass after 60 s) hashes one
  file at a time at 40 MiB/s, stat → hash → stat, only while serving and not
  paused; a missing file loses its entry; a path no row names is pruned.
  Building a catalog reads no file.
- `?since=` filters files by `hashed_at` (strictly after); metadata entries
  have no time and are always listed; jobs are always listed.
- Tests: `peer::rig::TestNode` = a node with its own DB, cache dir and a real
  port; `give_song` / `give_stems` / `give_lyrics` / `hash_now`.
```

- [ ] **Step 6: Commit** — `test(#229): status counts the catalog` then `feat(#229): exchange status counts + jobs, hasher spawned` then `docs(#229): catalog + hasher rules`.
- [ ] **Step 7: Push the lane**, monitor CI. (Hashing starts only on a node that serves — nowhere yet.)

---

## Lane 4 — Peer API (key guard, catalog / videos / artifact with Range, upload cap) + SNV serving gate

Design comment for #229: *Approach:* `/api/v1/peer/{catalog, videos/{id}, artifact/{id}/{kind}}` as a separate axum router merged into the app; a route-layer guard answers 404 when this node does not serve and 401 without the right `X-SP-Peer-Key` (compared as sha256 digests); files are served by tower-http `ServeFile` (Range, HEAD, 206/416 handled by the library) wrapped in a rate-limited body at `peer_serve_max_mbps` (SNV's uplink also carries the live stream); `peer_transfers_paused` answers 503; every answer is `no-store` (Cloudflare never caches it). *Rejected:* hand-written Range parsing (ServeFile is already a dependency and battle-tested), and serving through the existing CORS-open dashboard router (the peer API must not inherit the dashboard's no-auth stance). *Architektúra:* axum 0.8 router + `middleware::from_fn_with_state`, tower-http `ServeFile::try_call`, `futures::stream::unfold` throttle.

**MAIN SESSION OPS — before this lane's push:** set SNV's identity so the new SNV post-deploy gate (Task 4.4) holds on the first deploy. On dev1:

```bash
# 1. A fresh 64-hex peer key for SNV, minted straight into the secret channel
#    (never printed, never in chat): `openssl rand -hex 32` → secret name
#    `songplayer-peer-key-snv` (receive-files-credentials companion: `secret request` / `secret exec`).
# 2. Inside `secret exec` (the key arrives as $SNV_PEER_KEY):
curl -fsS -X PATCH http://10.77.9.201:8920/api/v1/settings \
  -H 'content-type: application/json' \
  --data "{\"node_name\":\"snv\",\"peer_api_key\":\"$SNV_PEER_KEY\"}"
# 3. Verify (no secret involved):
curl -fsS http://10.77.9.201:8920/api/v1/exchange/status | jq '{node_name, serving, config_error}'
#    → {"node_name":"snv","serving":true,"config_error":null}
```

### Task 4.1: the router, the key guard, `GET /api/v1/peer/catalog`

**Files:**
- Create: `crates/sp-server/src/peer/api.rs`, `crates/sp-server/src/peer/api_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod api;`; `router()` becomes `lan::router(ex.clone()).merge(api::router(ex))`)

**Interfaces:**
- Consumes: `NodeConfig`, `catalog::build`, `wire::rfc3339_to_ms`, `Exchange::transfers_paused`.
- Produces: `api::{PEER_KEY_HEADER: &str = "x-sp-peer-key", router(Arc<Exchange>) -> Router, keys_match(&str, &str) -> bool, CatalogQuery { since: Option<String> }}`; HTTP: `GET /api/v1/peer/catalog[?since=<RFC 3339>]` → 200 `Catalog` JSON, `Cache-Control: no-store`; 404 when not serving; 401 without the key; 400 for a bad `since`.

- [ ] **Step 1: Write the failing tests** `api_tests.rs`

```rust
//! #229 the peer API, driven through the real `peer::router`.

use super::*;
use crate::peer::kind::{ArtifactKind, Job};
use crate::peer::rig::{SNV_KEY, TestNode, bytes};
use crate::peer::wire::Catalog;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use tower::ServiceExt;

const YT: &str = "aaaaaaaaaaa";

async fn call(node: &TestNode, uri: &str, key: Option<&str>, range: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = axum::http::Request::builder().uri(uri);
    if let Some(key) = key {
        req = req.header(PEER_KEY_HEADER, key);
    }
    if let Some(range) = range {
        req = req.header("range", range);
    }
    let resp = crate::peer::router(node.ex.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, headers, body)
}

/// SNV serving one hashed song (`Way Maker` / `Sinach`).
async fn snv_with_song() -> TestNode {
    let node = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = node.add_video(YT).await;
    node.give_song(id, YT, "Way Maker", "Sinach").await;
    node.hash_now().await;
    node
}

#[test]
fn keys_match_only_the_same_key() {
    assert!(keys_match(SNV_KEY, SNV_KEY));
    assert!(!keys_match("", SNV_KEY));
    assert!(!keys_match(&SNV_KEY[..SNV_KEY.len() - 1], SNV_KEY));
    assert!(!keys_match(&format!("{SNV_KEY}x"), SNV_KEY));
}

#[tokio::test]
async fn a_node_that_does_not_serve_answers_404_whatever_the_key() {
    let node = TestNode::start("snv", None).await;
    let (status, _, _) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_key_without_a_node_name_does_not_serve() {
    let node = TestNode::start("snv", Some(SNV_KEY)).await;
    crate::db::models::set_setting(node.pool(), "node_name", "").await.unwrap();
    let (status, _, _) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_request_without_the_key_or_with_another_is_refused() {
    let node = snv_with_song().await;
    let other = "another-peer-key-0123456789abcdef";
    for key in [None, Some(other)] {
        let (status, _, body) = call(&node, "/api/v1/peer/catalog", key, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.is_empty());
    }
}

#[tokio::test]
async fn the_catalog_lists_the_hashed_song_and_the_running_jobs() {
    let node = snv_with_song().await;
    let _job = node.ex.announce("bbbbbbbbbbb", Job::Stems);
    let (status, headers, body) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    let c: Catalog = serde_json::from_slice(&body).unwrap();
    assert_eq!(c.node, "snv");
    let audio = c.artifacts.iter().find(|a| a.kind == ArtifactKind::Audio).unwrap();
    assert_eq!(audio.sha256, crate::peer::hash::sha256_hex(&bytes(3_000, 2)));
    assert_eq!(c.jobs.len(), 2, "the stems job announces both stems");
}

#[tokio::test]
async fn since_must_be_a_time_and_filters_the_files() {
    let node = snv_with_song().await;
    let (status, _, _) = call(&node, "/api/v1/peer/catalog?since=yesterday", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let later = "/api/v1/peer/catalog?since=2999-01-01T00:00:00Z";
    let (status, _, body) = call(&node, later, Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    let c: Catalog = serde_json::from_slice(&body).unwrap();
    assert!(c.artifacts.iter().all(|a| a.kind == ArtifactKind::Metadata));
}
```

(Task 4.2 extends the `wire` import with `PeerVideo`.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::api` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/api.rs` (the videos + artifact routes are added in 4.2 / 4.3; register only `catalog` now)

```rust
//! #229: the peer API — `/api/v1/peer/…`, for other SongPlayer nodes only.
//! Every request carries this node's `peer_api_key` as `X-SP-Peer-Key`
//! (compared as sha256 digests); on the public path (`sp.newlevel.media`)
//! Cloudflare Access sits in front too. A node that does not serve (no
//! `node_name` or no `peer_api_key`) answers 404: the API is off. Every
//! answer is `Cache-Control: no-store`. Keys are never logged.

use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::warn;

use super::Exchange;
use super::catalog;
use super::config::NodeConfig;
use super::wire::rfc3339_to_ms;

/// The header a peer sends this node's `peer_api_key` in.
pub const PEER_KEY_HEADER: &str = "x-sp-peer-key";

pub fn router(ex: Arc<Exchange>) -> Router {
    Router::new()
        .route("/api/v1/peer/catalog", get(catalog_route))
        .route_layer(middleware::from_fn_with_state(ex.clone(), guard))
        .with_state(ex)
}

/// `given` is `expected`: the two sha256 digests are compared, so the time
/// taken says nothing about the key's bytes.
pub fn keys_match(given: &str, expected: &str) -> bool {
    Sha256::digest(given.as_bytes()) == Sha256::digest(expected.as_bytes())
}

async fn guard(State(ex): State<Arc<Exchange>>, req: Request, next: Next) -> Response {
    let cfg = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(error = %e, "peer API: the exchange settings do not hold - answering 404");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let Some(expected) = cfg.serve_key.as_deref().filter(|_| cfg.serving()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let given = req
        .headers()
        .get(PEER_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !keys_match(given, expected) {
        warn!(path = %req.uri().path(), "peer API: refused a request without this node's key");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(req).await
}

#[derive(Debug, Deserialize)]
pub struct CatalogQuery {
    /// Only files listed (hashed) strictly after this RFC 3339 time.
    pub since: Option<String>,
}

async fn catalog_route(State(ex): State<Arc<Exchange>>, Query(q): Query<CatalogQuery>) -> Response {
    let since_ms = match q.since.as_deref() {
        None => None,
        Some(text) => match rfc3339_to_ms(text) {
            Some(ms) => Some(ms),
            None => return (StatusCode::BAD_REQUEST, "since must be an RFC 3339 time").into_response(),
        },
    };
    let node = NodeConfig::load(&ex.pool)
        .await
        .ok()
        .and_then(|c| c.node_name)
        .unwrap_or_default();
    match catalog::build(&ex, &node, since_ms).await {
        Ok(c) => no_store(Json(c).into_response()),
        Err(e) => {
            warn!(%e, "peer API: building the catalog failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn no_store(mut resp: Response) -> Response {
    resp.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
```


`peer/mod.rs`:

```rust
/// Every route of the exchange: the LAN status (no key) and the peer API
/// (`X-SP-Peer-Key`); `lib.rs` merges it into the app's router.
pub fn router(ex: Arc<Exchange>) -> axum::Router {
    lan::router(ex.clone()).merge(api::router(ex))
}
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::api` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): peer API key guard + catalog route` then `feat(#229): peer API router, key guard, GET /api/v1/peer/catalog`.

### Task 4.2: `GET /api/v1/peer/videos/{youtube_id}`

**Files:**
- Modify: `crates/sp-server/src/peer/wire.rs` (add `PeerLyrics`, `PeerVideo`), `crates/sp-server/src/peer/catalog.rs` (add `LYRICS_ROW`, `peer_lyrics`, `peer_video`), `crates/sp-server/src/peer/api.rs` (route), `crates/sp-server/src/peer/api_tests.rs`

**Interfaces:**
- Produces: `wire::PeerLyrics { source: String, pipeline_version: u32, alignment_model: Option<String>, reference: bool, translation_version: u32, translation_gender: Option<String> }`, `wire::PeerVideo { metadata: PeerMetadata, duration_ms: Option<i64>, lyrics: Option<PeerLyrics> }`; `catalog::peer_lyrics(&SqlitePool, &str) -> Result<Option<PeerLyrics>, sqlx::Error>`, `catalog::peer_video(&SqlitePool, &str) -> Result<Option<PeerVideo>, sqlx::Error>`; HTTP 200 `PeerVideo` / 404 unknown / 400 bad id.

- [ ] **Step 1: Write the failing tests** (append to `api_tests.rs`; its import becomes `use crate::peer::wire::{Catalog, PeerVideo};`)

```rust
#[tokio::test]
async fn a_videos_row_carries_its_title_and_lyrics() {
    let node = snv_with_song().await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ?")
        .bind(YT)
        .fetch_one(node.pool())
        .await
        .unwrap();
    node.give_lyrics(id, YT, "mtl+g35t").await;
    sqlx::query("UPDATE videos SET lyrics_reference = 1, lyrics_translation_version = 2, \
                 lyrics_translation_gender = 'm', duration_ms = 241000 WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let (status, headers, body) = call(&node, &format!("/api/v1/peer/videos/{YT}"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    let v: PeerVideo = serde_json::from_slice(&body).unwrap();
    assert_eq!((v.metadata.song.as_str(), v.metadata.artist.as_str()), ("Way Maker", "Sinach"));
    assert_eq!(v.metadata.metadata_source.as_deref(), Some("gemini"));
    assert_eq!(v.duration_ms, Some(241_000));
    let l = v.lyrics.unwrap();
    assert_eq!(l.source, "mtl+g35t");
    assert_eq!(l.pipeline_version, crate::lyrics::LYRICS_PIPELINE_VERSION);
    assert!(l.reference);
    assert_eq!((l.translation_version, l.translation_gender.as_deref()), (2, Some("m")));
}

#[tokio::test]
async fn an_unknown_video_is_404_and_a_bad_id_400() {
    let node = snv_with_song().await;
    let (status, _, _) = call(&node, "/api/v1/peer/videos/bbbbbbbbbbb", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(&node, "/api/v1/peer/videos/short", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_dubbed_videos_row_carries_no_lyrics() {
    let node = snv_with_song().await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ?")
        .bind(YT)
        .fetch_one(node.pool())
        .await
        .unwrap();
    node.give_lyrics(id, YT, "mtl+g35t").await;
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let (_, _, body) = call(&node, &format!("/api/v1/peer/videos/{YT}"), Some(SNV_KEY), None).await;
    let v: PeerVideo = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.lyrics, None);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::api` — Expected now: compile FAIL (`PeerVideo`).

- [ ] **Step 3: Implement.** Append to `wire.rs` (before its test hook):

```rust
/// A video's lyrics row as a node holds it: what an adopting node writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerLyrics {
    pub source: String,
    pub pipeline_version: u32,
    pub alignment_model: Option<String>,
    /// The ★ reference tier (the wall marks it).
    pub reference: bool,
    pub translation_version: u32,
    pub translation_gender: Option<String>,
}

/// `GET /api/v1/peer/videos/{youtube_id}`: a node adopts a title and a lyrics
/// row from it without running the providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerVideo {
    pub metadata: PeerMetadata,
    pub duration_ms: Option<i64>,
    pub lyrics: Option<PeerLyrics>,
}
```

Append to `catalog.rs` (before its test hook; extend the `use super::wire::{…}` with `PeerLyrics, PeerVideo`):

```rust
/// `?1` = the YouTube id, `?2` = the Live-Translate source: the same dub rule
/// as [`LYRICS_ROWS`] (no row of the video dub-requested or Live-Translate).
const LYRICS_ROW: &str = "SELECT lyrics_source, lyrics_pipeline_version, lyrics_alignment_model, \
       lyrics_reference, lyrics_translation_version, lyrics_translation_gender \
     FROM videos WHERE youtube_id = ?1 AND has_lyrics = 1 AND lyrics_source IS NOT NULL \
       AND NOT EXISTS (SELECT 1 FROM videos d WHERE d.youtube_id = ?1 AND d.has_lyrics = 1 \
                       AND (d.dub_requested = 1 OR d.lyrics_source = ?2)) \
     ORDER BY lyrics_pipeline_version DESC, lyrics_processed_at DESC LIMIT 1";

type LyricsRow = (String, i64, Option<String>, i64, i64, Option<String>);

/// The lyrics row of `youtube_id` this node serves, if any.
pub async fn peer_lyrics(pool: &SqlitePool, youtube_id: &str) -> Result<Option<PeerLyrics>, sqlx::Error> {
    let row: Option<LyricsRow> = sqlx::query_as(LYRICS_ROW)
        .bind(youtube_id)
        .bind(crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(source, version, alignment_model, reference, translation, gender)| PeerLyrics {
        source,
        pipeline_version: u32::try_from(version).unwrap_or(0),
        alignment_model,
        reference: reference != 0,
        translation_version: u32::try_from(translation).unwrap_or(0),
        translation_gender: gender,
    }))
}

/// `GET /api/v1/peer/videos/{youtube_id}`'s answer; `None` = no titled row.
pub async fn peer_video(pool: &SqlitePool, youtube_id: &str) -> Result<Option<PeerVideo>, sqlx::Error> {
    let Some(metadata) = metadata_for(pool, Some(youtube_id)).await?.pop() else {
        return Ok(None);
    };
    let duration: Option<Option<i64>> =
        sqlx::query_scalar("SELECT duration_ms FROM videos WHERE youtube_id = ? ORDER BY id LIMIT 1")
            .bind(youtube_id)
            .fetch_optional(pool)
            .await?;
    let lyrics = peer_lyrics(pool, youtube_id).await?;
    Ok(Some(PeerVideo { metadata, duration_ms: duration.flatten(), lyrics }))
}
```

In `api.rs`: add `use axum::extract::Path;` and `use crate::downloader::cache::is_valid_video_id;`, the route `.route("/api/v1/peer/videos/{youtube_id}", get(video_route))` before `.route_layer(…)`, and:

```rust
async fn video_route(State(ex): State<Arc<Exchange>>, Path(youtube_id): Path<String>) -> Response {
    if !is_valid_video_id(&youtube_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match catalog::peer_video(&ex.pool, &youtube_id).await {
        Ok(Some(video)) => no_store(Json(video).into_response()),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!(%e, youtube_id = %youtube_id, "peer API: reading a video failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): GET /api/v1/peer/videos/{id}` then `feat(#229): peer videos route — title + lyrics row a peer adopts`.

### Task 4.3: `GET /api/v1/peer/artifact/{youtube_id}/{kind}` with Range, cap, pause

**Files:**
- Modify: `crates/sp-server/src/peer/throttle.rs` (+ `throttled`), `crates/sp-server/src/peer/throttle_tests.rs`, `crates/sp-server/src/peer/api.rs`, `crates/sp-server/src/peer/api_tests.rs`

**Interfaces:**
- Consumes: `catalog::{artifact_files, metadata_for}`, `sp_core::config::{SETTING_PEER_SERVE_MAX_MBPS, peer_serve_max_mbps}`, `tower_http::services::ServeFile::try_call`.
- Produces: `throttle::throttled(axum::body::Body, rate: u64) -> axum::body::Body`; HTTP: 200 whole file / 206 a `Range` / 416 (ServeFile) / 404 unknown kind or no such file or a dubbed video's lyrics / 400 bad id / 503 + `Retry-After: 600` while paused; the `metadata` kind answers `PeerMetadata::to_bytes` as `application/json`.

- [ ] **Step 1: Write the failing tests.** Append to `throttle_tests.rs`:

```rust
#[tokio::test(start_paused = true)]
async fn a_throttled_body_keeps_its_bytes_and_its_rate() {
    use axum::body::{Body, Bytes};
    let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from(vec![1u8; 1_000])),
        Ok(Bytes::from(vec![2u8; 1_000])),
        Ok(Bytes::from(vec![3u8; 500])),
    ];
    let body = Body::from_stream(futures::stream::iter(chunks));
    let started = tokio::time::Instant::now();
    let out = axum::body::to_bytes(throttled(body, 1_000), usize::MAX).await.unwrap();
    assert_eq!(out.len(), 2_500);
    assert_eq!((out[0], out[1_000], out[2_499]), (1, 2, 3));
    assert_eq!(started.elapsed(), Duration::from_millis(2_500), "2 500 bytes at 1 000 B/s");
}

#[tokio::test(start_paused = true)]
async fn rate_zero_passes_the_body_as_it_comes() {
    let started = tokio::time::Instant::now();
    let body = axum::body::Body::from(vec![7u8; 4_096]);
    let out = axum::body::to_bytes(throttled(body, 0), usize::MAX).await.unwrap();
    assert_eq!(out.len(), 4_096);
    assert_eq!(started.elapsed(), Duration::ZERO);
}
```

Append to `api_tests.rs`:

```rust
fn artifact(kind: &str) -> String {
    format!("/api/v1/peer/artifact/{YT}/{kind}")
}

#[tokio::test]
async fn an_artifact_is_served_whole() {
    let node = snv_with_song().await;
    let (status, headers, body) = call(&node, &artifact("audio"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body, bytes(3_000, 2));
    let (_, _, video) = call(&node, &artifact("video"), Some(SNV_KEY), None).await;
    assert_eq!(video, bytes(2_000, 1));
}

#[tokio::test]
async fn a_range_request_gets_the_rest_of_the_file() {
    let node = snv_with_song().await;
    let (status, headers, body) = call(&node, &artifact("audio"), Some(SNV_KEY), Some("bytes=10-")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 10-2999/3000");
    assert_eq!(body, bytes(3_000, 2)[10..].to_vec());
}

#[tokio::test]
async fn an_unknown_or_absent_kind_is_404_and_a_bad_id_400() {
    let node = snv_with_song().await;
    for kind in ["dub", "unknown", "stem_vocals", "lyrics"] {
        let (status, _, _) = call(&node, &artifact(kind), Some(SNV_KEY), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{kind}");
    }
    let (status, _, _) = call(&node, "/api/v1/peer/artifact/x/audio", Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn metadata_is_served_as_its_canonical_bytes() {
    let node = snv_with_song().await;
    let (status, headers, body) = call(&node, &artifact("metadata"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    let (_, _, cat) = call(&node, "/api/v1/peer/catalog", Some(SNV_KEY), None).await;
    let c: Catalog = serde_json::from_slice(&cat).unwrap();
    let listed = c.artifacts.iter().find(|a| a.kind == ArtifactKind::Metadata).unwrap();
    assert_eq!(crate::peer::hash::sha256_hex(&body), listed.sha256);
}

#[tokio::test]
async fn a_paused_node_answers_503_retry_after() {
    let node = snv_with_song().await;
    crate::db::models::set_setting(node.pool(), "peer_transfers_paused", "true").await.unwrap();
    let (status, headers, _) = call(&node, &artifact("audio"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "600");
}

/// Review Focus 5.
#[tokio::test]
async fn the_lyrics_of_a_dubbed_video_are_not_served() {
    let node = snv_with_song().await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM videos WHERE youtube_id = ?")
        .bind(YT)
        .fetch_one(node.pool())
        .await
        .unwrap();
    node.give_lyrics(id, YT, "mtl+g35t").await;
    let (status, _, _) = call(&node, &artifact("lyrics"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::OK, "plain lyrics are served");
    sqlx::query("UPDATE videos SET dub_requested = 1 WHERE id = ?")
        .bind(id)
        .execute(node.pool())
        .await
        .unwrap();
    let (status, _, _) = call(&node, &artifact("lyrics"), Some(SNV_KEY), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::api peer::throttle` — Expected now: compile FAIL (`throttled`).

- [ ] **Step 3: Implement.** Append to `throttle.rs` (before its test hook):

```rust
/// `body`, sent at no more than `rate` bytes/s (0 = as it comes).
pub fn throttled(body: axum::body::Body, rate: u64) -> axum::body::Body {
    use futures::StreamExt;
    if rate == 0 {
        return body;
    }
    let started = tokio::time::Instant::now();
    let chunks = futures::stream::unfold(
        (body.into_data_stream(), 0u64),
        move |(mut stream, sent)| async move {
            let chunk = stream.next().await?;
            let sent = sent + chunk.as_ref().map_or(0, |b| b.len() as u64);
            tokio::time::sleep(wait_for(sent, started.elapsed(), rate)).await;
            Some((chunk, (stream, sent)))
        },
    );
    axum::body::Body::from_stream(chunks)
}
```

In `api.rs`: imports `use axum::body::Body;`, `use super::kind::ArtifactKind;`, `use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};`, `use sp_core::config::{SETTING_PEER_SERVE_MAX_MBPS, peer_serve_max_mbps};`, `use tower_http::services::ServeFile;`, `use tracing::info;`, `use super::throttle::{mbps_to_bytes, throttled};`; the route `.route("/api/v1/peer/artifact/{youtube_id}/{kind}", get(artifact_route))`; and:

```rust
async fn artifact_route(
    State(ex): State<Arc<Exchange>>,
    Path((youtube_id, kind_name)): Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(kind) = ArtifactKind::parse(&kind_name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !is_valid_video_id(&youtube_id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if ex.transfers_paused().await {
        return (StatusCode::SERVICE_UNAVAILABLE, [(RETRY_AFTER, "600")]).into_response();
    }
    if kind == ArtifactKind::Metadata {
        return match catalog::metadata_for(&ex.pool, Some(&youtube_id)).await {
            Ok(mut found) => match found.pop() {
                Some(m) => no_store(([(CONTENT_TYPE, "application/json")], m.to_bytes()).into_response()),
                None => StatusCode::NOT_FOUND.into_response(),
            },
            Err(e) => {
                warn!(%e, youtube_id = %youtube_id, "peer API: reading a title failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        };
    }
    let files = match catalog::artifact_files(&ex.pool, &ex.cache_dir, Some(&youtube_id)).await {
        Ok(files) => files,
        Err(e) => {
            warn!(%e, youtube_id = %youtube_id, "peer API: reading the rows failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(file) = files.into_iter().find(|f| f.kind == kind) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mbps = crate::db::models::get_setting(&ex.pool, SETTING_PEER_SERVE_MAX_MBPS)
        .await
        .ok()
        .flatten();
    let rate = mbps_to_bytes(peer_serve_max_mbps(mbps.as_deref()));
    match ServeFile::new(&file.path).try_call(req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            info!(
                youtube_id = %youtube_id,
                kind = kind.as_str(),
                status = parts.status.as_u16(),
                "peer API: serving an artifact"
            );
            no_store(Response::from_parts(parts, throttled(Body::new(body), rate)))
        }
        Err(e) => {
            warn!(%e, youtube_id = %youtube_id, kind = kind.as_str(), "peer API: reading an artifact failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::` — Expected: PASS. Note: the default cap (20 Mbit/s = 2.5 MB/s) keeps each 3 000-byte test transfer ≈ 1 ms.
- [ ] **Step 5: Commit** — `test(#229): artifact route — whole, Range, pause, dub lyrics` then `feat(#229): GET /api/v1/peer/artifact with Range, upload cap and pause`.

### Task 4.4: SNV serving gate (post-deploy), docs, push

**Files:**
- Create: `e2e/post-deploy-peer-serving.spec.ts`
- Modify: `.claude/rules/peer-exchange.md`

**Interfaces:**
- Consumes: `GET /api/v1/exchange/status` (`node_name`, `serving`, `config_error`, `catalog.files`).
- Produces: the SNV post-deploy suite (`post-deploy.config.ts` matches `post-deploy*.spec.ts`) fails when SNV stops serving the exchange.

- [ ] **Step 1: Write the spec** `e2e/post-deploy-peer-serving.spec.ts`

```ts
/**
 * #229: SNV serves the node exchange (PP reads its catalog through
 * Cloudflare). LAN read, no key: GET /api/v1/exchange/status — this node is
 * `snv`, its peer API is on, its rows name files for the catalog. The live
 * PP → SNV read through Cloudflare is gated at PP (post-deploy-pp.spec.ts).
 */
import { test, expect } from "@playwright/test";

interface ExchangeStatus {
  node_name: string | null;
  serving: boolean;
  config_error: string | null;
  catalog: { files: number; listed: number } | null;
}

test("SNV serves the node exchange (#229)", async ({ request }) => {
  const resp = await request.get("/api/v1/exchange/status", { timeout: 10_000 });
  expect(resp.status(), "GET /api/v1/exchange/status").toBe(200);
  const s = (await resp.json()) as ExchangeStatus;
  console.log(`[#229 exchange] ${JSON.stringify(s)}`);
  expect(s.config_error, "the exchange settings hold").toBeNull();
  expect(s.node_name, "SNV's node_name").toBe("snv");
  expect(s.serving, "peer_api_key is set at SNV").toBe(true);
  expect(s.catalog?.files ?? 0, "SNV's rows name cached songs").toBeGreaterThan(0);
});
```

Check it lists (Tier-0 allowed): `cd e2e && npx playwright test --config=post-deploy.config.ts --list | grep peer-serving` (symlink `node_modules` from the main checkout in a worktree, `post-deploy-program-state.md`).

- [ ] **Step 2: Append to `.claude/rules/peer-exchange.md`**

```markdown
## The peer API (`peer::api`)

- `GET /api/v1/peer/catalog[?since=]`, `GET /api/v1/peer/videos/{id}`,
  `GET /api/v1/peer/artifact/{id}/{kind}` — every request needs
  `X-SP-Peer-Key` = this node's `peer_api_key` (401 otherwise, compared as
  sha256 digests); a node that does not serve answers 404. On the public
  path Cloudflare Access is in front as well (a service-token policy).
- Files go through tower-http `ServeFile` (Range → 206, HEAD, 416) and a
  throttled body at `peer_serve_max_mbps` (default 20 Mbit/s: SNV's uplink
  carries the live stream). `peer_transfers_paused` → 503 `Retry-After: 600`
  (a transfer already running finishes). Every answer is `no-store`.
- The `metadata` kind answers `PeerMetadata::to_bytes` (its sha is the
  catalog's); the lyrics of a dubbed video are never served (404).
- SNV's post-deploy gate `e2e/post-deploy-peer-serving.spec.ts`: node `snv`,
  serving, catalog files > 0. If it reddens after a settings change, read
  `/api/v1/exchange/status.config_error` first.
- Set SNV's identity once (main session, secret channel): PATCH
  `{"node_name":"snv","peer_api_key":"<64 hex>"}`.
```

- [ ] **Step 3: Commit** — `test(#229): SNV post-deploy gate — the exchange is served` then `docs(#229): peer API rules`.
- [ ] **Step 4: Push the lane**, monitor CI through Deploy to win-resolume + E2E (the new spec must pass on SNV).
- [ ] **Step 5: MAIN SESSION OPS — after the deploy:** on dev1, inside `secret exec` with `$SNV_PEER_KEY`: `curl -fsS -H "x-sp-peer-key: $SNV_PEER_KEY" http://10.77.9.201:8920/api/v1/peer/catalog | jq '{node, artifacts: (.artifacts|length), jobs: (.jobs|length)}'` — artifacts grow as the hasher works (first pass ≈ 50 min over ~115 GB; watch `exchange: hashing pass` in the SongPlayer log).

---

## Lane 5 — Peer client (Cloudflare Access headers, no redirects, Range-resumed sha-checked fetch, probe)

Design comment for #229: *Approach:* one `PeerClient` per process (reqwest, rustls) that sends `X-SP-Peer-Key` and, for a peer with a token, `CF-Access-Client-Id/Secret`; **redirects are never followed** (the Access app has `auto_redirect_to_identity: true`, so a refused token is a 302 to the login page); statuses map to typed `PeerError`s; bodies are bounded and parsed into typed structs; a good catalog is cached 60 s. An artifact goes to `<cache>/peer/<yt>_<kind>_<sha16>.part` (an older copy's part is never resumed into a newer one), resumed with `Range: bytes=N-` (a 206 must start at N, a 200 restarts), bounded by the catalog's size, sha-checked at the end (a mismatch drops the part and the cached catalog); one transfer at a time per peer; refused while paused. `POST /api/v1/exchange/probe` reads every peer's catalog now — the live gate. *Rejected:* following redirects and sniffing the HTML (fragile, and the login page is a 200); a total timeout on an artifact (a 300 MB video at 20 Mbit/s takes 2 min — a per-read 60 s timeout bounds a stall instead). *Architektúra:* reqwest 0.12 (`redirect::Policy::none`, `connect_timeout`, `read_timeout`, `Response::chunk`), tokio fs, `peer::hash`.

### Task 5.1: `peer::client` — catalog + video reads

**Files:**
- Create: `crates/sp-server/src/peer/client.rs`, `crates/sp-server/src/peer/client_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod client;`; `Exchange` gains `pub(crate) client: client::PeerClient`, built in `new()` with `client::PeerClient::new()`)

**Interfaces:**
- Consumes: `api::PEER_KEY_HEADER`, `config::PeerConfig`, `wire::{Catalog, PeerVideo, ms_to_rfc3339, now_ms}`.
- Produces: `client::{CATALOG_TTL = 60 s, MAX_CATALOG_BYTES = 32 MiB, PeerError { Unreachable(String), AccessRefused(u16), KeyRefused, NotFound, Paused, BadResponse(String), ShaMismatch { expected, got }, Io(String) } (+ From<io::Error>, From<sqlx::Error>), status_error(u16) -> Option<PeerError>, fresh(at: Instant, now: Instant) -> bool, LastRead { ok, at: String, artifacts, jobs: usize, latency_ms: u64, error: Option<String> }, PeerClient}` with `pub(crate) fn new()`, `pub(crate) fn with_max_catalog_bytes(usize)`, `pub(crate) fn get(&PeerConfig, &str) -> reqwest::RequestBuilder`, `read_catalog(&PeerConfig) -> Result<Arc<Catalog>, PeerError>`, `catalog(&PeerConfig)` (cached), `forget_catalog(&str)`, `last_reads() -> HashMap<String, LastRead>`, `video(&PeerConfig, &str) -> Result<PeerVideo, PeerError>`; `pub(crate) fn lock`, `pub(crate) fn unreachable_err(reqwest::Error) -> PeerError`.

- [ ] **Step 1: Write the failing tests** `client_tests.rs`

```rust
//! #229 `peer::client`: wiremock stands in for Cloudflare Access + a peer;
//! the rig for a real node.

use super::*;
use crate::peer::config::PeerConfig;
use crate::peer::kind::ArtifactKind;
use crate::peer::rig::{SNV_KEY, TestNode};
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn peer_at(base_url: &str, with_token: bool) -> PeerConfig {
    PeerConfig {
        name: "snv".into(),
        base_url: base_url.into(),
        key: SNV_KEY.into(),
        cf_client_id: with_token.then(|| "pp-client.access".to_string()),
        cf_client_secret: with_token.then(|| "pp-cf-secret".to_string()),
    }
}

fn catalog_body() -> String {
    format!(
        r#"{{"node":"snv","artifacts":[
            {{"youtube_id":"aaaaaaaaaaa","kind":"audio","version":1,"size":3000,"sha256":"{SHA}"}},
            {{"youtube_id":"aaaaaaaaaaa","kind":"dub","version":1,"size":9,"sha256":"{SHA}"}}],
          "jobs":[]}}"#
    )
}

async fn serves_catalog(server: &MockServer, body: String, times: u64) {
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(times)
        .mount(server)
        .await;
}

#[test]
fn every_status_means_its_error() {
    assert_eq!(status_error(200), None);
    assert_eq!(status_error(206), None);
    for s in [300, 301, 302, 307, 399, 403] {
        assert_eq!(status_error(s), Some(PeerError::AccessRefused(s)), "{s}");
    }
    assert_eq!(status_error(401), Some(PeerError::KeyRefused));
    assert_eq!(status_error(404), Some(PeerError::NotFound));
    assert_eq!(status_error(503), Some(PeerError::Paused));
    for s in [204, 299, 400, 402, 416, 500, 502] {
        assert_eq!(status_error(s), Some(PeerError::BadResponse(format!("HTTP {s}"))), "{s}");
    }
}

#[test]
fn a_cached_catalog_is_fresh_for_the_ttl_only() {
    let t = Instant::now();
    assert!(fresh(t, t));
    assert!(fresh(t, t + CATALOG_TTL - Duration::from_nanos(1)));
    assert!(!fresh(t, t + CATALOG_TTL));
}

/// Review Focus 1: Access refuses a bad / missing service token with a 302 to
/// its login page. It is a refusal, never followed, never parsed.
#[tokio::test]
async fn a_cloudflare_login_redirect_is_access_refused_and_never_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/cdn-cgi/access/login", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cdn-cgi/access/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
        .expect(0)
        .mount(&server)
        .await;
    let client = PeerClient::new();
    let err = client.read_catalog(&peer_at(&server.uri(), true)).await.unwrap_err();
    assert_eq!(err, PeerError::AccessRefused(302));
    let read = &client.last_reads()["snv"];
    assert!(!read.ok);
    assert!(read.error.as_deref().unwrap().contains("Cloudflare Access"));
}

#[tokio::test]
async fn a_403_is_access_refused_and_a_401_the_key() {
    for (status, want) in [(403, PeerError::AccessRefused(403)), (401, PeerError::KeyRefused)] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let err = PeerClient::new().read_catalog(&peer_at(&server.uri(), false)).await.unwrap_err();
        assert_eq!(err, want);
    }
}

#[tokio::test]
async fn the_key_and_the_service_token_travel_as_headers() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 1).await;
    PeerClient::new().read_catalog(&peer_at(&server.uri(), true)).await.unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    let header = |name: &str| req.headers.get(name).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(header("x-sp-peer-key").as_deref(), Some(SNV_KEY));
    assert_eq!(header("cf-access-client-id").as_deref(), Some("pp-client.access"));
    assert_eq!(header("cf-access-client-secret").as_deref(), Some("pp-cf-secret"));
}

#[tokio::test]
async fn no_access_headers_without_a_token() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 1).await;
    PeerClient::new().read_catalog(&peer_at(&server.uri(), false)).await.unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.headers.get("cf-access-client-id").is_none());
    assert!(req.headers.get("cf-access-client-secret").is_none());
}

/// Review Focus 3, over HTTP.
#[tokio::test]
async fn a_newer_peers_catalog_keeps_what_this_node_knows() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 1).await;
    let c = PeerClient::new().read_catalog(&peer_at(&server.uri(), false)).await.unwrap();
    assert_eq!(c.artifacts.len(), 1);
    assert_eq!(c.artifacts[0].kind, ArtifactKind::Audio);
}

#[tokio::test]
async fn a_catalog_over_the_bound_is_refused_at_the_exact_byte() {
    let body = catalog_body();
    let server = MockServer::start().await;
    serves_catalog(&server, body.clone(), 2).await;
    let peer = peer_at(&server.uri(), false);
    assert!(PeerClient::with_max_catalog_bytes(body.len()).read_catalog(&peer).await.is_ok());
    let err = PeerClient::with_max_catalog_bytes(body.len() - 1).read_catalog(&peer).await.unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
}

#[tokio::test]
async fn a_non_json_answer_is_a_bad_response_that_quotes_nothing() {
    let server = MockServer::start().await;
    serves_catalog(&server, "\"leaked-text\"".to_string(), 1).await;
    let err = PeerClient::new().read_catalog(&peer_at(&server.uri(), false)).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("line 1") && !text.contains("leaked-text"), "{text}");
}

#[tokio::test]
async fn an_unreachable_peer_is_unreachable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let err = PeerClient::new().read_catalog(&peer_at(&base, false)).await.unwrap_err();
    assert!(matches!(err, PeerError::Unreachable(_)), "{err:?}");
}

#[tokio::test]
async fn the_catalog_is_read_once_per_ttl_until_forgotten() {
    let server = MockServer::start().await;
    serves_catalog(&server, catalog_body(), 2).await;
    let client = PeerClient::new();
    let peer = peer_at(&server.uri(), false);
    client.catalog(&peer).await.unwrap();
    client.catalog(&peer).await.unwrap();
    client.forget_catalog("snv");
    client.catalog(&peer).await.unwrap();
    // MockServer verifies `.expect(2)` when it drops.
}

#[tokio::test]
async fn a_failed_read_is_not_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2)
        .mount(&server)
        .await;
    let client = PeerClient::new();
    let peer = peer_at(&server.uri(), false);
    assert!(client.catalog(&peer).await.is_err());
    assert!(client.catalog(&peer).await.is_err());
}

#[tokio::test]
async fn reads_a_real_nodes_catalog_and_video_row() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video("aaaaaaaaaaa").await;
    snv.give_song(id, "aaaaaaaaaaa", "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let client = PeerClient::new();
    let peer = snv.as_peer(SNV_KEY);
    let c = client.read_catalog(&peer).await.unwrap();
    assert_eq!(c.node, "snv");
    assert!(c.artifacts.iter().any(|a| a.kind == ArtifactKind::Video));
    let v = client.video(&peer, "aaaaaaaaaaa").await.unwrap();
    assert_eq!(v.metadata.song, "Way Maker");
    let read = &client.last_reads()["snv"];
    assert!(read.ok && read.artifacts == c.artifacts.len() && read.error.is_none());
    assert_eq!(client.video(&peer, "bbbbbbbbbbb").await.unwrap_err(), PeerError::NotFound);
}

#[tokio::test]
async fn a_bad_youtube_id_is_never_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let err = PeerClient::new().video(&peer_at(&server.uri(), false), "../x").await.unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::client` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/client.rs`

```rust
//! #229: this node asking its peers — a catalog, a video's row (an artifact:
//! `fetch.rs`). A peer behind Cloudflare Access gets its service token as
//! `CF-Access-Client-Id/Secret`; redirects are NEVER followed, because Access
//! refuses a bad token with a 302 to its login page (the app has
//! `auto_redirect_to_identity`), which must read as a refusal, never as a
//! catalog. Bodies are bounded and parsed into typed structs; a parse error
//! names only its position. A good catalog is kept [`CATALOG_TTL`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::api::PEER_KEY_HEADER;
use super::config::PeerConfig;
use super::wire::{Catalog, PeerVideo, ms_to_rfc3339, now_ms};
use crate::downloader::cache::is_valid_video_id;

/// How long a good catalog read is reused.
pub const CATALOG_TTL: Duration = Duration::from_secs(60);
/// The largest catalog read (SNV's ~1 000 songs × 6 kinds ≈ 1 MB).
pub const MAX_CATALOG_BYTES: usize = 32 << 20;
const MAX_VIDEO_BYTES: usize = 64 << 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Between two reads of a body; an artifact has no total bound (minutes at the cap).
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// A catalog or a video row, whole.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CF_ACCESS_CLIENT_ID: &str = "cf-access-client-id";
const CF_ACCESS_CLIENT_SECRET: &str = "cf-access-client-secret";

/// Why a peer did not give what was asked. The text never holds a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    #[error("unreachable: {0}")]
    Unreachable(String),
    #[error("refused by Cloudflare Access (HTTP {0}) - check the service token")]
    AccessRefused(u16),
    #[error("refused this node's peer key (HTTP 401)")]
    KeyRefused,
    #[error("not found (HTTP 404); for a catalog: the peer API is off there")]
    NotFound,
    #[error("paused there (HTTP 503)")]
    Paused,
    #[error("bad answer: {0}")]
    BadResponse(String),
    #[error("sha256 mismatch: the catalog says {expected}, the bytes are {got}")]
    ShaMismatch { expected: String, got: String },
    #[error("local: {0}")]
    Io(String),
}

impl From<std::io::Error> for PeerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<sqlx::Error> for PeerError {
    fn from(e: sqlx::Error) -> Self {
        Self::Io(format!("database: {e}"))
    }
}

/// The error an HTTP status means; `None` for a success.
pub fn status_error(status: u16) -> Option<PeerError> {
    match status {
        200 | 206 => None,
        300..=399 | 403 => Some(PeerError::AccessRefused(status)),
        401 => Some(PeerError::KeyRefused),
        404 => Some(PeerError::NotFound),
        503 => Some(PeerError::Paused),
        other => Some(PeerError::BadResponse(format!("HTTP {other}"))),
    }
}

/// A catalog read at `at` is still good at `now`.
pub fn fresh(at: Instant, now: Instant) -> bool {
    now.duration_since(at) < CATALOG_TTL
}

/// The last catalog read of a peer, for the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRead {
    pub ok: bool,
    pub at: String,
    pub artifacts: usize,
    pub jobs: usize,
    pub latency_ms: u64,
    pub error: Option<String>,
}

pub struct PeerClient {
    http: reqwest::Client,
    max_catalog_bytes: usize,
    cache: Mutex<HashMap<String, (Instant, Arc<Catalog>)>>,
    reads: Mutex<HashMap<String, LastRead>>,
    /// One transfer at a time per peer (`fetch.rs`).
    pub(crate) slots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl PeerClient {
    pub(crate) fn new() -> Self {
        Self::with_max_catalog_bytes(MAX_CATALOG_BYTES)
    }

    pub(crate) fn with_max_catalog_bytes(max_catalog_bytes: usize) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(format!("SongPlayer/{}", sp_core::config::VERSION))
            .build()
            .expect("the peer HTTP client builds from static options");
        Self {
            http,
            max_catalog_bytes,
            cache: Mutex::new(HashMap::new()),
            reads: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// A GET of `path` on `peer`, with its key and (when it has one) its Access token.
    pub(crate) fn get(&self, peer: &PeerConfig, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", peer.base_url.trim_end_matches('/'));
        let mut req = self.http.get(url).header(PEER_KEY_HEADER, &peer.key);
        if let (Some(id), Some(secret)) = (&peer.cf_client_id, &peer.cf_client_secret) {
            req = req.header(CF_ACCESS_CLIENT_ID, id).header(CF_ACCESS_CLIENT_SECRET, secret);
        }
        req
    }

    /// `peer`'s catalog, read now; a good one refreshes the cache. The outcome is kept for the status.
    pub async fn read_catalog(&self, peer: &PeerConfig) -> Result<Arc<Catalog>, PeerError> {
        let started = Instant::now();
        let result = self.catalog_now(peer).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let read = match &result {
            Ok(c) => LastRead {
                ok: true,
                at: ms_to_rfc3339(now_ms()),
                artifacts: c.artifacts.len(),
                jobs: c.jobs.len(),
                latency_ms,
                error: None,
            },
            Err(e) => LastRead {
                ok: false,
                at: ms_to_rfc3339(now_ms()),
                artifacts: 0,
                jobs: 0,
                latency_ms,
                error: Some(e.to_string()),
            },
        };
        lock(&self.reads).insert(peer.name.clone(), read);
        if let Ok(c) = &result {
            lock(&self.cache).insert(peer.name.clone(), (Instant::now(), Arc::clone(c)));
        }
        result
    }

    async fn catalog_now(&self, peer: &PeerConfig) -> Result<Arc<Catalog>, PeerError> {
        let resp = self
            .get(peer, "/api/v1/peer/catalog")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(unreachable_err)?;
        if let Some(e) = status_error(resp.status().as_u16()) {
            return Err(e);
        }
        let body = read_bounded(resp, self.max_catalog_bytes).await?;
        let catalog: Catalog = serde_json::from_slice(&body).map_err(|e| {
            PeerError::BadResponse(format!("not a catalog (line {}, column {})", e.line(), e.column()))
        })?;
        Ok(Arc::new(catalog.sanitized()))
    }

    /// `peer`'s catalog, read at most once per [`CATALOG_TTL`] (only a good read is kept).
    pub async fn catalog(&self, peer: &PeerConfig) -> Result<Arc<Catalog>, PeerError> {
        let now = Instant::now();
        let cached = lock(&self.cache)
            .get(&peer.name)
            .filter(|(at, _)| fresh(*at, now))
            .map(|(_, c)| Arc::clone(c));
        match cached {
            Some(c) => Ok(c),
            None => self.read_catalog(peer).await,
        }
    }

    /// Drop `peer`'s cached catalog (after a sha mismatch: it may be stale).
    pub fn forget_catalog(&self, peer: &str) {
        lock(&self.cache).remove(peer);
    }

    pub fn last_reads(&self) -> HashMap<String, LastRead> {
        lock(&self.reads).clone()
    }

    /// `GET /api/v1/peer/videos/{youtube_id}` on `peer`.
    pub async fn video(&self, peer: &PeerConfig, youtube_id: &str) -> Result<PeerVideo, PeerError> {
        if !is_valid_video_id(youtube_id) {
            return Err(PeerError::BadResponse(format!("{youtube_id:?} is no YouTube id")));
        }
        let resp = self
            .get(peer, &format!("/api/v1/peer/videos/{youtube_id}"))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(unreachable_err)?;
        if let Some(e) = status_error(resp.status().as_u16()) {
            return Err(e);
        }
        let body = read_bounded(resp, MAX_VIDEO_BYTES).await?;
        serde_json::from_slice(&body).map_err(|e| {
            PeerError::BadResponse(format!("not a video row (line {}, column {})", e.line(), e.column()))
        })
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A transport error, without its URL.
pub(crate) fn unreachable_err(e: reqwest::Error) -> PeerError {
    PeerError::Unreachable(e.without_url().to_string())
}

/// The whole body, refused once it passes `max` bytes.
async fn read_bounded(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, PeerError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(unreachable_err)? {
        if body.len() + chunk.len() > max {
            return Err(PeerError::BadResponse(format!("the answer is over {max} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
```

In `peer/mod.rs`: field `pub(crate) client: client::PeerClient,` and `client: client::PeerClient::new(),` in `new()`.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::client` — Expected: PASS (14 tests).
- [ ] **Step 5: Commit** — `test(#229): peer client — Access refusals, headers, bounds, cache` then `feat(#229): peer::client — catalog and video reads, no redirects`.


### Task 5.2: `peer::fetch` — Range-resumed, sha-checked, one per peer, pausable

**Files:**
- Create: `crates/sp-server/src/peer/fetch.rs`, `crates/sp-server/src/peer/fetch_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod fetch;`)

**Interfaces:**
- Consumes: `client::{PeerClient, PeerError, status_error, lock, unreachable_err}`, `hash::sha256_file`, `wire::{Artifact, is_sha256_hex}`, `Exchange::transfers_paused`.
- Produces: `fetch::{part_name(&Artifact) -> Option<String>, content_range_start(&str) -> Option<u64>}`; `PeerClient::{fetch(&PeerConfig, &Artifact, parts_dir: &Path) -> Result<PathBuf, PeerError>, slot(&str) -> Arc<tokio::sync::Mutex<()>> (pub(crate))}`; `Exchange::{parts_dir(&self) -> PathBuf (pub(crate)), fetch(&self, &PeerConfig, &Artifact) -> Result<PathBuf, PeerError>}` — the returned path is a verified part in `<cache>/peer/`; the CALLER renames it into place.

- [ ] **Step 1: Write the failing tests** `fetch_tests.rs`

```rust
//! #229 `peer::fetch`.

use super::*;
use crate::peer::client::{PeerClient, PeerError};
use crate::peer::config::PeerConfig;
use crate::peer::hash::sha256_hex;
use crate::peer::kind::ArtifactKind;
use crate::peer::rig::{SNV_KEY, TestNode, bytes};
use crate::peer::wire::Artifact;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const YT: &str = "aaaaaaaaaaa";
const AUDIO_PATH: &str = "/api/v1/peer/artifact/aaaaaaaaaaa/audio";

fn body() -> Vec<u8> {
    bytes(1_000, 5)
}

fn artifact_of(data: &[u8]) -> Artifact {
    Artifact {
        youtube_id: YT.into(),
        kind: ArtifactKind::Audio,
        version: 1,
        size: data.len() as u64,
        sha256: sha256_hex(data),
        updated_at: None,
    }
}

fn peer_at(base_url: &str) -> PeerConfig {
    PeerConfig { name: "snv".into(), base_url: base_url.into(), key: SNV_KEY.into(), cf_client_id: None, cf_client_secret: None }
}

fn part_of(dir: &std::path::Path, a: &Artifact) -> std::path::PathBuf {
    dir.join(part_name(a).unwrap())
}

#[test]
fn a_part_is_named_by_video_kind_and_sha() {
    let a = artifact_of(&body());
    assert_eq!(part_name(&a), Some(format!("{YT}_audio_{}.part", &a.sha256[..16])));
    assert_eq!(part_name(&Artifact { youtube_id: "../x".into(), ..a.clone() }), None);
    assert_eq!(part_name(&Artifact { sha256: "nothex".into(), ..a.clone() }), None);
    assert_eq!(part_name(&Artifact { kind: ArtifactKind::Unknown, ..a }), None);
}

#[test]
fn content_range_names_its_first_byte() {
    assert_eq!(content_range_start("bytes 300-999/1000"), Some(300));
    assert_eq!(content_range_start("bytes 0-0/1"), Some(0));
    assert_eq!(content_range_start("bytes */1000"), None);
    assert_eq!(content_range_start("items 3-4/5"), None);
}

#[tokio::test]
async fn fetches_a_real_nodes_artifact_and_checks_its_sha() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let client = PeerClient::new();
    let peer = snv.as_peer(SNV_KEY);
    let c = client.read_catalog(&peer).await.unwrap();
    let audio = c.artifacts.iter().find(|a| a.kind == ArtifactKind::Audio).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let part = client.fetch(&peer, audio, dir.path()).await.unwrap();
    assert_eq!(part, part_of(dir.path(), audio));
    assert_eq!(std::fs::read(&part).unwrap(), bytes(3_000, 2));
}

#[tokio::test]
async fn resumes_a_part_with_a_range_request() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(AUDIO_PATH))
        .and(header("range", "bytes=300-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 300-999/1000")
                .set_body_bytes(data[300..].to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data[..300]).unwrap();
    let part = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn a_206_from_another_byte_is_refused_and_the_part_dropped() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-999/1000")
                .set_body_bytes(data.clone()),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data[..300]).unwrap();
    let err = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
    assert!(!part_of(dir.path(), &a).exists());
}

#[tokio::test]
async fn a_server_that_ignores_range_restarts_the_part() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data[..300]).unwrap();
    let part = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn a_complete_part_is_checked_without_a_request() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).expect(0).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(part_of(dir.path(), &a), &data).unwrap();
    let part = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn a_part_longer_than_the_artifact_is_fetched_again() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut longer = data.clone();
    longer.push(0);
    std::fs::write(part_of(dir.path(), &a), &longer).unwrap();
    let part = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap();
    assert_eq!(std::fs::read(part).unwrap(), data);
}

#[tokio::test]
async fn one_byte_over_the_catalog_size_is_refused_and_dropped() {
    let data = body();
    let a = artifact_of(&data);
    let mut over = data.clone();
    over.push(9);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(over))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
    assert!(!part_of(dir.path(), &a).exists());
}

#[tokio::test]
async fn a_short_body_keeps_the_part_for_the_next_attempt() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data[..700].to_vec()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let err = PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap_err();
    assert!(matches!(err, PeerError::BadResponse(_)), "{err:?}");
    assert_eq!(std::fs::read(part_of(dir.path(), &a)).unwrap(), data[..700].to_vec());
}

#[tokio::test]
async fn a_sha_mismatch_drops_the_part_and_the_cached_catalog() {
    let data = body();
    let mut lying = artifact_of(&data);
    lying.sha256 = sha256_hex(b"something else");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(AUDIO_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/peer/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"node":"snv"}"#))
        .expect(2)
        .mount(&server)
        .await;
    let client = PeerClient::new();
    let peer = peer_at(&server.uri());
    client.catalog(&peer).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let err = client.fetch(&peer, &lying, dir.path()).await.unwrap_err();
    assert!(matches!(err, PeerError::ShaMismatch { .. }), "{err:?}");
    assert!(!part_of(dir.path(), &lying).exists());
    client.catalog(&peer).await.unwrap(); // read again: the mismatch forgot the cache
}

#[tokio::test]
async fn a_part_of_an_older_copy_is_dropped_other_kinds_kept() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let older = dir.path().join(format!("{YT}_audio_ffffffffffffffff.part"));
    let other_kind = dir.path().join(format!("{YT}_video_ffffffffffffffff.part"));
    std::fs::write(&older, b"old").unwrap();
    std::fs::write(&other_kind, b"video part").unwrap();
    PeerClient::new().fetch(&peer_at(&server.uri()), &a, dir.path()).await.unwrap();
    assert!(!older.exists());
    assert!(other_kind.exists());
}

#[tokio::test]
async fn one_transfer_at_a_time_per_peer() {
    let data = body();
    let a = artifact_of(&data);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let client = std::sync::Arc::new(PeerClient::new());
    let dir = tempfile::tempdir().unwrap();
    let slot = client.slot("snv");
    let held = slot.lock().await;
    let (c2, peer, dir2, a2) = (client.clone(), peer_at(&server.uri()), dir.path().to_path_buf(), a.clone());
    let task = tokio::spawn(async move { c2.fetch(&peer, &a2, &dir2).await });
    let waited = tokio::time::timeout(Duration::from_secs(1), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(waited.is_err(), "no request while another transfer holds the peer");
    drop(held);
    task.await.unwrap().unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_paused_node_fetches_nothing() {
    let pp = TestNode::start("pp", None).await;
    crate::db::models::set_setting(pp.pool(), "peer_transfers_paused", "true").await.unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;
    let err = pp.ex.fetch(&peer_at(&server.uri()), &artifact_of(&body())).await.unwrap_err();
    assert_eq!(err, PeerError::Paused);
    assert_eq!(pp.ex.parts_dir(), pp.cache().join("peer"));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::fetch` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/fetch.rs`

```rust
//! #229: one artifact from a peer into `<cache>/peer/` — a subdir the cache
//! self-heal and the phase-0 copy never touch. The part is named by the
//! artifact's sha, so a part of an older copy is never resumed into a newer
//! one; it resumes with `Range: bytes=N-` (a 206 must start at N, a 200
//! restarts), stops at the catalog's size, and is sha-checked at the end (a
//! mismatch drops it and the cached catalog). One transfer at a time per
//! peer. The caller renames the verified part into place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tracing::info;

use super::Exchange;
use super::client::{PeerClient, PeerError, lock, status_error, unreachable_err};
use super::config::PeerConfig;
use super::hash::sha256_file;
use super::kind::ArtifactKind;
use super::wire::{Artifact, is_sha256_hex};
use crate::downloader::cache::is_valid_video_id;

/// `<yt>_<kind>_<first 16 sha digits>.part`, or `None` for an artifact this
/// node cannot name safely.
pub fn part_name(a: &Artifact) -> Option<String> {
    let nameable = is_valid_video_id(&a.youtube_id)
        && is_sha256_hex(&a.sha256)
        && a.kind != ArtifactKind::Unknown;
    nameable.then(|| format!("{}_{}_{}.part", a.youtube_id, a.kind.as_str(), &a.sha256[..16]))
}

/// The first byte of a `Content-Range: bytes <first>-<last>/<size>`.
pub fn content_range_start(value: &str) -> Option<u64> {
    value.strip_prefix("bytes ")?.split('-').next()?.trim().parse().ok()
}

impl PeerClient {
    /// The transfer slot of `peer` (one transfer at a time per peer).
    pub(crate) fn slot(&self, peer: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(lock(&self.slots).entry(peer.to_string()).or_default())
    }

    /// Artifact `a` of `peer` as a verified part in `parts_dir`.
    pub async fn fetch(&self, peer: &PeerConfig, a: &Artifact, parts_dir: &Path) -> Result<PathBuf, PeerError> {
        let name = part_name(a)
            .ok_or_else(|| PeerError::BadResponse("an artifact this node cannot name".into()))?;
        let slot = self.slot(&peer.name);
        let _turn = slot.lock().await;
        tokio::fs::create_dir_all(parts_dir).await?;
        drop_older_parts(parts_dir, a, &name).await;
        let part = parts_dir.join(&name);
        let mut have = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
        if have > a.size {
            tokio::fs::remove_file(&part).await?;
            have = 0;
        }
        if have < a.size {
            self.download(peer, a, &part, have).await?;
        }
        let got = sha256_file(&part, 0).await?;
        if got != a.sha256 {
            let _ = tokio::fs::remove_file(&part).await;
            self.forget_catalog(&peer.name);
            return Err(PeerError::ShaMismatch { expected: a.sha256.clone(), got });
        }
        Ok(part)
    }

    async fn download(&self, peer: &PeerConfig, a: &Artifact, part: &Path, have: u64) -> Result<(), PeerError> {
        let path = format!("/api/v1/peer/artifact/{}/{}", a.youtube_id, a.kind.as_str());
        let mut req = self.get(peer, &path);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = req.send().await.map_err(unreachable_err)?;
        let status = resp.status().as_u16();
        if let Some(e) = status_error(status) {
            return Err(e);
        }
        let resumed = status == 206;
        if resumed {
            let start = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(content_range_start);
            if start != Some(have) {
                let _ = tokio::fs::remove_file(part).await;
                return Err(PeerError::BadResponse(format!("asked from byte {have}, got {start:?}")));
            }
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(part)
            .await?;
        let mut written = if resumed { have } else { 0 };
        while let Some(chunk) = resp.chunk().await.map_err(unreachable_err)? {
            written += chunk.len() as u64;
            if written > a.size {
                drop(file);
                let _ = tokio::fs::remove_file(part).await;
                return Err(PeerError::BadResponse(format!("more than the catalog's {} bytes", a.size)));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if written != a.size {
            return Err(PeerError::BadResponse(format!(
                "{written} of {} bytes (the part is kept; the next attempt resumes)",
                a.size
            )));
        }
        info!(
            peer = %peer.name,
            youtube_id = %a.youtube_id,
            kind = a.kind.as_str(),
            resumed_from = if resumed { have } else { 0 },
            bytes = a.size,
            "exchange: fetched an artifact"
        );
        Ok(())
    }
}

/// Every other part of the same video and kind (an older copy's) is dropped.
async fn drop_older_parts(dir: &Path, a: &Artifact, keep: &str) {
    let prefix = format!("{}_{}_", a.youtube_id, a.kind.as_str());
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".part") && name != keep {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

impl Exchange {
    /// Where fetched artifacts wait as parts.
    pub(crate) fn parts_dir(&self) -> PathBuf {
        self.cache_dir.join("peer")
    }

    /// Artifact `a` of `peer`, a verified part; refused while transfers are
    /// paused (a transfer already running finishes).
    pub async fn fetch(&self, peer: &PeerConfig, a: &Artifact) -> Result<PathBuf, PeerError> {
        if self.transfers_paused().await {
            return Err(PeerError::Paused);
        }
        self.client.fetch(peer, a, &self.parts_dir()).await
    }
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::fetch` — Expected: PASS (14 tests).
- [ ] **Step 5: Commit** — `test(#229): artifact fetch — resume, bounds, sha, one per peer, pause` then `feat(#229): peer::fetch — Range-resumed, sha-checked parts`.

### Task 5.3: `POST /api/v1/exchange/probe` + last reads in the status; push; Cloudflare token

**Files:**
- Modify: `crates/sp-server/src/peer/lan.rs`, `crates/sp-server/src/peer/lan_tests.rs`, `.claude/rules/peer-exchange.md`, `scripts/cloudflare/README.md`

**Interfaces:**
- Consumes: `PeerClient::{read_catalog, last_reads}`.
- Produces: `lan::ProbeResult { name, base_url, ok: bool, artifacts: usize, jobs: usize, latency_ms: u64, error: Option<String> }`; `POST /api/v1/exchange/probe` → 200 `[ProbeResult]` (409 + reason when the settings do not hold); `PeerStatus.last_read: Option<LastRead>`.

- [ ] **Step 1: Write the failing tests** (append to `lan_tests.rs`)

```rust
async fn post_probe(ex: &Arc<Exchange>) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/exchange/probe")
        .body(Body::empty())
        .unwrap();
    let resp = crate::peer::router(ex.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn the_probe_reads_each_peer_now_and_the_status_keeps_it() {
    use crate::peer::rig::{SNV_KEY, TestNode};
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video("aaaaaaaaaaa").await;
    snv.give_song(id, "aaaaaaaaaaa", "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    let wrong = TestNode::start("wrong", Some(SNV_KEY)).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY), wrong.as_peer("another-peer-key-0123456789abcdef")]).await;
    let (status, body) = post_probe(&pp.ex).await;
    assert_eq!(status, StatusCode::OK);
    let results: Vec<ProbeResult> = serde_json::from_str(&body).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].name, "snv");
    assert!(results[0].ok && results[0].artifacts == 3 && results[0].error.is_none());
    assert!(!results[1].ok);
    assert!(results[1].error.as_deref().unwrap().contains("peer key"));
    let (_, body) = get_status(&pp.ex).await;
    let s: ExchangeStatus = serde_json::from_str(&body).unwrap();
    assert!(s.peers[0].last_read.as_ref().unwrap().ok);
    assert!(!s.peers[1].last_read.as_ref().unwrap().ok);
}

#[tokio::test]
async fn the_probe_refuses_settings_that_do_not_hold() {
    let (ex, _dir) = exchange().await;
    crate::db::models::set_setting(&ex.pool, SETTING_NODE_NAME, "PP").await.unwrap();
    let (status, _) = post_probe(&ex).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
```

(`artifacts == 3`: video + audio + metadata.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::lan` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** in `lan.rs`: imports `use axum::http::StatusCode;`, `use axum::response::{IntoResponse, Response};`, `use axum::routing::post;`, `use super::client::LastRead;`, `use tracing::info;`; add `pub last_read: Option<LastRead>,` to `PeerStatus` (fill it from `let reads = ex.client.last_reads();` → `last_read: reads.get(&p.name).cloned()`); the route `.route("/api/v1/exchange/probe", post(probe))`; and:

```rust
/// One peer's live catalog read (`POST /api/v1/exchange/probe`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub name: String,
    pub base_url: String,
    pub ok: bool,
    pub artifacts: usize,
    pub jobs: usize,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Read every peer's catalog now (the live gate of PP → SNV through Cloudflare).
pub async fn probe(State(ex): State<Arc<Exchange>>) -> Response {
    let cfg = match NodeConfig::load(&ex.pool).await {
        Ok(cfg) => cfg,
        Err(e) => return (StatusCode::CONFLICT, e).into_response(),
    };
    let mut results = Vec::new();
    for peer in &cfg.peers {
        let read = ex.client.read_catalog(peer).await;
        let latency_ms = ex.client.last_reads().get(&peer.name).map_or(0, |r| r.latency_ms);
        results.push(ProbeResult {
            name: peer.name.clone(),
            base_url: peer.base_url.clone(),
            ok: read.is_ok(),
            artifacts: read.as_ref().map_or(0, |c| c.artifacts.len()),
            jobs: read.as_ref().map_or(0, |c| c.jobs.len()),
            latency_ms,
            error: read.err().map(|e| e.to_string()),
        });
    }
    info!(peers = results.len(), ok = results.iter().filter(|r| r.ok).count(), "exchange: probe");
    Json(results).into_response()
}
```

(Update `status_shows_the_node_and_its_peers_without_secrets`'s expected `PeerStatus` with `last_read: None` in the same commit — a fixture follows a new field, no assertion is weakened.)

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::` — Expected: PASS.

- [ ] **Step 5: Append to `.claude/rules/peer-exchange.md`**

```markdown
## The peer client (`peer::client`, `peer::fetch`) and the probe

- Redirects are NEVER followed: Cloudflare Access refuses a bad/missing
  service token with a 302 to its login page (`auto_redirect_to_identity`).
  Status → `PeerError`: 3xx/403 `AccessRefused`, 401 `KeyRefused`, 404
  `NotFound` (for a catalog: the API is off there), 503 `Paused`.
- A good catalog is cached 60 s (`fresh`); a failed read is not cached; each
  read's outcome is in `/api/v1/exchange/status` (`peers[].last_read`).
- An artifact → `<cache>/peer/<yt>_<kind>_<sha16>.part`: resumed with
  `Range` (a 206 must start where asked, a 200 restarts), never past the
  catalog's size, sha-checked (a mismatch drops the part AND the cached
  catalog). A short body keeps the part for the next attempt. One transfer
  at a time per peer. `Exchange::fetch` refuses while paused.
- `POST /api/v1/exchange/probe`: every peer's catalog read now; PP's
  post-deploy gate needs `snv` ok with artifacts > 0 through
  `https://sp.newlevel.media`.
```

- [ ] **Step 6: Commit** — `test(#229): exchange probe + last reads` then `feat(#229): POST /api/v1/exchange/probe, last reads in the status` then `docs(#229): peer client rules`.
- [ ] **Step 7: Push the lane**, monitor CI.

- [ ] **Step 8: MAIN SESSION OPS — a Cloudflare Access service token for PP.** Load the `cloudflare-api-tokens` skill first; the account, app and existing policy ids are in `scripts/cloudflare/README.md`, and the account-owned API token is used only through `airuleset.py secret exec` (never read by hand). The new token's secret is shown ONCE: the call that mints it pipes `.result.client_id` / `.result.client_secret` straight into the secret channel as `songplayer-cf-token-pp`.
  1. `POST /accounts/{account}/access/service_tokens` `{"name":"songplayer-peer-pp · claude","duration":"8760h"}`.
  2. `POST /accounts/{account}/access/apps/{app}/policies` `{"name":"SongPlayer peer PP (service token) · claude","decision":"non_identity","precedence":2,"include":[{"service_token":{"token_id":"<id from 1>"}}]}`.
  3. Verify from dev1 inside `secret exec` (env `CF_ID`, `CF_SECRET`, `SNV_PEER_KEY`):
     `curl -s -o /dev/null -w '%{http_code}\n' -H "cf-access-client-id: $CF_ID" -H "cf-access-client-secret: $CF_SECRET" -H "x-sp-peer-key: $SNV_PEER_KEY" https://sp.newlevel.media/api/v1/peer/catalog` → `200`; the same without the Access headers → `302`.
  4. Replace the README's "Future: service token" section with "Service token for PP peer reads (#229)": the token id, its expiry date, the policy id, the two verify calls (no secret). Commit `docs(#229): Cloudflare service token for PP's peer reads` (it rides Lane 6's push).

---

## Lane 6 — PP deploy (own runner, main releases only, PP post-deploy subset)

Design comment for #229: *Approach:* a separate workflow `deploy-pp.yml` triggered by `workflow_run` of CI on `main` (conclusion success, event push) plus an explicit `workflow_dispatch` with a CI run id (a dev build at PP only on purpose). It takes that run's `tauri-installer` + `dist` artifacts (`download-artifact` with `run-id`), runs on the PP runner (label `resolume-pp` only), stops SongPlayer, installs `/S`, copies `dist`, starts the phase-0 scheduled task, checks the version, then runs `post-deploy-pp.config.ts` (version in the DOM, PP's own settings kept, a playlist on program with an SP-program receiver, SP-program-MAX, a manual scene through the facade = "OBS manuál", the live probe of SNV through Cloudflare). It never touches PP's DB. *Rejected:* a `deploy-pp` job inside `ci.yml` — a queued job on an offline self-hosted runner holds the branch's concurrency group and blocks every later main push (`ci-workflows.md`); and re-registering the scheduled task/firewall on every PP deploy (phase 0 owns them, for PP's logged-on user `newlevel`; touching them each release adds risk for nothing). *Architektúra:* GitHub Actions `workflow_run` + self-hosted Windows runner (existing `scripts/setup-runner.ps1`), Playwright (existing e2e helpers).

### Task 6.1: the PP runner label + workflow guard tests

**Files:**
- Modify: `scripts/setup-runner.ps1` (labels from `RUNNER_LABELS`), `.github/actionlint.yaml` (declare `resolume-pp`)
- Create: `scripts/tests/test_deploy_pp_workflow.py`

**Interfaces:**
- Produces: `setup-runner.ps1` honours `$env:RUNNER_LABELS` (default unchanged `self-hosted,windows,resolume`); pytest guards that pin the deploy's trigger, runner label and concurrency group (Task 6.2 makes them pass).

- [ ] **Step 1: Write the failing tests** `scripts/tests/test_deploy_pp_workflow.py`

```python
"""#229: PP gets main releases only, on its own runner label (deploy-pp.yml).

A deploy-pp job that ran on a dev push, or on a runner SNV's jobs can land
on, would put a dev build on the PP site's live wall.
"""

from pathlib import Path

import check_workflow_ps_ascii as ps_ascii

_REPO = Path(__file__).resolve().parents[2]
_WORKFLOWS = _REPO / ".github" / "workflows"


def _deploy_pp() -> str:
    return (_WORKFLOWS / "deploy-pp.yml").read_text(encoding="utf-8")


def test_pp_deploys_a_green_ci_run_on_main_or_an_explicit_dispatch_only():
    text = _deploy_pp()
    assert "workflow_run:" in text
    assert "workflows: [CI]" in text
    assert "branches: [main]" in text
    assert "workflow_dispatch:" in text
    assert "github.event.workflow_run.conclusion == 'success'" in text
    assert "github.event.workflow_run.head_branch == 'main'" in text
    assert "push:" not in text, "never on a push of its own"


def test_every_pp_job_runs_on_the_pp_runner_and_no_other_workflow_does():
    assert _deploy_pp().count("runs-on: [self-hosted, windows, resolume-pp]") == 2
    for workflow in _WORKFLOWS.glob("*.yml"):
        if workflow.name != "deploy-pp.yml":
            assert "resolume-pp" not in workflow.read_text(encoding="utf-8"), workflow.name


def test_pp_deploys_in_its_own_group_and_never_cancels_one_in_flight():
    text = _deploy_pp()
    assert "group: deploy-pp" in text
    assert "cancel-in-progress: false" in text


def test_pp_powershell_is_ascii():
    assert ps_ascii.violations(_deploy_pp()) == []


def test_the_pp_label_is_declared_and_the_runner_setup_takes_it():
    actionlint = (_REPO / ".github" / "actionlint.yaml").read_text(encoding="utf-8")
    assert "- resolume-pp" in actionlint
    setup = (_REPO / "scripts" / "setup-runner.ps1").read_text(encoding="utf-8")
    assert "$env:RUNNER_LABELS" in setup
```

- [ ] **Step 2: Run (local, Python is allowed):** `ruff format scripts/tests/test_deploy_pp_workflow.py && ruff check scripts/tests/ && pytest scripts/tests/test_deploy_pp_workflow.py -v` — Expected: 5 FAIL (no `deploy-pp.yml`, no label, no env var).

- [ ] **Step 3: Implement.** In `scripts/setup-runner.ps1` replace `$Labels = "self-hosted,windows,resolume"` with:

```powershell
# #229: the PP site's runner takes its own label (RUNNER_LABELS=self-hosted,windows,resolume-pp)
# so SNV's jobs ([self-hosted, windows, resolume]) never land on it.
$Labels = if ($env:RUNNER_LABELS) { $env:RUNNER_LABELS } else { "self-hosted,windows,resolume" }
```

In `.github/actionlint.yaml` add `    - resolume-pp` under `labels:` (and extend its comment: "and `resolume-pp`, the PP site's runner used by deploy-pp.yml").

- [ ] **Step 4: Run:** `pytest scripts/tests/test_deploy_pp_workflow.py -v` — Expected: `test_the_pp_label_is_declared_and_the_runner_setup_takes_it` PASS, the other 4 still FAIL until Task 6.2.
- [ ] **Step 5: Commit** — `test(#229): deploy-pp guards — main only, own runner, own group` then `ci(#229): the runner setup takes RUNNER_LABELS; actionlint knows resolume-pp`.

### Task 6.2: `.github/workflows/deploy-pp.yml`

**Files:**
- Create: `.github/workflows/deploy-pp.yml`

**Interfaces:**
- Consumes: CI's artifacts `tauri-installer` and `dist` of the chosen run; the PP scheduled task `SongPlayer` created by phase 0; repo variable `PP_MANUAL_SCENE` (optional).
- Produces: jobs `resolve` (ubuntu), `deploy-pp` and `e2e-pp` (`[self-hosted, windows, resolume-pp]`).

- [ ] **Step 1: Write the workflow** (every PowerShell code line ASCII — `check_workflow_ps_ascii.py`)

```yaml
name: Deploy to PP

# #229: the PP church site (resolume-pp) runs main releases only; SNV
# (win-resolume) stays the dev rig. A CI run on main that ended green deploys
# here. A dev build reaches PP only on purpose: dispatch this workflow with the
# id of the CI run whose build PP gets. PP's settings live in PP's DB and no
# step here touches it.
on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [main]
  workflow_dispatch:
    inputs:
      ci_run_id:
        description: "CI run id whose tauri-installer + dist artifacts PP gets"
        required: true
        type: string

# Its own group: a queued job on an offline PP runner holds THIS group, never
# CI's branch group (ci-workflows.md). A newer release waits for the one in
# flight; a deploy is never cancelled half-way (it stops SongPlayer first).
concurrency:
  group: deploy-pp
  cancel-in-progress: false

permissions:
  contents: read
  actions: read

jobs:
  resolve:
    name: Pick the build for PP
    runs-on: ubuntu-latest
    if: >-
      github.event_name == 'workflow_dispatch'
      || (github.event.workflow_run.conclusion == 'success'
          && github.event.workflow_run.event == 'push'
          && github.event.workflow_run.head_branch == 'main')
    outputs:
      run_id: ${{ steps.pick.outputs.run_id }}
      head_sha: ${{ steps.pick.outputs.head_sha }}
    steps:
      - name: Resolve the CI run and its commit
        id: pick
        env:
          GH_TOKEN: ${{ github.token }}
          EVENT_NAME: ${{ github.event_name }}
          RUN_FROM_EVENT: ${{ github.event.workflow_run.id }}
          SHA_FROM_EVENT: ${{ github.event.workflow_run.head_sha }}
          RUN_FROM_INPUT: ${{ inputs.ci_run_id }}
        run: |
          set -euo pipefail
          if [ "$EVENT_NAME" = "workflow_dispatch" ]; then
            RUN_ID="$RUN_FROM_INPUT"
            case "$RUN_ID" in
              ''|*[!0-9]*) echo "FAIL: ci_run_id must be a number"; exit 1 ;;
            esac
            NAME=$(gh api "repos/$GITHUB_REPOSITORY/actions/runs/$RUN_ID" --jq .name)
            if [ "$NAME" != "CI" ]; then
              echo "FAIL: run $RUN_ID is '$NAME', not a CI run"
              exit 1
            fi
            HEAD_SHA=$(gh api "repos/$GITHUB_REPOSITORY/actions/runs/$RUN_ID" --jq .head_sha)
          else
            RUN_ID="$RUN_FROM_EVENT"
            HEAD_SHA="$SHA_FROM_EVENT"
          fi
          echo "PP gets CI run $RUN_ID (commit $HEAD_SHA)"
          echo "run_id=$RUN_ID" >> "$GITHUB_OUTPUT"
          echo "head_sha=$HEAD_SHA" >> "$GITHUB_OUTPUT"

  deploy-pp:
    name: Deploy to resolume-pp
    needs: [resolve]
    runs-on: [self-hosted, windows, resolume-pp]
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ needs.resolve.outputs.head_sha }}

      - name: Download Tauri installer
        uses: actions/download-artifact@v4
        with:
          name: tauri-installer
          path: artifacts/tauri
          run-id: ${{ needs.resolve.outputs.run_id }}
          github-token: ${{ github.token }}

      - name: Download WASM frontend
        uses: actions/download-artifact@v4
        with:
          name: dist
          path: artifacts/dist
          run-id: ${{ needs.resolve.outputs.run_id }}
          github-token: ${{ github.token }}

      - name: Deploy SongPlayer (PP's DB and settings stay as they are)
        shell: powershell
        run: |
          $ErrorActionPreference = "Continue"
          Write-Host "=== Stopping SongPlayer ==="
          Stop-ScheduledTask -TaskName "SongPlayer" -ErrorAction SilentlyContinue
          taskkill /F /IM "SongPlayer.exe" 2>&1 | Out-Null
          $elapsed = 0
          while ($elapsed -lt 15) {
            $proc = Get-Process -Name "SongPlayer" -ErrorAction SilentlyContinue
            $state = (Get-ScheduledTask -TaskName "SongPlayer" -ErrorAction SilentlyContinue).State
            if (-not $proc -and $state -ne "Running") { break }
            Start-Sleep -Seconds 1
            $elapsed++
          }
          taskkill /F /IM "CLIProxyAPI.exe" 2>&1 | Out-Null
          $elapsed = 0
          while ($elapsed -lt 30) {
            $portInUse = netstat -an | Select-String ":8920 " | Select-String "LISTENING"
            if (-not $portInUse) { break }
            Start-Sleep -Seconds 2
            $elapsed += 2
          }
          Write-Host "=== Installing SongPlayer ==="
          $installer = Get-ChildItem "artifacts/tauri/*.exe" | Select-Object -First 1
          if (-not $installer) {
            Write-Error "FAIL: no installer in the tauri-installer artifact"
            exit 1
          }
          Start-Process -FilePath $installer.FullName -ArgumentList "/S" -Wait
          $distTarget = "C:\Program Files\SongPlayer\dist"
          Remove-Item -Path $distTarget -Recurse -Force -ErrorAction SilentlyContinue
          Copy-Item -Path "artifacts\dist" -Destination $distTarget -Recurse -Force
          Write-Host "Dashboard deployed to $distTarget"

      - name: Start SongPlayer (the phase 0 scheduled task)
        shell: powershell
        run: |
          $ErrorActionPreference = "Stop"
          $task = Get-ScheduledTask -TaskName "SongPlayer" -ErrorAction SilentlyContinue
          if (-not $task) {
            Write-Error "FAIL: no SongPlayer scheduled task at PP - the phase 0 install creates it (issue 229)"
            exit 1
          }
          Write-Host "SongPlayer task runs as $($task.Principal.UserId)"
          schtasks.exe /run /tn "SongPlayer"
          if ($LASTEXITCODE -ne 0) { throw "schtasks.exe failed (exit code: $LASTEXITCODE)" }
          Start-Sleep -Seconds 10

      - name: Health checks
        shell: powershell
        run: |
          $ErrorActionPreference = "Stop"
          $VERSION = (Get-Content "VERSION" -Raw).Trim()
          $proc = Get-Process -Name "SongPlayer" -ErrorAction SilentlyContinue
          if (-not $proc) {
            Write-Error "FAIL: SongPlayer.exe is not running"
            exit 1
          }
          $apiOk = $false
          for ($i = 1; $i -le 6; $i++) {
            try {
              $resp = Invoke-RestMethod -Uri "http://localhost:8920/api/v1/status" -TimeoutSec 5
              $apiOk = $true
              break
            } catch {
              Write-Host "Attempt ${i}/6 - waiting 5s..."
              Start-Sleep -Seconds 5
            }
          }
          if (-not $apiOk) {
            Write-Error "FAIL: the API did not answer"
            exit 1
          }
          if ($resp.version -ne $VERSION) {
            Write-Error "FAIL: API version '$($resp.version)' != expected '$VERSION'"
            exit 1
          }
          Write-Host "OK: PP runs $VERSION"

  e2e-pp:
    name: E2E Tests (resolume-pp)
    needs: [resolve, deploy-pp]
    runs-on: [self-hosted, windows, resolume-pp]
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ needs.resolve.outputs.head_sha }}

      - name: Post-deploy Playwright (PP subset)
        shell: powershell
        env:
          SONGPLAYER_URL: http://localhost:8920
          FACADE_WS_URL: ws://localhost:4456
          PP_MANUAL_SCENE: ${{ vars.PP_MANUAL_SCENE }}
        run: |
          $env:SP_EXPECTED_VERSION = (Get-Content "VERSION" -Raw).Trim()
          Set-Location $env:GITHUB_WORKSPACE\e2e
          npm ci 2>&1 | Select-Object -Last 5
          if ($LASTEXITCODE -ne 0) {
            Write-Error "FAIL: npm ci failed"
            exit 1
          }
          npx playwright install chromium 2>&1 | Select-Object -Last 3
          npx playwright test --config=post-deploy-pp.config.ts --reporter=list 2>&1
          if ($LASTEXITCODE -ne 0) {
            Write-Error "FAIL: the PP post-deploy suite failed"
            exit 1
          }
          Write-Host "OK: the PP post-deploy suite passed"

      - name: Upload the PP post-deploy report on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: pp-post-deploy-report
          path: |
            e2e/post-deploy-report/
            e2e/test-results/
          retention-days: 7
          if-no-files-found: ignore
```

- [ ] **Step 2: Run (local):** `pytest scripts/tests/test_deploy_pp_workflow.py -v` (5 PASS), `python3 scripts/check_workflow_ps_ascii.py .github/workflows/deploy-pp.yml` (exit 0), `~/.local/bin/actionlint .github/workflows/deploy-pp.yml` (no finding).
- [ ] **Step 3: Commit** — `ci(#229): deploy-pp.yml — PP gets green main releases, its own runner and group`.

### Task 6.3: the PP post-deploy subset (Playwright)

**Files:**
- Create: `e2e/peer-probe-gate.ts`, `e2e/peer-probe-gate.spec.ts`, `e2e/post-deploy-pp.spec.ts`, `e2e/post-deploy-pp.config.ts`
- Modify: `e2e/post-deploy.config.ts` (SNV must not run the PP spec)

**Interfaces:**
- Consumes: `GET /api/v1/status` (`version`, `active_scene`), `GET /api/v1/exchange/status`, `GET /api/v1/program` (`source`, `health.connections`, `degraded_reason`), `POST /api/v1/exchange/probe`, `e2e/obs-driver.ts` (`ObsDriver.connect/listScenes/switchScene/currentProgramScene/disconnect`), `e2e/obs-baseline-scene.ts::pickBaselineScene`, `e2e/av-sync-probe.ts::AV_PROBE_SCENE`, `e2e/ndi-health-gate.ts::programReceiverVerdict`, `e2e/post-deploy-max.spec.ts` (run as is).
- Produces: `peer-probe-gate.ts::{ProbeResult, probeFailures(results, peer, viaHost): string[]}` (unit-tested in the mock suite).

- [ ] **Step 1: Write the failing unit spec** `e2e/peer-probe-gate.spec.ts` (mock suite: `playwright.config.ts` runs every `*.spec.ts` except `post-deploy*`)

```ts
import { test, expect } from "@playwright/test";
import { probeFailures, type ProbeResult } from "./peer-probe-gate";

const ok: ProbeResult = {
  name: "snv",
  base_url: "https://sp.newlevel.media",
  ok: true,
  artifacts: 4211,
  jobs: 0,
  latency_ms: 180,
  error: null,
};

test.describe("peer probe gate (#229)", () => {
  test("a live read through the public host passes", () => {
    expect(probeFailures([ok], "snv", "sp.newlevel.media")).toEqual([]);
  });

  test("a missing peer fails", () => {
    expect(probeFailures([], "snv", "sp.newlevel.media")).toEqual(["no peer named snv is configured"]);
  });

  test("a read over the LAN is not the Cloudflare path", () => {
    const lan = { ...ok, base_url: "http://10.77.9.201:8920" };
    const failures = probeFailures([lan], "snv", "sp.newlevel.media");
    expect(failures).toHaveLength(1);
    expect(failures[0]).toContain("not read through https://sp.newlevel.media");
  });

  test("a refused read fails with its error", () => {
    const refused = { ...ok, ok: false, artifacts: 0, error: "refused by Cloudflare Access (HTTP 302)" };
    expect(probeFailures([refused], "snv", "sp.newlevel.media")).toEqual([
      "reading snv's catalog failed: refused by Cloudflare Access (HTTP 302)",
    ]);
  });

  test("an empty catalog fails", () => {
    expect(probeFailures([{ ...ok, artifacts: 0 }], "snv", "sp.newlevel.media")).toEqual([
      "snv's catalog lists no artifact",
    ]);
  });
});
```

- [ ] **Step 2: Run (local, Tier-0 allows node):** in `e2e/`, `npx playwright test peer-probe-gate.spec.ts` — Expected: FAIL (module not found).

- [ ] **Step 3: Implement** `e2e/peer-probe-gate.ts`

```ts
/**
 * #229: the PP post-deploy gate's decision on PP's live read of a peer's
 * catalog (`POST /api/v1/exchange/probe`), pure so the mock suite tests it
 * (`peer-probe-gate.spec.ts`); the box read is `post-deploy-pp.spec.ts`.
 */

/** One entry of `POST /api/v1/exchange/probe` (sp-server `lan::ProbeResult`). */
export interface ProbeResult {
  name: string;
  base_url: string;
  ok: boolean;
  artifacts: number;
  jobs: number;
  latency_ms: number;
  error: string | null;
}

/** Why the read of `peer` fails the gate; empty when it passes: the peer is
 *  configured, read through `https://<viaHost>` (Cloudflare), the read
 *  worked and the catalog lists something. */
export function probeFailures(results: ProbeResult[], peer: string, viaHost: string): string[] {
  const r = results.find((x) => x.name === peer);
  if (!r) return [`no peer named ${peer} is configured`];
  const failures: string[] = [];
  if (!r.base_url.startsWith(`https://${viaHost}`)) {
    failures.push(`peer ${peer} is not read through https://${viaHost} (base_url ${r.base_url})`);
  }
  if (!r.ok) failures.push(`reading ${peer}'s catalog failed: ${r.error ?? "no error text"}`);
  else if (r.artifacts <= 0) failures.push(`${peer}'s catalog lists no artifact`);
  return failures;
}
```

- [ ] **Step 4: Write the PP spec** `e2e/post-deploy-pp.spec.ts`

```ts
/**
 * #229: the post-deploy subset at the PP site (resolume-pp), run by
 * .github/workflows/deploy-pp.yml after every main release:
 *  - the dashboard shows the deployed version (no console error);
 *  - the deploy kept PP's own settings (node `pp`, peer `snv`);
 *  - a playlist plays on SongPlayer's program and SP-program has a receiver;
 *  - a manual scene pressed through the facade reaches the program as
 *    "OBS manuál" (source -1);
 *  - PP reads SNV's catalog live through Cloudflare (`POST /api/v1/exchange/probe`).
 * SP-program-MAX is gated by post-deploy-max.spec.ts in the same run
 * (post-deploy-pp.config.ts). The A/V gate comes later, once PP records.
 */
import { test, expect, APIRequestContext } from "@playwright/test";
import { ObsDriver } from "./obs-driver";
import { pickBaselineScene } from "./obs-baseline-scene";
import { AV_PROBE_SCENE } from "./av-sync-probe";
import { programReceiverVerdict, type ProgramReceiverView } from "./ndi-health-gate";
import { probeFailures, type ProbeResult } from "./peer-probe-gate";

const FACADE_WS_URL = process.env.FACADE_WS_URL || "ws://localhost:4456";
const EXPECTED_VERSION = (process.env.SP_EXPECTED_VERSION || "").trim();
const MANUAL_SCENE = (process.env.PP_MANUAL_SCENE || "").trim();
const PEER = "snv";
const PEER_HOST = "sp.newlevel.media";
/** `GET /api/v1/program` → `source` for "OBS manuál" (sp-core PROGRAM_INPUT_ID). */
const OBS_MANUAL_SOURCE = -1;

/** A bounded read that never throws (`expect.poll` does not retry a throw). */
async function getJson<T>(request: APIRequestContext, url: string): Promise<T | null> {
  try {
    const resp = await request.get(url, { timeout: 10_000 });
    return resp.ok() ? ((await resp.json()) as T) : null;
  } catch {
    return null;
  }
}

test.describe.serial("PP post-deploy (#229)", () => {
  let driver: ObsDriver;
  let startScene = "";

  test.beforeAll(async () => {
    driver = await ObsDriver.connect(FACADE_WS_URL);
    startScene = await driver.currentProgramScene();
    console.log(`[#229 pp] program scene at the start: ${startScene}`);
  });

  test.afterAll(async () => {
    if (startScene) await driver.switchScene(startScene);
    await driver.disconnect();
  });

  test("the dashboard shows the deployed version", async ({ page, request }) => {
    const consoleMessages: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        consoleMessages.push(`[${msg.type()}] ${msg.text()}`);
      }
    });
    expect(EXPECTED_VERSION, "deploy-pp.yml sets SP_EXPECTED_VERSION").not.toBe("");
    const status = await getJson<{ version: string }>(request, "/api/v1/status");
    expect(status?.version).toBe(EXPECTED_VERSION);
    await page.goto("/");
    await expect(page.locator('[data-testid="version"]')).toHaveText(`v${EXPECTED_VERSION}`, {
      timeout: 30_000,
    });
    expect(consoleMessages, "a clean browser console").toEqual([]);
  });

  test("the deploy kept PP's own settings", async ({ request }) => {
    const s = await getJson<{
      node_name: string | null;
      config_error: string | null;
      peers: { name: string }[];
    }>(request, "/api/v1/exchange/status");
    expect(s, "GET /api/v1/exchange/status").not.toBeNull();
    expect(s!.config_error).toBeNull();
    expect(s!.node_name).toBe("pp");
    expect(s!.peers.map((p) => p.name)).toContain(PEER);
  });

  test("a playlist plays on program and SP-program has a receiver", async ({ request }) => {
    test.setTimeout(120_000);
    const scene = pickBaselineScene(await driver.listScenes());
    expect(scene.startsWith("sp-"), `a playlist scene to play (got ${scene})`).toBe(true);
    await driver.switchScene(scene);
    await expect
      .poll(async () => (await getJson<{ active_scene: string | null }>(request, "/api/v1/status"))?.active_scene ?? null, {
        message: `SongPlayer's program reaches ${scene}`,
        timeout: 15_000,
      })
      .toBe(scene);
    await expect
      .poll(
        async () => {
          const p = await getJson<ProgramReceiverView>(request, "/api/v1/program");
          return p ? programReceiverVerdict(p).ok : false;
        },
        { message: "SP-program has a live receiver", timeout: 60_000 },
      )
      .toBe(true);
  });

  test("a manual scene through the facade reaches the program as OBS manual", async ({ request }) => {
    const scenes = await driver.listScenes();
    const manual = MANUAL_SCENE || scenes.find((s) => !s.startsWith("sp-") && s !== AV_PROBE_SCENE);
    expect(manual, `a manual scene among ${JSON.stringify(scenes)}`).toBeTruthy();
    await driver.switchScene(manual!);
    await expect
      .poll(async () => (await getJson<{ source: number | null }>(request, "/api/v1/program"))?.source ?? null, {
        message: `${manual} reaches the program as OBS manual`,
        timeout: 15_000,
      })
      .toBe(OBS_MANUAL_SOURCE);
  });

  test("PP reads SNV's catalog live through Cloudflare", async ({ request }) => {
    const resp = await request.post("/api/v1/exchange/probe", { timeout: 60_000 });
    expect(resp.status(), "POST /api/v1/exchange/probe").toBe(200);
    const results = (await resp.json()) as ProbeResult[];
    console.log(`[#229 pp] probe: ${JSON.stringify(results)}`);
    expect(probeFailures(results, PEER, PEER_HOST)).toEqual([]);
  });
});
```

`e2e/post-deploy-pp.config.ts`:

```ts
import { defineConfig } from "@playwright/test";

/**
 * #229: the post-deploy subset at the PP site, run by deploy-pp.yml.
 * The SNV suite is post-deploy.config.ts (it ignores post-deploy-pp*).
 */
export default defineConfig({
  testDir: ".",
  testMatch: ["**/post-deploy-pp.spec.ts", "**/post-deploy-max.spec.ts"],
  timeout: 90_000,
  retries: 0,
  workers: 1,
  reporter: [
    ["list"],
    ["html", { outputFolder: "post-deploy-report", open: "never" }],
  ],
  use: {
    baseURL: process.env.SONGPLAYER_URL || "http://localhost:8920",
    headless: true,
    ignoreHTTPSErrors: true,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
```

In `e2e/post-deploy.config.ts`, the `chromium` project's `testIgnore` becomes `["**/post-deploy-preview.spec.ts", "**/post-deploy-owner-path.spec.ts", "**/post-deploy-pp*.spec.ts"]` (comment: "#229: the PP subset runs only at PP, deploy-pp.yml").

- [ ] **Step 5: Run (local):** in `e2e/`: `npx playwright test peer-probe-gate.spec.ts` (5 PASS); `npx playwright test --config=post-deploy-pp.config.ts --list` (lists the 5 PP tests + the MAX test); `npx playwright test --config=post-deploy.config.ts --list | grep -c post-deploy-pp` → `0`. Strict type check per `post-deploy-program-state.md` (scratch `typescript` + `@types/node`, `tsc --noEmit --strict … post-deploy-pp.spec.ts peer-probe-gate.ts post-deploy-pp.config.ts`).
- [ ] **Step 6: Commit** — `test(#229): peer probe gate unit spec` then `test(#229): PP post-deploy subset + its config; SNV ignores it`.

### Task 6.4: docs, push, and the PP side (ops)

**Files:**
- Modify: `.claude/rules/peer-exchange.md`

- [ ] **Step 1: Append to `.claude/rules/peer-exchange.md`**

```markdown
## PP deploy (`deploy-pp.yml`)

- PP gets main releases only: `workflow_run` of CI on `main` (green, push)
  or an explicit `workflow_dispatch -f ci_run_id=<CI run>` (a dev build at
  PP only on purpose). Its own concurrency group `deploy-pp` (an offline PP
  runner never blocks CI), never cancelled half-way.
- Runner: label `resolume-pp` ONLY (`RUNNER_LABELS=self-hosted,windows,
  resolume-pp` for `scripts/setup-runner.ps1`); it must never carry
  `resolume` (SNV's jobs would land on PP). Pinned by
  `scripts/tests/test_deploy_pp_workflow.py`.
- The deploy stops the phase-0 `SongPlayer` task, installs `/S`, copies
  `dist`, starts the task, checks the version; it never touches PP's DB, the
  task, the ACL or the firewall (phase 0 owns them).
- Post-deploy at PP: `post-deploy-pp.config.ts` = `post-deploy-pp.spec.ts` +
  `post-deploy-max.spec.ts`. The manual scene is repo variable
  `PP_MANUAL_SCENE` (else the first non-`sp-` scene).
- "produkcia beží" at PP: `gh run cancel` the deploy-pp run in flight and
  `gh workflow disable deploy-pp.yml` until "event skončil"; touch nothing at PP.
- A DB copied SNV → PP carries SNV's `node_name` / `peer_api_key`: PATCH
  PP's `node_name=pp` and `peer_api_key=""` before that DB's first start.
```

- [ ] **Step 2: Commit** `docs(#229): PP deploy rules`; **push the lane**, monitor CI (the new pytest + the mock-suite `peer-probe-gate.spec.ts` run in CI; deploy-pp.yml does not run on a dev push).

- [ ] **Step 3: MAIN SESSION OPS — the PP side** (PP reached the way phase 0 reaches it; no event running — the owner guards that):
  1. **Node.js LTS on PP** (the e2e job runs `npm ci` / `npx`): `winget install OpenJS.NodeJS.LTS` as admin; verify `node --version`.
  2. **Register the runner** as PP's logged-on user `newlevel`, elevated: on dev1 `gh api -X POST repos/zbynekdrlik/songplayer/actions/runners/registration-token --jq .token` → into the secret channel; at PP `$env:RUNNER_TOKEN=<from the channel>; $env:RUNNER_LABELS="self-hosted,windows,resolume-pp"; irm https://raw.githubusercontent.com/zbynekdrlik/songplayer/dev/scripts/setup-runner.ps1 | iex`. Verify on dev1: `gh api repos/zbynekdrlik/songplayer/actions/runners --jq '.runners[] | {name, status, labels: [.labels[].name]}'` → `resolume-pp` online, labels `self-hosted, Windows, X64, resolume-pp` and NOT `resolume`.
  3. **PP's identity:** PATCH `http://10.76.8.201:8920/api/v1/settings` `{"node_name":"pp","peer_api_key":""}` now (no secret). Set `peers` ONLY once PP runs a build that masks it (after the first green Deploy job of `deploy-pp.yml`): inside `secret exec` with `SNV_PEER_KEY`, `CF_ID`, `CF_SECRET`, PATCH `{"peers":"[{\"name\":\"snv\",\"base_url\":\"https://sp.newlevel.media\",\"key\":\"$SNV_PEER_KEY\",\"cf_client_id\":\"$CF_ID\",\"cf_client_secret\":\"$CF_SECRET\"}]"}`; verify `GET /api/v1/exchange/status` → `node_name pp`, `config_error null`, then `POST /api/v1/exchange/probe` → `snv` ok, artifacts > 0.
  4. **`gh variable set PP_MANUAL_SCENE --body "<the manual scene read in PP's cg OBS>"`**.
  5. **The first PP deploy** comes with the next release (dev → main PR merged): `deploy-pp.yml` must be on `main` for `workflow_run` to fire. Watch it to terminal; if `e2e-pp` failed only because `peers` was not set yet, set it (step 3) and `gh run rerun <run> --job <e2e-pp job id>`.

---

## Lane 7 — Ask-first core (decision, waits, provenance, announcements)

Design comment for #229: *Approach:* one pure `decide(job, youtube_id, reads, waited)`: a peer whose catalog holds every artifact the job NEEDS at a version this node takes → **Fetch** (beats everything, even after 2 h); else waited ≥ 2 h → **Local**; else a peer announcing a job that MAKES those kinds → **Wait**; else a peer whose catalog could not be read (unreachable, Access/key refused) → **Wait** (an outage or a misconfiguration is not "nobody has it" — bounded by the same 2 h); else **Local**. `Exchange::ask` feeds it the cached catalogs and the time already waited (`peer_waits`, first wait kept), records a wait, ends it on Local, and returns `Local(JobGuard)` so the job is announced in this node's catalog for as long as it runs — a caller cannot forget to announce. Rechecks back off (a quarter of the wait so far, 2–20 min, never past the bound). Provenance `source = peer:<node>` goes to `peer_fetches`; the row's own `metadata_source` / `lyrics_source` keep their meaning (they drive the repair queue and the ★ tier). *Rejected:* writing `peer:<node>` into `lyrics_source` / `metadata_source` (would break `alignment_model_for_source`, the ★ wall marker and `REPAIR_QUEUE_WHERE`), and treating an unreadable peer as "nobody has it" (a 30 s internet blip at PP would start a local stems run for a song SNV already has). *Architektúra:* pure fn + V30 tables + the Lane 2 job board.

### Task 7.1: `peer::decide` — the pure decision

**Files:**
- Create: `crates/sp-server/src/peer/decide.rs`, `crates/sp-server/src/peer/decide_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod decide;`)

**Interfaces:**
- Consumes: `kind::{Job, acceptable}`, `wire::{Artifact, Catalog}`.
- Produces: `decide::{MAX_PEER_WAIT = 2 h, PeerRead<'a> { peer: &'a str, catalog: Option<&'a Catalog> }, Decision { Fetch { peer: String, artifacts: Vec<Artifact> }, Wait { peer: String, why: WaitWhy }, Local(LocalWhy) }, WaitWhy { PeerRunsIt, PeerUnreadable }, LocalWhy { NoPeers, NobodyHasIt, WaitedLongEnough }, decide(Job, &str, &[PeerRead], Option<Duration>) -> Decision, holds(&Catalog, Job, &str) -> Option<Vec<Artifact>>, recheck_after(Duration) -> Duration}`.

- [ ] **Step 1: Write the failing tests** `decide_tests.rs`

```rust
//! #229 `peer::decide`.

use super::*;
use crate::peer::kind::{ArtifactKind, Job, MEDIA_VERSION, STEMS_VERSION};
use crate::peer::wire::{Artifact, Catalog, CatalogJob, JobState};
use std::time::Duration;
use ArtifactKind::{Audio, StemInstrumental, StemVocals, Video};

const YT: &str = "aaaaaaaaaaa";
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const MIN: Duration = Duration::from_secs(60);

fn art(kind: ArtifactKind, version: u32) -> Artifact {
    Artifact { youtube_id: YT.into(), kind, version, size: 10, sha256: SHA.into(), updated_at: None }
}

fn catalog(artifacts: Vec<Artifact>, running: &[ArtifactKind]) -> Catalog {
    Catalog {
        node: "x".into(),
        artifacts,
        jobs: running
            .iter()
            .map(|k| CatalogJob { youtube_id: YT.into(), kind: *k, node: "x".into(), state: JobState::Running, started_at: Some("t".into()) })
            .collect(),
    }
}

fn read<'a>(peer: &'a str, c: Option<&'a Catalog>) -> PeerRead<'a> {
    PeerRead { peer, catalog: c }
}

#[test]
fn no_peers_processes_here() {
    assert_eq!(decide(Job::Download, YT, &[], None), Decision::Local(LocalWhy::NoPeers));
}

#[test]
fn a_peer_with_every_needed_artifact_is_fetched_from() {
    let snv = catalog(vec![art(Audio, MEDIA_VERSION), art(Video, MEDIA_VERSION)], &[]);
    assert_eq!(
        decide(Job::Download, YT, &[read("snv", Some(&snv))], None),
        Decision::Fetch { peer: "snv".into(), artifacts: vec![art(Video, MEDIA_VERSION), art(Audio, MEDIA_VERSION)] }
    );
}

#[test]
fn a_peer_missing_one_needed_artifact_is_not_fetched_from() {
    let snv = catalog(vec![art(StemVocals, STEMS_VERSION)], &[]);
    assert_eq!(decide(Job::Stems, YT, &[read("snv", Some(&snv))], None), Decision::Local(LocalWhy::NobodyHasIt));
    assert_eq!(holds(&snv, Job::Stems, YT), None);
    let both = catalog(vec![art(StemVocals, STEMS_VERSION), art(StemInstrumental, STEMS_VERSION)], &[]);
    assert_eq!(holds(&both, Job::Stems, YT).map(|a| a.len()), Some(2));
    assert_eq!(holds(&both, Job::Stems, "bbbbbbbbbbb"), None, "another video");
}

#[test]
fn a_format_this_node_does_not_take_is_not_fetched() {
    let newer = catalog(vec![art(Audio, MEDIA_VERSION + 1), art(Video, MEDIA_VERSION)], &[]);
    assert_eq!(decide(Job::Download, YT, &[read("snv", Some(&newer))], None), Decision::Local(LocalWhy::NobodyHasIt));
}

#[test]
fn a_peer_that_has_it_wins_over_one_running_it() {
    let runner = catalog(vec![], &[Video]);
    let haver = catalog(vec![art(Audio, MEDIA_VERSION), art(Video, MEDIA_VERSION)], &[]);
    let d = decide(Job::Download, YT, &[read("a", Some(&runner)), read("b", Some(&haver))], None);
    assert!(matches!(d, Decision::Fetch { ref peer, .. } if peer == "b"), "{d:?}");
}

#[test]
fn a_peer_running_the_job_is_waited_for() {
    let snv = catalog(vec![], &[StemInstrumental]);
    assert_eq!(
        decide(Job::Stems, YT, &[read("snv", Some(&snv))], Some(MIN)),
        Decision::Wait { peer: "snv".into(), why: WaitWhy::PeerRunsIt }
    );
}

#[test]
fn a_job_making_other_kinds_is_not_waited_for() {
    let snv = catalog(vec![], &[ArtifactKind::Lyrics]);
    assert_eq!(decide(Job::Stems, YT, &[read("snv", Some(&snv))], None), Decision::Local(LocalWhy::NobodyHasIt));
}

#[test]
fn an_unreadable_peer_is_waited_for_after_the_readable_ones() {
    let busy = catalog(vec![], &[Video]);
    assert_eq!(
        decide(Job::Download, YT, &[read("down", None)], None),
        Decision::Wait { peer: "down".into(), why: WaitWhy::PeerUnreadable }
    );
    assert_eq!(
        decide(Job::Download, YT, &[read("down", None), read("busy", Some(&busy))], None),
        Decision::Wait { peer: "busy".into(), why: WaitWhy::PeerRunsIt }
    );
}

#[test]
fn the_wait_ends_at_two_hours_but_a_peers_copy_is_still_taken() {
    let snv = catalog(vec![], &[Video]);
    let reads = [read("snv", Some(&snv))];
    let just_under = MAX_PEER_WAIT - Duration::from_secs(1);
    assert!(matches!(decide(Job::Download, YT, &reads, Some(just_under)), Decision::Wait { .. }));
    assert_eq!(decide(Job::Download, YT, &reads, Some(MAX_PEER_WAIT)), Decision::Local(LocalWhy::WaitedLongEnough));
    let has = catalog(vec![art(Audio, MEDIA_VERSION), art(Video, MEDIA_VERSION)], &[]);
    let late = decide(Job::Download, YT, &[read("snv", Some(&has))], Some(3 * MAX_PEER_WAIT));
    assert!(matches!(late, Decision::Fetch { .. }));
    assert_eq!(MAX_PEER_WAIT, Duration::from_secs(7_200));
}

#[test]
fn a_recheck_is_a_quarter_of_the_wait_2_to_20_min_never_past_the_bound() {
    let m = |n: u64| Duration::from_secs(n * 60);
    assert_eq!(recheck_after(Duration::ZERO), m(2));
    assert_eq!(recheck_after(m(8)), m(2), "the floor exactly");
    assert_eq!(recheck_after(m(40)), m(10));
    assert_eq!(recheck_after(m(60)), m(15));
    assert_eq!(recheck_after(m(80)), m(20), "the ceiling exactly");
    assert_eq!(recheck_after(m(100)), m(20));
    assert_eq!(recheck_after(m(110)), m(10), "only what is left of the 2 h");
    assert_eq!(recheck_after(MAX_PEER_WAIT - Duration::from_secs(30)), m(1), "at least a minute");
    assert_eq!(recheck_after(m(180)), m(1));
}
```

(`Decision` needs `Debug, PartialEq, Eq`; `Artifact` already has them.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::decide` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/decide.rs`

```rust
//! #229: ask first — whether a heavy job is fetched from a peer, waited for,
//! or run here. Pure: the peers' catalogs and the time already waited in, the
//! decision out (`ask.rs` does the reading and the recording).
//!
//! 1. A peer that holds every artifact the job needs, at a version this node
//!    takes → Fetch (even after the wait bound).
//! 2. Waited ≥ [`MAX_PEER_WAIT`] → Local.
//! 3. A peer running a job that makes those kinds → Wait.
//! 4. A peer whose catalog could not be read → Wait (an outage is not
//!    "nobody has it"; the same bound applies).
//! 5. Else → Local.

use std::time::Duration;

use super::kind::{Job, acceptable};
use super::wire::{Artifact, Catalog};

/// How long a job waits for its peers before it runs here (spec: "~2 h").
pub const MAX_PEER_WAIT: Duration = Duration::from_secs(2 * 60 * 60);
const MIN_RECHECK: Duration = Duration::from_secs(2 * 60);
const MAX_RECHECK: Duration = Duration::from_secs(20 * 60);
const LAST_RECHECK: Duration = Duration::from_secs(60);

/// One peer's catalog as read now; `None` = it could not be read.
#[derive(Debug, Clone, Copy)]
pub struct PeerRead<'a> {
    pub peer: &'a str,
    pub catalog: Option<&'a Catalog>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Fetch { peer: String, artifacts: Vec<Artifact> },
    Wait { peer: String, why: WaitWhy },
    Local(LocalWhy),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitWhy {
    PeerRunsIt,
    PeerUnreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalWhy {
    NoPeers,
    NobodyHasIt,
    WaitedLongEnough,
}

pub fn decide(job: Job, youtube_id: &str, reads: &[PeerRead<'_>], waited: Option<Duration>) -> Decision {
    if reads.is_empty() {
        return Decision::Local(LocalWhy::NoPeers);
    }
    for r in reads {
        if let Some(c) = r.catalog
            && let Some(artifacts) = holds(c, job, youtube_id)
        {
            return Decision::Fetch { peer: r.peer.to_string(), artifacts };
        }
    }
    if waited.is_some_and(|w| w >= MAX_PEER_WAIT) {
        return Decision::Local(LocalWhy::WaitedLongEnough);
    }
    if let Some(r) = reads
        .iter()
        .find(|r| r.catalog.is_some_and(|c| c.announces(youtube_id, job.makes())))
    {
        return Decision::Wait { peer: r.peer.to_string(), why: WaitWhy::PeerRunsIt };
    }
    if let Some(r) = reads.iter().find(|r| r.catalog.is_none()) {
        return Decision::Wait { peer: r.peer.to_string(), why: WaitWhy::PeerUnreadable };
    }
    Decision::Local(LocalWhy::NobodyHasIt)
}

/// Every artifact `job` needs for `youtube_id`, in [`Job::needs`] order, when
/// `catalog` holds them all at a version this node takes.
pub fn holds(catalog: &Catalog, job: Job, youtube_id: &str) -> Option<Vec<Artifact>> {
    job.needs()
        .iter()
        .map(|kind| {
            catalog
                .artifacts
                .iter()
                .find(|a| a.youtube_id == youtube_id && a.kind == *kind && acceptable(*kind, a.version))
                .cloned()
        })
        .collect()
}

/// The next re-check after waiting `waited`: a quarter of it, 2 to 20 min,
/// never past [`MAX_PEER_WAIT`], and at least a minute.
pub fn recheck_after(waited: Duration) -> Duration {
    let next = (waited / 4).clamp(MIN_RECHECK, MAX_RECHECK);
    let left = MAX_PEER_WAIT.saturating_sub(waited);
    next.min(left).max(LAST_RECHECK)
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::decide` — Expected: PASS (10 tests).
- [ ] **Step 5: Commit** — `test(#229): ask-first decision table` then `feat(#229): peer::decide — fetch, wait (≤ 2 h) or here`.

### Task 7.2: V30 `peer_waits` + `peer_fetches` + job defers

**Files:**
- Modify: `crates/sp-server/src/db/mod.rs` (`(30, MIGRATION_V30)`, the const, the `tests_v30` hook), `crates/sp-server/src/db/models_peer.rs`, `crates/sp-server/src/db/models_peer_tests.rs`
- Create: `crates/sp-server/src/db/mod_tests_v30.rs`

**Interfaces:**
- Produces: tables `peer_waits(youtube_id, job, since_ms)`, `peer_fetches(youtube_id, kind, node, version, sha256, fetched_at_ms)`; `models_peer::{waited(&SqlitePool, youtube_id, job: &str, now_ms: i64) -> Result<Option<Duration>, sqlx::Error>, start_wait(pool, youtube_id, job, now_ms)` (keeps the first), `end_wait(pool, youtube_id, job)`, `record_fetch(pool, youtube_id, kind: &str, node: &str, version: u32, sha256: &str, at_ms: i64)`, `fetch_record(pool, youtube_id, kind) -> Result<Option<(String, i64, String)>, sqlx::Error>` (node, version, sha), `defer_download(pool, video_id, wait: Duration)` (sets `next_attempt_at`, RFC 3339 like `record_download_failure`, attempts untouched), `defer_stems(pool, video_id, wait)` (sets `stem_next_attempt_at`, status + attempts untouched)}`.

- [ ] **Step 1: Write the failing tests.** `mod_tests_v30.rs`:

```rust
//! V30 (#229): ask-first waits + provenance.

use super::test_helpers::{apply_first_n, apply_upto};
use super::*;

#[tokio::test]
async fn migration_v30_creates_the_wait_and_fetch_tables() {
    let pool = create_memory_pool().await.unwrap();
    apply_first_n(&pool, 29).await;
    apply_upto(&pool, 30).await;
    let wait = "INSERT INTO peer_waits (youtube_id, job, since_ms) VALUES ('aaaaaaaaaaa', 'stems', 1)";
    sqlx::query(wait).execute(&pool).await.unwrap();
    assert!(sqlx::query(wait).execute(&pool).await.is_err(), "one wait per video + job");
    let fetch = "INSERT INTO peer_fetches (youtube_id, kind, node, version, sha256, fetched_at_ms) \
                 VALUES ('aaaaaaaaaaa', 'audio', 'snv', 1, 'ab', 2)";
    sqlx::query(fetch).execute(&pool).await.unwrap();
    assert!(sqlx::query(fetch).execute(&pool).await.is_err(), "one record per video + kind");
    assert_eq!(current_schema_version(&pool).await.unwrap(), 30);
}
```

Append to `models_peer_tests.rs`:

```rust
#[tokio::test]
async fn a_wait_keeps_its_first_start_until_it_ends() {
    let pool = pool().await;
    assert_eq!(waited(&pool, "aaaaaaaaaaa", "stems", 5_000).await.unwrap(), None);
    start_wait(&pool, "aaaaaaaaaaa", "stems", 1_000).await.unwrap();
    start_wait(&pool, "aaaaaaaaaaa", "stems", 4_000).await.unwrap();
    assert_eq!(waited(&pool, "aaaaaaaaaaa", "stems", 61_000).await.unwrap(), Some(Duration::from_secs(60)));
    assert_eq!(waited(&pool, "aaaaaaaaaaa", "lyrics", 61_000).await.unwrap(), None, "per job");
    assert_eq!(waited(&pool, "aaaaaaaaaaa", "stems", 500).await.unwrap(), Some(Duration::ZERO), "a clock step back");
    end_wait(&pool, "aaaaaaaaaaa", "stems").await.unwrap();
    assert_eq!(waited(&pool, "aaaaaaaaaaa", "stems", 61_000).await.unwrap(), None);
}

#[tokio::test]
async fn a_fetch_record_is_kept_per_video_and_kind() {
    let pool = pool().await;
    record_fetch(&pool, "aaaaaaaaaaa", "audio", "snv", 1, "s1", 10).await.unwrap();
    record_fetch(&pool, "aaaaaaaaaaa", "audio", "snv", 1, "s2", 20).await.unwrap();
    assert_eq!(
        fetch_record(&pool, "aaaaaaaaaaa", "audio").await.unwrap(),
        Some(("snv".to_string(), 1, "s2".to_string()))
    );
    assert_eq!(fetch_record(&pool, "aaaaaaaaaaa", "video").await.unwrap(), None);
}

#[tokio::test]
async fn a_job_defer_sets_only_its_recheck() {
    let pool = pool().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (1, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, normalized, download_attempts, stem_attempts) \
         VALUES (1, 'aaaaaaaaaaa', 0, 2, 3) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let before = chrono::Utc::now();
    defer_download(&pool, id, Duration::from_secs(120)).await.unwrap();
    defer_stems(&pool, id, Duration::from_secs(300)).await.unwrap();
    let (next, attempts, stem_next, stem_attempts, stem_status): (String, i64, String, i64, Option<String>) =
        sqlx::query_as(
            "SELECT next_attempt_at, download_attempts, stem_next_attempt_at, stem_attempts, stem_status \
             FROM videos WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let next = chrono::DateTime::parse_from_rfc3339(&next).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - before).num_seconds();
    assert!((119..=125).contains(&ahead), "{ahead}");
    assert_eq!((attempts, stem_attempts, stem_status), (2, 3, None));
    let stem_next = chrono::DateTime::parse_from_rfc3339(&stem_next).unwrap();
    let stem_ahead = (stem_next.with_timezone(&chrono::Utc) - before).num_seconds();
    assert!((299..=305).contains(&stem_ahead), "{stem_ahead}");
}
```

(Add `use std::time::Duration;` to the test file's imports.)

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server db::` — Expected now: compile FAIL.

- [ ] **Step 3: Implement.** `db/mod.rs`:

```rust
// V30 (#229): ask first. `peer_waits` = since when a job waits for a peer
// (the 2 h bound counts from the FIRST wait); `peer_fetches` = which node an
// artifact came from (source = peer:<node>); the row's own source columns
// keep their meaning.
const MIGRATION_V30: &str = "
CREATE TABLE peer_waits (
    youtube_id TEXT NOT NULL,
    job TEXT NOT NULL,
    since_ms INTEGER NOT NULL,
    PRIMARY KEY (youtube_id, job)
);
CREATE TABLE peer_fetches (
    youtube_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    node TEXT NOT NULL,
    version INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    fetched_at_ms INTEGER NOT NULL,
    PRIMARY KEY (youtube_id, kind)
);
";
```

plus `(30, MIGRATION_V30),` and the hook `#[path = "mod_tests_v30.rs"] #[cfg(test)] mod tests_v30;`. Append to `models_peer.rs` (before its test hook; add `use std::time::Duration;`; update the module doc to name V30):

```rust
/// How long `job` of `youtube_id` has waited for a peer at `now_ms`; `None` = not waiting.
pub async fn waited(pool: &SqlitePool, youtube_id: &str, job: &str, now_ms: i64) -> Result<Option<Duration>, sqlx::Error> {
    let since: Option<i64> = sqlx::query_scalar("SELECT since_ms FROM peer_waits WHERE youtube_id = ? AND job = ?")
        .bind(youtube_id)
        .bind(job)
        .fetch_optional(pool)
        .await?;
    Ok(since.map(|s| Duration::from_millis(u64::try_from(now_ms - s).unwrap_or(0))))
}

/// The job waits from `now_ms` — unless it already waits (the first start counts).
pub async fn start_wait(pool: &SqlitePool, youtube_id: &str, job: &str, now_ms: i64) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO peer_waits (youtube_id, job, since_ms) VALUES (?, ?, ?)")
        .bind(youtube_id)
        .bind(job)
        .bind(now_ms)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn end_wait(pool: &SqlitePool, youtube_id: &str, job: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM peer_waits WHERE youtube_id = ? AND job = ?")
        .bind(youtube_id)
        .bind(job)
        .execute(pool)
        .await?;
    Ok(())
}

/// `kind` of `youtube_id` came from peer `node` (the latest fetch wins).
pub async fn record_fetch(
    pool: &SqlitePool,
    youtube_id: &str,
    kind: &str,
    node: &str,
    version: u32,
    sha256: &str,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO peer_fetches (youtube_id, kind, node, version, sha256, fetched_at_ms) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(youtube_id)
    .bind(kind)
    .bind(node)
    .bind(i64::from(version))
    .bind(sha256)
    .bind(at_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// `(node, version, sha256)` of the last fetch of `kind` of `youtube_id`.
pub async fn fetch_record(pool: &SqlitePool, youtube_id: &str, kind: &str) -> Result<Option<(String, i64, String)>, sqlx::Error> {
    sqlx::query_as("SELECT node, version, sha256 FROM peer_fetches WHERE youtube_id = ? AND kind = ?")
        .bind(youtube_id)
        .bind(kind)
        .fetch_optional(pool)
        .await
}

/// The download of row `video_id` is picked again after `wait` — no attempt
/// counted (`fetch_next_unprocessed` compares the same RFC 3339 form).
pub async fn defer_download(pool: &SqlitePool, video_id: i64, wait: Duration) -> Result<(), sqlx::Error> {
    let at = chrono::Utc::now() + chrono::Duration::from_std(wait).unwrap_or_else(|_| chrono::Duration::zero());
    sqlx::query("UPDATE videos SET next_attempt_at = ? WHERE id = ?")
        .bind(at.to_rfc3339())
        .bind(video_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The stems of row `video_id` are picked again after `wait` — status and
/// attempts untouched (the stem selector compares `stem_next_attempt_at`).
pub async fn defer_stems(pool: &SqlitePool, video_id: i64, wait: Duration) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET stem_next_attempt_at = \
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now', printf('+%d seconds', ?)) WHERE id = ?",
    )
    .bind(i64::try_from(wait.as_secs()).unwrap_or(i64::MAX))
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}
```

(The lyrics job defers through the existing `crate::db::models::record_lyrics_wait`.)

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server db::` — Expected: PASS (V29's test uses `apply_upto(29)`, so V30 never runs inside it).
- [ ] **Step 5: Commit** — `test(#229): V30 waits + provenance, job defers` then `feat(#229): V30 peer_waits + peer_fetches, download/stems defers`.

### Task 7.3: `peer::ask` — `Exchange::ask`, `fetch_failed`, `fetched`

**Files:**
- Create: `crates/sp-server/src/peer/ask.rs`, `crates/sp-server/src/peer/ask_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod ask;` and `pub use ask::{Ask, FetchPlan, PeerStep};` under the module list)

**Interfaces:**
- Consumes: `decide::*`, `models_peer::{waited, start_wait, end_wait, record_fetch}`, `PeerClient::catalog`, `Exchange::announce`, `NodeConfig`.
- Produces: `FetchPlan { peer: PeerConfig, artifacts: Vec<Artifact> }` with `fn artifact(&self, ArtifactKind) -> Result<&Artifact, PeerError>`; `#[must_use] enum Ask { Fetch(FetchPlan), Wait { peer: String, recheck: Duration }, Local(JobGuard) }`; `#[must_use] enum PeerStep { Done, Deferred, Local(Option<JobGuard>) }` (what each worker hook returns: `Done` = the artifacts are in place and recorded; `Deferred` = re-picked later, no attempt counted; `Local(guard)` = run the job here, announced while `guard` lives; `Local(None)` = no exchange wired, e.g. a unit-test worker); `Exchange::ask(&self, Job, &str) -> Ask`, `Exchange::fetch_failed(&self, Job, youtube_id, peer: &str, &PeerError) -> Duration`, `Exchange::fetched(&self, Job, youtube_id, peer: &str, &[Artifact])`.

- [ ] **Step 1: Write the failing tests** `ask_tests.rs`

```rust
//! #229 `Exchange::ask`, over two real nodes.

use super::*;
use crate::db::models_peer::{fetch_record, waited};
use crate::peer::client::PeerError;
use crate::peer::decide::MAX_PEER_WAIT;
use crate::peer::kind::{ArtifactKind, Job};
use crate::peer::rig::{SNV_KEY, TestNode};
use crate::peer::wire::now_ms;
use std::time::Duration;

const YT: &str = "aaaaaaaaaaa";

/// SNV serving one hashed song; PP asking SNV.
async fn snv_and_pp() -> (TestNode, TestNode) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    (snv, pp)
}

async fn wait_rows(node: &TestNode) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits").fetch_one(node.pool()).await.unwrap()
}

#[tokio::test]
async fn with_no_peers_the_job_runs_here_announced_and_nothing_is_written() {
    let pp = TestNode::start("pp", None).await;
    let Ask::Local(guard) = pp.ex.ask(Job::Stems, YT).await else { panic!("expected Local") };
    assert_eq!(pp.ex.board.snapshot("pp").len(), 2, "both stems announced");
    drop(guard);
    assert!(pp.ex.board.snapshot("pp").is_empty());
    assert_eq!(wait_rows(&pp).await, 0);
}

#[tokio::test]
async fn a_peer_that_has_it_is_fetched_from() {
    let (_snv, pp) = snv_and_pp().await;
    let Ask::Fetch(plan) = pp.ex.ask(Job::Download, YT).await else { panic!("expected Fetch") };
    assert_eq!(plan.peer.name, "snv");
    assert_eq!(plan.artifact(ArtifactKind::Video).unwrap().size, 2_000);
    assert_eq!(plan.artifact(ArtifactKind::Audio).unwrap().size, 3_000);
    assert!(plan.artifact(ArtifactKind::Lyrics).is_err());
    assert_eq!(wait_rows(&pp).await, 0);
}

#[tokio::test]
async fn a_peer_running_the_job_is_waited_for_from_the_first_wait() {
    let (snv, pp) = snv_and_pp().await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Stems);
    let Ask::Wait { peer, recheck } = pp.ex.ask(Job::Stems, "bbbbbbbbbbb").await else { panic!("expected Wait") };
    assert_eq!((peer.as_str(), recheck), ("snv", Duration::from_secs(120)));
    let first = waited(pp.pool(), "bbbbbbbbbbb", "stems", now_ms()).await.unwrap().unwrap();
    pp.ex.client.forget_catalog("snv");
    let Ask::Wait { .. } = pp.ex.ask(Job::Stems, "bbbbbbbbbbb").await else { panic!("still Wait") };
    let second = waited(pp.pool(), "bbbbbbbbbbb", "stems", now_ms()).await.unwrap().unwrap();
    assert!(second >= first, "the first start is kept");
}

#[tokio::test]
async fn after_two_hours_the_job_runs_here_and_the_wait_ends() {
    let (snv, pp) = snv_and_pp().await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Stems);
    let long_ago = now_ms() - i64::try_from(MAX_PEER_WAIT.as_millis()).unwrap() - 1_000;
    crate::db::models_peer::start_wait(pp.pool(), "bbbbbbbbbbb", "stems", long_ago).await.unwrap();
    let Ask::Local(_guard) = pp.ex.ask(Job::Stems, "bbbbbbbbbbb").await else { panic!("expected Local") };
    assert_eq!(wait_rows(&pp).await, 0);
}

#[tokio::test]
async fn an_unreachable_peer_is_waited_for() {
    let pp = TestNode::start("pp", None).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gone = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut snv = pp.as_peer(SNV_KEY);
    snv.name = "snv".into();
    snv.base_url = gone;
    pp.set_peers(&[snv]).await;
    let Ask::Wait { peer, .. } = pp.ex.ask(Job::Lyrics, YT).await else { panic!("expected Wait") };
    assert_eq!(peer, "snv");
}

#[tokio::test]
async fn settings_that_do_not_hold_run_the_job_here() {
    let pp = TestNode::start("pp", None).await;
    crate::db::models::set_setting(pp.pool(), "peers", "not a list").await.unwrap();
    let Ask::Local(_guard) = pp.ex.ask(Job::Lyrics, YT).await else { panic!("expected Local") };
    assert_eq!(wait_rows(&pp).await, 0);
}

#[tokio::test]
async fn a_failed_fetch_waits_and_a_done_one_records_its_origin() {
    let (_snv, pp) = snv_and_pp().await;
    let recheck = pp.ex.fetch_failed(Job::Download, YT, "snv", &PeerError::NotFound).await;
    assert_eq!(recheck, Duration::from_secs(120));
    assert_eq!(wait_rows(&pp).await, 1);
    let Ask::Fetch(plan) = pp.ex.ask(Job::Download, YT).await else { panic!("expected Fetch") };
    pp.ex.fetched(Job::Download, YT, "snv", &plan.artifacts).await;
    assert_eq!(wait_rows(&pp).await, 0);
    let (node, version, sha) = fetch_record(pp.pool(), YT, "audio").await.unwrap().unwrap();
    assert_eq!((node.as_str(), version), ("snv", 1));
    assert_eq!(sha, plan.artifact(ArtifactKind::Audio).unwrap().sha256);
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::ask` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/ask.rs`

```rust
//! #229: ask first. Before a heavy job a node reads its peers' catalogs and
//! fetches what a peer has, waits (≤ 2 h) for what a peer is making, or runs
//! the job itself — announced in its own catalog for as long as the returned
//! guard lives. With no peers (SNV today) the answer is "here" with no network
//! and no DB write.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::Exchange;
use super::board::JobGuard;
use super::client::PeerError;
use super::config::{NodeConfig, PeerConfig};
use super::decide::{Decision, LocalWhy, PeerRead, decide, recheck_after};
use super::kind::{ArtifactKind, Job};
use super::wire::{Artifact, Catalog, now_ms};
use crate::db::models_peer;

/// What to fetch, from whom.
#[derive(Debug, Clone)]
pub struct FetchPlan {
    pub peer: PeerConfig,
    pub artifacts: Vec<Artifact>,
}

impl FetchPlan {
    pub fn artifact(&self, kind: ArtifactKind) -> Result<&Artifact, PeerError> {
        self.artifacts
            .iter()
            .find(|a| a.kind == kind)
            .ok_or_else(|| PeerError::BadResponse(format!("the plan has no {}", kind.as_str())))
    }
}

/// The answer to "ask first".
#[must_use = "a Local answer's guard announces the job; a Wait must be recorded on the row"]
pub enum Ask {
    Fetch(FetchPlan),
    Wait { peer: String, recheck: Duration },
    Local(JobGuard),
}

/// What a worker's hook does next.
#[must_use = "Local carries the job's announcement"]
pub enum PeerStep {
    /// The artifacts are in place and recorded: the job is done.
    Done,
    /// Re-picked later, no attempt counted.
    Deferred,
    /// Run the job here, announced while the guard lives (`None`: no exchange wired).
    Local(Option<JobGuard>),
}

impl Exchange {
    /// Ask the peers about `job` for `youtube_id`.
    pub async fn ask(&self, job: Job, youtube_id: &str) -> Ask {
        let cfg = match NodeConfig::load(&self.pool).await {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(youtube_id, job = job.as_str(), error = %e, "exchange: the settings do not hold - processing here");
                return Ask::Local(self.announce(youtube_id, job));
            }
        };
        if !cfg.asking() {
            return Ask::Local(self.announce(youtube_id, job));
        }
        let mut catalogs: Vec<(&str, Option<Arc<Catalog>>)> = Vec::new();
        for peer in &cfg.peers {
            let read = match self.client.catalog(peer).await {
                Ok(c) => Some(c),
                Err(e) => {
                    debug!(peer = %peer.name, error = %e, "exchange: a peer's catalog could not be read");
                    None
                }
            };
            catalogs.push((peer.name.as_str(), read));
        }
        let now = now_ms();
        let waited = match models_peer::waited(&self.pool, youtube_id, job.as_str(), now).await {
            Ok(w) => w,
            Err(e) => {
                warn!(youtube_id, %e, "exchange: reading the wait failed");
                None
            }
        };
        let reads: Vec<PeerRead<'_>> = catalogs
            .iter()
            .map(|(peer, c)| PeerRead { peer: *peer, catalog: c.as_deref() })
            .collect();
        match decide(job, youtube_id, &reads, waited) {
            Decision::Fetch { peer, artifacts } => match cfg.peer(&peer) {
                Some(p) => Ask::Fetch(FetchPlan { peer: p.clone(), artifacts }),
                None => Ask::Local(self.announce(youtube_id, job)),
            },
            Decision::Wait { peer, why } => {
                if let Err(e) = models_peer::start_wait(&self.pool, youtube_id, job.as_str(), now).await {
                    warn!(youtube_id, %e, "exchange: recording the wait failed");
                }
                let recheck = recheck_after(waited.unwrap_or_default());
                info!(
                    youtube_id,
                    job = job.as_str(),
                    peer = %peer,
                    ?why,
                    waited_s = waited.map_or(0, |w| w.as_secs()),
                    recheck_s = recheck.as_secs(),
                    "exchange: a peer will have it - waiting"
                );
                Ask::Wait { peer, recheck }
            }
            Decision::Local(why) => {
                if let Err(e) = models_peer::end_wait(&self.pool, youtube_id, job.as_str()).await {
                    warn!(youtube_id, %e, "exchange: ending the wait failed");
                }
                if why == LocalWhy::WaitedLongEnough {
                    info!(youtube_id, job = job.as_str(), "exchange: waited 2 h for the peers - processing here");
                } else {
                    debug!(youtube_id, job = job.as_str(), ?why, "exchange: no peer has it - processing here");
                }
                Ask::Local(self.announce(youtube_id, job))
            }
        }
    }

    /// A fetch from `peer` did not work: the job waits (counted against the
    /// 2 h bound) and asks again after the returned recheck.
    pub async fn fetch_failed(&self, job: Job, youtube_id: &str, peer: &str, error: &PeerError) -> Duration {
        let now = now_ms();
        if let Err(e) = models_peer::start_wait(&self.pool, youtube_id, job.as_str(), now).await {
            warn!(youtube_id, %e, "exchange: recording the wait failed");
        }
        let waited = models_peer::waited(&self.pool, youtube_id, job.as_str(), now)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        warn!(youtube_id, job = job.as_str(), peer, %error, "exchange: fetching from a peer failed - asking again later");
        recheck_after(waited)
    }

    /// `job` is done with what `peer` had: the wait ends and each artifact's
    /// origin is recorded (`source = peer:<node>`).
    pub async fn fetched(&self, job: Job, youtube_id: &str, peer: &str, artifacts: &[Artifact]) {
        if let Err(e) = models_peer::end_wait(&self.pool, youtube_id, job.as_str()).await {
            warn!(youtube_id, %e, "exchange: ending the wait failed");
        }
        for a in artifacts {
            let recorded = models_peer::record_fetch(
                &self.pool,
                youtube_id,
                a.kind.as_str(),
                peer,
                a.version,
                &a.sha256,
                now_ms(),
            )
            .await;
            if let Err(e) = recorded {
                warn!(youtube_id, %e, "exchange: recording a fetch failed");
            }
        }
        info!(youtube_id, job = job.as_str(), source = %format!("peer:{peer}"), "exchange: done with a peer's copy");
    }
}

#[cfg(test)]
#[path = "ask_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::` — Expected: PASS.
- [ ] **Step 5: Append to `.claude/rules/peer-exchange.md`** and commit (`test(#229): Exchange::ask over two nodes`, `feat(#229): peer::ask — fetch, wait or here, announced`, `docs(#229): ask-first rules`):

```markdown
## Ask first (`peer::decide`, `peer::ask`, V30)

- `decide`: a peer holding EVERY needed artifact at an accepted version →
  Fetch (even after 2 h); waited ≥ 2 h → Local; a peer announcing a job
  that makes those kinds → Wait; a peer that could not be read → Wait (an
  outage or a refused key/token is not "nobody has it"); else Local.
- `Exchange::ask` returns `Local(JobGuard)`: the job is announced in this
  node's catalog while the guard lives — hold it until the job ends.
- Waits: `peer_waits` keeps the FIRST start; rechecks back off
  (`recheck_after`: a quarter of the wait, 2–20 min, ≥ 1 min, never past
  the bound); a failed fetch (`fetch_failed`) counts as waiting. Each job
  defers its own row with no attempt counted (`defer_download`,
  `defer_stems`, `record_lyrics_wait`).
- Provenance: `peer_fetches` (`source = peer:<node>` in the log). The row's
  `metadata_source` / `lyrics_source` keep the PEER's real values (they drive
  `REPAIR_QUEUE_WHERE`, `alignment_model_for_source`, the ★ marker).
```

- [ ] **Step 6: Push the lane**, monitor CI (nothing calls `ask` yet).

---

## Lane 8 — The download job asks first (fetch instead of process)

Design comment for #229: *Approach:* `DownloadWorker::process_next` calls `peer::download::first` right after it picks a row: `Local` → the unchanged yt-dlp + loudnorm path, announced while it runs; `Wait` → the row's `next_attempt_at` moves (no attempt counted); `Fetch` → the pair is fetched as parts, named after THIS node's title (an operator's correction here first, then the peer's provider/operator title, else this node's own providers via `download_title`), renamed into the cache and recorded through `metadata::manual::record_download` — the same record path as a local download, which re-reads a correction made meanwhile under `cache::SONG_FILES` (#136). The worker then sends `processed:<id>` as after a local download. *Rejected:* adopting the peer's file NAMES (a node's names follow its own row, #136 — a correction at PP must name PP's files), and putting the logic in `downloader/` (excluded from the mutation gate; only the 10-line hook lives there). *Architektúra:* `peer::ask` + `peer::fetch` + the existing `record_download`.

### Task 8.1: `peer::download`

**Files:**
- Create: `crates/sp-server/src/peer/download.rs`, `crates/sp-server/src/peer/download_tests.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod download;`), `crates/sp-server/src/metadata/manual.rs` (`async fn manual_title` → `pub(crate) async fn manual_title`)

**Interfaces:**
- Consumes: `Exchange::{ask, fetch, fetched, fetch_failed}`, `PeerClient::video`, `models_peer::defer_download`, `crate::downloader::VideoRow { id, youtube_id, title }` (`pub(crate)`), `crate::downloader::cache::{video_filename, audio_filename}`, `crate::metadata::manual::{DownloadTitle, MANUAL_SOURCE, download_title, manual_title, record_download}`, `crate::metadata::ProviderChain`.
- Produces: `download::{first(Option<&Arc<Exchange>>, &ProviderChain, &VideoRow) -> PeerStep, adopted_title(&PeerMetadata) -> Option<DownloadTitle>}`, `pub(crate) adopt(&Exchange, &ProviderChain, &VideoRow, &FetchPlan) -> Result<(), PeerError>`.

- [ ] **Step 1: Add a counting provider to the rig** (`rig.rs`; Lane 9's repair tests reuse it)

```rust
/// A metadata provider that counts its calls and answers "Chain Song" /
/// "Chain Artist": a test sees whether this node asked its providers.
pub(crate) struct Counting(pub(crate) std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl crate::metadata::MetadataProvider for Counting {
    async fn extract(
        &self,
        _video_id: &str,
        _title: &str,
    ) -> Result<sp_core::metadata::VideoMetadata, crate::metadata::MetadataError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(sp_core::metadata::VideoMetadata {
            song: "Chain Song".into(),
            artist: "Chain Artist".into(),
            source: sp_core::metadata::MetadataSource::Gemini,
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "counting"
    }
}

/// A one-provider chain of [`Counting`] and its call counter.
pub(crate) fn counting_chain() -> (crate::metadata::ProviderChain, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (crate::metadata::ProviderChain::new(vec![Box::new(Counting(calls.clone()))]), calls)
}
```

- [ ] **Step 1b: Write the failing tests** `download_tests.rs`

```rust
//! #229: the download job asks first — two real nodes, a counting provider.

use super::*;
use crate::db::models_peer::fetch_record;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode, bytes, counting_chain};
use crate::peer::wire::PeerMetadata;
use sp_core::metadata::MetadataSource;
use std::sync::atomic::Ordering;

const YT: &str = "aaaaaaaaaaa";

/// SNV has the song hashed; PP asks SNV and has an undownloaded row of it.
async fn snv_and_pp() -> (TestNode, TestNode, VideoRow) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let id = snv.add_video(YT).await;
    snv.give_song(id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let pp_id = pp.add_video(YT).await;
    (snv, pp, VideoRow { id: pp_id, youtube_id: YT.into(), title: "A YouTube title".into() })
}

#[derive(Debug, sqlx::FromRow)]
struct RowNow {
    normalized: i64,
    song: Option<String>,
    artist: Option<String>,
    metadata_source: Option<String>,
    audio_file_path: Option<String>,
    download_attempts: i64,
    next_attempt_at: Option<String>,
}

async fn row_now(node: &TestNode, id: i64) -> RowNow {
    sqlx::query_as(
        "SELECT normalized, song, artist, metadata_source, audio_file_path, download_attempts, next_attempt_at \
         FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

fn meta(source: Option<&str>, gemini_failed: bool, song: &str) -> PeerMetadata {
    PeerMetadata {
        youtube_id: YT.into(),
        song: song.into(),
        artist: "Sinach".into(),
        metadata_source: source.map(str::to_string),
        gemini_failed,
    }
}

#[test]
fn a_peers_provider_or_operator_title_is_taken_a_parser_guess_is_not() {
    assert_eq!(MetadataSource::Gemini.as_str(), "gemini");
    assert_eq!(MetadataSource::Regex.as_str(), "regex");
    let t = adopted_title(&meta(Some("gemini"), false, "Way Maker")).unwrap();
    assert_eq!((t.song.as_str(), t.artist.as_str(), t.source, t.gemini_failed), ("Way Maker", "Sinach", "gemini", false));
    assert_eq!(adopted_title(&meta(Some("manual"), false, "Cesta")).unwrap().source, MANUAL_SOURCE);
    assert_eq!(adopted_title(&meta(Some("regex"), false, "Odd")).unwrap().source, "regex");
    assert_eq!(adopted_title(&meta(Some("gemini"), true, "Guess")), None, "a parser guess");
    assert_eq!(adopted_title(&meta(Some("gemini"), false, "  ")), None, "no song");
    assert_eq!(adopted_title(&meta(Some("someday-a-new-source"), false, "X")), None, "unknown source");
    assert_eq!(adopted_title(&meta(None, false, "X")), None);
}

/// The spec's "a peer has the artifact, so the other node fetches and processes nothing".
#[tokio::test]
async fn a_peers_pair_is_taken_and_nothing_runs_here() {
    let (_snv, pp, row) = snv_and_pp().await;
    let (chain, calls) = counting_chain();
    let step = first(Some(&pp.ex), &chain, &row).await;
    assert!(matches!(step, PeerStep::Done));
    let now = row_now(&pp, row.id).await;
    assert_eq!(now.normalized, 1);
    assert_eq!((now.song.as_deref(), now.artist.as_deref()), (Some("Way Maker"), Some("Sinach")));
    assert_eq!(now.metadata_source.as_deref(), Some("gemini"));
    assert_eq!(now.download_attempts, 0);
    let audio = pp.cache().join(audio_filename("Way Maker", "Sinach", YT, false));
    let video = pp.cache().join(video_filename("Way Maker", "Sinach", YT, false));
    assert_eq!(now.audio_file_path.as_deref(), Some(audio.to_string_lossy().as_ref()));
    assert_eq!(std::fs::read(&audio).unwrap(), bytes(3_000, 2));
    assert_eq!(std::fs::read(&video).unwrap(), bytes(2_000, 1));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no provider was asked");
    assert_eq!(fetch_record(pp.pool(), YT, "audio").await.unwrap().unwrap().0, "snv");
    assert_eq!(std::fs::read_dir(pp.ex.parts_dir()).unwrap().count(), 0, "no part left");
}

#[tokio::test]
async fn an_operator_correction_here_names_the_pair() {
    let (_snv, pp, row) = snv_and_pp().await;
    let other = pp.add_video_to(2, YT).await;
    sqlx::query("UPDATE videos SET song = 'Cesta', artist = 'Zbor', metadata_source = 'manual' WHERE id = ?")
        .bind(other)
        .execute(pp.pool())
        .await
        .unwrap();
    let (chain, calls) = counting_chain();
    assert!(matches!(first(Some(&pp.ex), &chain, &row).await, PeerStep::Done));
    let now = row_now(&pp, row.id).await;
    assert_eq!((now.song.as_deref(), now.metadata_source.as_deref()), (Some("Cesta"), Some("manual")));
    assert!(pp.cache().join(audio_filename("Cesta", "Zbor", YT, false)).exists());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_peers_parser_title_asks_this_nodes_providers() {
    let (snv, pp, row) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET gemini_failed = 1, metadata_source = 'regex' WHERE youtube_id = ?")
        .bind(YT)
        .execute(snv.pool())
        .await
        .unwrap();
    let (chain, calls) = counting_chain();
    assert!(matches!(first(Some(&pp.ex), &chain, &row).await, PeerStep::Done));
    assert_eq!(row_now(&pp, row.id).await.song.as_deref(), Some("Chain Song"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// The spec's "sha256 mismatch → discard and retry".
#[tokio::test]
async fn a_sha_mismatch_defers_then_the_retry_takes_the_pair() {
    let (snv, pp, row) = snv_and_pp().await;
    let wrong = crate::peer::hash::sha256_hex(b"not the audio");
    sqlx::query("UPDATE peer_hashes SET sha256 = ? WHERE path LIKE '%_audio.flac'")
        .bind(&wrong)
        .execute(snv.pool())
        .await
        .unwrap();
    let (chain, _) = counting_chain();
    assert!(matches!(first(Some(&pp.ex), &chain, &row).await, PeerStep::Deferred));
    let now = row_now(&pp, row.id).await;
    assert_eq!((now.normalized, now.download_attempts), (0, 0));
    assert!(now.next_attempt_at.is_some(), "re-picked later");
    sqlx::query("DELETE FROM peer_hashes").execute(snv.pool()).await.unwrap();
    snv.hash_now().await;
    assert!(matches!(first(Some(&pp.ex), &chain, &row).await, PeerStep::Done));
    assert_eq!(row_now(&pp, row.id).await.normalized, 1);
}

/// The spec's "a peer is running the job, so the other waits".
#[tokio::test]
async fn a_peer_downloading_it_is_waited_for_without_an_attempt() {
    let (snv, pp, _) = snv_and_pp().await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Download);
    let id = pp.add_video("bbbbbbbbbbb").await;
    let row = VideoRow { id, youtube_id: "bbbbbbbbbbb".into(), title: "t".into() };
    let (chain, calls) = counting_chain();
    assert!(matches!(first(Some(&pp.ex), &chain, &row).await, PeerStep::Deferred));
    let now = row_now(&pp, id).await;
    assert_eq!((now.normalized, now.download_attempts), (0, 0));
    let next = chrono::DateTime::parse_from_rfc3339(now.next_attempt_at.as_deref().unwrap()).unwrap();
    let ahead = (next.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
    assert!((100..=125).contains(&ahead), "{ahead}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// The spec's "nobody has it, so the node processes and announces".
#[tokio::test]
async fn nobody_has_it_so_it_runs_here_announced() {
    let (_snv, pp, _) = snv_and_pp().await;
    let id = pp.add_video("ccccccccccc").await;
    let row = VideoRow { id, youtube_id: "ccccccccccc".into(), title: "t".into() };
    let (chain, _) = counting_chain();
    let PeerStep::Local(Some(guard)) = first(Some(&pp.ex), &chain, &row).await else { panic!("expected Local") };
    let catalog = crate::peer::catalog::build(&pp.ex, "pp", None).await.unwrap();
    assert_eq!(catalog.jobs.len(), 3, "video, audio and metadata announced");
    drop(guard);
    assert!(crate::peer::catalog::build(&pp.ex, "pp", None).await.unwrap().jobs.is_empty());
}

#[tokio::test]
async fn with_no_exchange_the_worker_runs_as_before() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    let row = VideoRow { id, youtube_id: YT.into(), title: "t".into() };
    let (chain, _) = counting_chain();
    assert!(matches!(first(None, &chain, &row).await, PeerStep::Local(None)));
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::download` — Expected now: compile FAIL.

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/download.rs` (and make `manual_title` `pub(crate)` in `metadata/manual.rs`)

```rust
//! #229: the download job (download + normalize + metadata) asks first. A
//! peer's pair is fetched instead of yt-dlp + loudnorm and named after THIS
//! node's title: an operator's correction here, else the peer's provider or
//! operator title, else this node's own providers (`download_title`). It is
//! recorded through `metadata::manual::record_download`, the local download's
//! own record path, which re-reads a correction made meanwhile under
//! `cache::SONG_FILES` (#136, `.claude/rules/song-files.md`).

use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::config::PeerConfig;
use super::kind::{ArtifactKind, Job, METADATA_PROVIDER};
use super::wire::PeerMetadata;
use crate::db::models_peer;
use crate::downloader::VideoRow;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::metadata::ProviderChain;
use crate::metadata::manual::{
    DownloadTitle, MANUAL_SOURCE, download_title, manual_title, record_download,
};

/// The download worker's hook: fetch, wait or run here.
pub async fn first(ex: Option<&Arc<Exchange>>, chain: &ProviderChain, row: &VideoRow) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    match ex.ask(Job::Download, &row.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => {
            defer(ex, row.id, recheck).await;
            PeerStep::Deferred
        }
        Ask::Fetch(plan) => match adopt(ex, chain, row, &plan).await {
            Ok(()) => {
                ex.fetched(Job::Download, &row.youtube_id, &plan.peer.name, &plan.artifacts).await;
                PeerStep::Done
            }
            Err(e) => {
                let recheck = ex.fetch_failed(Job::Download, &row.youtube_id, &plan.peer.name, &e).await;
                defer(ex, row.id, recheck).await;
                PeerStep::Deferred
            }
        },
    }
}

async fn defer(ex: &Exchange, video_id: i64, wait: Duration) {
    if let Err(e) = models_peer::defer_download(&ex.pool, video_id, wait).await {
        warn!(video_id, %e, "exchange: deferring the download failed");
    }
}

/// The peer's pair into this node's cache under this node's title, recorded.
pub(crate) async fn adopt(ex: &Exchange, chain: &ProviderChain, row: &VideoRow, plan: &FetchPlan) -> Result<(), PeerError> {
    let video_artifact = plan.artifact(ArtifactKind::Video)?;
    let audio_artifact = plan.artifact(ArtifactKind::Audio)?;
    let title = title_for(ex, chain, row, &plan.peer).await;
    let video_part = ex.fetch(&plan.peer, video_artifact).await?;
    let audio_part = ex.fetch(&plan.peer, audio_artifact).await?;
    let gf = title.gemini_failed;
    let video = ex.cache_dir.join(video_filename(&title.song, &title.artist, &row.youtube_id, gf));
    let audio = ex.cache_dir.join(audio_filename(&title.song, &title.artist, &row.youtube_id, gf));
    tokio::fs::rename(&audio_part, &audio).await?;
    tokio::fs::rename(&video_part, &video).await?;
    record_download(&ex.pool, &ex.cache_dir, row.id, &row.youtube_id, &title, &video, &audio).await?;
    Ok(())
}

/// The title the fetched pair is named after and recorded with.
async fn title_for(ex: &Exchange, chain: &ProviderChain, row: &VideoRow, peer: &PeerConfig) -> DownloadTitle {
    if let Ok(Some((song, artist))) = manual_title(&ex.pool, &row.youtube_id).await {
        return DownloadTitle { song, artist, source: MANUAL_SOURCE, gemini_failed: false };
    }
    match ex.client.video(peer, &row.youtube_id).await {
        Ok(video) => {
            if let Some(title) = adopted_title(&video.metadata) {
                return title;
            }
        }
        Err(e) => warn!(
            youtube_id = %row.youtube_id,
            %e,
            "exchange: reading the peer's title failed - asking this node's providers"
        ),
    }
    download_title(&ex.pool, chain, &row.youtube_id, &row.title).await
}

/// A peer's title this node takes as its own: a provider's answer or an
/// operator's correction, with a song, under a `metadata_source` this node
/// writes (`MetadataSource::as_str` or `manual`). `None` = ask this node's providers.
pub fn adopted_title(m: &PeerMetadata) -> Option<DownloadTitle> {
    if m.version() < METADATA_PROVIDER || m.song.trim().is_empty() {
        return None;
    }
    let source = match m.metadata_source.as_deref()? {
        MANUAL_SOURCE => MANUAL_SOURCE,
        "gemini" => "gemini",
        "regex" => "regex",
        _ => return None,
    };
    Some(DownloadTitle { song: m.song.clone(), artist: m.artist.clone(), source, gemini_failed: false })
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
```

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::download` — Expected: PASS (8 tests).
- [ ] **Step 5: Commit** — `test(#229): the download job asks first — fetch, wait, here, sha retry` then `feat(#229): peer::download — a peer's pair named after this node's title`.

### Task 8.2: the hook in `DownloadWorker` + wiring

**Files:**
- Create: `crates/sp-server/src/downloader/mod_tests.rs` (the existing test module, moved), `crates/sp-server/src/downloader/mod_tests_peer.rs`
- Modify: `crates/sp-server/src/downloader/mod.rs`, `crates/sp-server/src/lib.rs`

**Interfaces:**
- Consumes: `peer::download::first`, `peer::PeerStep`.
- Produces: `DownloadWorker::with_peer(self, Arc<crate::peer::Exchange>) -> Self`; `process_next` returns `true` + sends `processed:<id>` on `Done`, returns `false` on `Deferred` (no `downloading:` event), and holds the `Local` guard through the whole local download.

- [ ] **Step 1: Move the tests out (mechanical, no change).** `downloader/mod.rs` is at 993/1000 lines; its `#[cfg(test)] mod tests { … }` runs from line 573 to the end of the file:

```bash
python3 - <<'PY'
p = "crates/sp-server/src/downloader/mod.rs"
s = open(p).read()
marker = "#[cfg(test)]\nmod tests {\n"
i = s.index(marker)
body = s[i + len(marker):].rstrip()
assert body.endswith("}"), "the test module must be the file's last item"
lines = [l[4:] if l.startswith("    ") else l for l in body[:-1].rstrip().split("\n")]
open("crates/sp-server/src/downloader/mod_tests.rs", "w").write(
    "//! The download worker's tests (moved out of mod.rs for the 1000-line cap, #229).\n\n"
    + "\n".join(lines).strip("\n") + "\n"
)
open(p, "w").write(s[:i] + '#[cfg(test)]\n#[path = "mod_tests.rs"]\nmod tests;\n')
PY
cargo fmt --all && git checkout -- crates/sp-server/src/db/models.rs 2>/dev/null; wc -l crates/sp-server/src/downloader/mod.rs
git add crates/sp-server/src/downloader && git commit -m "refactor(#229): move the download worker's tests to mod_tests.rs (1000-line cap)"
```

Expected: `mod.rs` ≈ 575 lines; `git diff --stat HEAD~1` shows only the move (+/- the same test lines, re-indented).

- [ ] **Step 2: Write the failing tests** `crates/sp-server/src/downloader/mod_tests_peer.rs`

```rust
//! #229: the download worker asks its peers first. Its tools are missing on
//! purpose: a local download fails (one attempt counted) — so a row that
//! ends normalized with no attempt was never downloaded here.

use super::*;
use crate::metadata::ProviderChain;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode};
use std::sync::Arc;
use super::tools::ToolPaths;
use tokio::sync::broadcast;

const YT: &str = "aaaaaaaaaaa";

fn worker(node: &TestNode) -> DownloadWorker {
    let tools = ToolPaths {
        ytdlp: node.cache().join("no-yt-dlp"),
        ffmpeg: node.cache().join("no-ffmpeg"),
        python: None,
        deno: None,
    };
    let (events, _) = broadcast::channel(8);
    DownloadWorker::new(
        node.pool().clone(),
        tools,
        node.cache().to_path_buf(),
        node.cache().to_path_buf(),
        Arc::new(ProviderChain::new(vec![])),
        events,
        Arc::new(tokio::sync::Mutex::new(())),
    )
    .with_peer(node.ex.clone())
}

async fn state(node: &TestNode, id: i64) -> (i64, i64) {
    sqlx::query_as("SELECT normalized, download_attempts FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_download_a_peer_has_is_fetched_and_never_run() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    let w = worker(&pp);
    let mut events = w.event_tx.subscribe();
    assert!(w.process_next().await);
    assert_eq!(state(&pp, id).await, (1, 0));
    assert_eq!(events.try_recv().unwrap(), format!("processed:{YT}"));
}

#[tokio::test]
async fn with_nobody_having_it_the_worker_runs_its_own_download() {
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    let w = worker(&pp);
    assert!(!w.process_next().await, "yt-dlp is missing here");
    assert_eq!(state(&pp, id).await, (0, 1), "the local path ran and failed once");
}

#[tokio::test]
async fn a_peer_downloading_it_defers_the_row_without_an_attempt() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let _running = snv.ex.announce(YT, Job::Download);
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    let w = worker(&pp);
    let mut events = w.event_tx.subscribe();
    assert!(!w.process_next().await);
    assert_eq!(state(&pp, id).await, (0, 0));
    assert!(events.try_recv().is_err(), "no downloading:/processed: event for a deferred row");
}
```

Hook it in `downloader/mod.rs` right after the `tests` hook: `#[cfg(test)]\n#[path = "mod_tests_peer.rs"]\nmod tests_peer;`.

- [ ] **Step 3: Run (CI only):** `cargo test -p sp-server downloader::` — Expected now: compile FAIL (`with_peer`).

- [ ] **Step 4: Implement** in `downloader/mod.rs`. Field (after `event_tx`):

```rust
    /// #229: this node in the exchange — asked before each download (`None` in tests that do not need it).
    peer: Option<Arc<crate::peer::Exchange>>,
```

`new()` sets `peer: None,`; add after `new`:

```rust
    /// #229: ask the exchange's peers before each download.
    pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self {
        self.peer = Some(peer);
        self
    }
```

In `process_next`, between the `"processing video"` INFO and the `downloading:` event:

```rust
        // #229: ask the peers first (`peer::download`): a peer's pair is
        // taken, a peer's running download waited for; else run it here,
        // announced until this function returns.
        let _announced = match crate::peer::download::first(self.peer.as_ref(), &self.metadata, &row).await {
            crate::peer::PeerStep::Done => {
                let _ = self.event_tx.send(format!("processed:{}", row.youtube_id));
                return true;
            }
            crate::peer::PeerStep::Deferred => return false,
            crate::peer::PeerStep::Local(job) => job,
        };
```

In `lib.rs`: next to `let dl_metadata_chain = metadata_chain.clone();` add `let dl_exchange = exchange.clone();` (the `exchange` binding must be created BEFORE the tools task is spawned — it already is, Lane 1 placed it right after `AppState`), and chain `.with_peer(dl_exchange)` onto `downloader::DownloadWorker::new(…)`. `cargo fmt --all`; `wc -l` both files.

- [ ] **Step 5: Run (CI only):** `cargo test -p sp-server downloader::` and `cargo test -p sp-server peer::` — Expected: PASS (the moved tests unchanged).
- [ ] **Step 6: Commit** — `test(#229): the download worker asks first` then `feat(#229): DownloadWorker asks its peers first (with_peer, wired in lib.rs)`.
- [ ] **Step 7: Append to `.claude/rules/peer-exchange.md`** and commit `docs(#229): …`:

```markdown
## The download job (`peer::download`, the `DownloadWorker` hook)

- `process_next` → `peer::download::first` right after the row is picked:
  Done → `processed:<id>` + return; Deferred → `next_attempt_at` moved, no
  attempt; Local → the yt-dlp path, announced until `process_next` returns.
- The pair is named after THIS node's title: a local `manual` correction,
  else the peer's title when it is a provider's or an operator's (version
  ≥ 1, a known source), else this node's providers. Recorded by
  `record_download` (the local path's own, #136).
- `downloader/` is out of the mutation gate: logic stays in `peer/`, only
  the hook lives in `downloader/mod.rs`; its tests are `mod_tests.rs` +
  `mod_tests_peer.rs` (tools missing on purpose: an attempt counted = the
  local path ran).
```

- [ ] **Step 8: Push the lane**, monitor CI. At SNV (no peers) every download still takes the local path: `ask` answers `Local` with no network.

---

## Lane 9 — Stems, lyrics and the metadata repair ask first; PP's workers on

Design comment for #229: *Approach:* the same one-hook shape as the download job. **Stems:** after the terminal skip, `peer::stems::first` — a peer's two stems are fetched as parts and renamed under `stem_paths(<the audio the row records AFTER the transfer>)`, read under `cache::SONG_FILES` (#136), then `mark_stems_done`; a wait moves `stem_next_attempt_at` (no attempt). **Lyrics:** after a row is picked, `peer::lyrics::first` — never for an operator's own ask (`lyrics_manual_priority`, a `lyrics_override_text`); the peer's row (`/videos`) must match its catalog and must not be the Live-Translate track; a peer copy identical to what the row already serves (the same source at the same version — the daily full-mix upgrade) is "nothing newer" and the job runs here; the track is parsed as a typed `LyricsTrack` and its `source` must equal the row's; the JSON is renamed into `{yt}_lyrics.json` and `adopt_lyrics` writes the peer's source, version, alignment model, ★ and translation version (0 when this row asks another translation gender, so the local retranslate pass redoes the SK lines). **Repair:** before the providers, `peer::repair::peer_title` — a peer's provider/operator title repairs the row through the repair's own locked rename + record (moved into `apply_title`, unchanged). *Rejected:* fetching stems before the worker's venv gate (the gate order is pinned by the existing tests; a node fetches stems once it could separate them — the venv comes with the lyrics bootstrap), and adopting a peer track for a row whose operator asked THIS node (the operator's intent wins). *Architektúra:* `peer::ask` + `peer::fetch` + the existing `mark_stems_done`, `record_lyrics_wait`, the repair's rename path.

### Task 9.1: `peer::stems` + the `StemWorker` hook

**Files:**
- Create: `crates/sp-server/src/peer/stems.rs`, `crates/sp-server/src/peer/stems_tests.rs`, `crates/sp-server/src/stems/worker_tests.rs` (moved), `crates/sp-server/src/stems/worker_tests_peer.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod stems;`), `crates/sp-server/src/stems/worker.rs`

**Interfaces:**
- Consumes: `Exchange::{ask, fetch, fetched, fetch_failed}`, `models_peer::defer_stems`, `crate::db::models_stems::{StemJob, mark_stems_done}`, `crate::stems::stem_paths`, `crate::downloader::cache::SONG_FILES`.
- Produces: `peer::stems::first(Option<&Arc<Exchange>>, &StemJob) -> PeerStep`, `pub(crate) adopt(&Exchange, &StemJob, &FetchPlan) -> Result<(), PeerError>`; `StemWorker::with_peer(self, Arc<Exchange>) -> Self`.

- [ ] **Step 1: Move the stem worker's tests out (mechanical).** `stems/worker.rs` is at 970/1000; its `#[cfg(test)] mod tests { … }` (from line 606) is the file's last item:

```bash
python3 - <<'PY'
p = "crates/sp-server/src/stems/worker.rs"
s = open(p).read()
marker = "#[cfg(test)]\nmod tests {\n"
assert s.count(marker) == 1
i = s.index(marker)
body = s[i + len(marker):].rstrip()
assert body.endswith("}"), "the test module must be the file's last item"
lines = [l[4:] if l.startswith("    ") else l for l in body[:-1].rstrip().split("\n")]
open("crates/sp-server/src/stems/worker_tests.rs", "w").write(
    "//! The stem worker's tests (moved out of worker.rs for the 1000-line cap, #229).\n\n"
    + "\n".join(lines).strip("\n") + "\n"
)
open(p, "w").write(s[:i] + '#[cfg(test)]\n#[path = "worker_tests.rs"]\nmod tests;\n')
PY
cargo fmt --all && git checkout -- crates/sp-server/src/db/models.rs 2>/dev/null; wc -l crates/sp-server/src/stems/worker.rs
git add crates/sp-server/src/stems && git commit -m "refactor(#229): move the stem worker's tests to worker_tests.rs (1000-line cap)"
```

(`include_str!("../../../../scripts/stem_worker.py")` in the moved tests still resolves: the new file sits in the same directory.)

- [ ] **Step 2: Write the failing tests** `crates/sp-server/src/peer/stems_tests.rs`

```rust
//! #229: the stems job asks first.

use super::*;
use crate::peer::kind::Job;
use crate::peer::rig::{SNV_KEY, TestNode, bytes};
use std::path::{Path, PathBuf};

const YT: &str = "aaaaaaaaaaa";

/// SNV has the song and its stems hashed; PP has the song (its own files) and asks SNV.
async fn snv_and_pp() -> (TestNode, TestNode, StemJob, PathBuf) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_stems(snv_id).await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    let (_, audio) = pp.give_song(id, YT, "Cesta", "Zbor").await;
    let job = StemJob {
        video_id: id,
        youtube_id: YT.into(),
        audio_file_path: audio.to_string_lossy().into_owned(),
        duration_ms: None,
        song: None,
        artist: None,
    };
    (snv, pp, job, audio)
}

async fn stem_state(node: &TestNode, id: i64) -> (Option<String>, i64, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT stem_status, stem_attempts, stem_next_attempt_at, vocals_file_path FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn a_peers_stems_land_under_this_nodes_audio_and_are_done() {
    let (_snv, pp, job, audio) = snv_and_pp().await;
    assert!(matches!(first(Some(&pp.ex), &job).await, PeerStep::Done));
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    assert_eq!(std::fs::read(&vocals).unwrap(), bytes(1_500, 3));
    assert_eq!(std::fs::read(&instrumental).unwrap(), bytes(1_700, 4));
    let (status, attempts, _, recorded) = stem_state(&pp, job.video_id).await;
    assert_eq!((status.as_deref(), attempts), (Some("done"), 0));
    assert_eq!(recorded.as_deref(), Some(vocals.to_string_lossy().as_ref()));
}

/// Review Focus 4: a rename here while the transfer ran.
#[tokio::test]
async fn stems_fetched_during_a_rename_land_under_the_new_name() {
    let (_snv, pp, job, audio) = snv_and_pp().await;
    let Ask::Fetch(plan) = pp.ex.ask(Job::Stems, YT).await else { panic!("expected Fetch") };
    let renamed = pp.cache().join("Opravena_Zbor_aaaaaaaaaaa_normalized_audio.flac");
    std::fs::rename(&audio, &renamed).unwrap();
    sqlx::query("UPDATE videos SET audio_file_path = ? WHERE id = ?")
        .bind(renamed.to_string_lossy().to_string())
        .bind(job.video_id)
        .execute(pp.pool())
        .await
        .unwrap();
    adopt(&pp.ex, &job, &plan).await.unwrap();
    let (new_vocals, _) = crate::stems::stem_paths(&renamed);
    let (old_vocals, _) = crate::stems::stem_paths(Path::new(&job.audio_file_path));
    assert!(new_vocals.exists());
    assert!(!old_vocals.exists());
}

#[tokio::test]
async fn a_peer_separating_it_defers_without_an_attempt() {
    let (snv, pp, job, _) = snv_and_pp().await;
    let other = pp.add_video("bbbbbbbbbbb").await;
    let (_, audio) = pp.give_song(other, "bbbbbbbbbbb", "Iny", "Zbor").await;
    let _running = snv.ex.announce("bbbbbbbbbbb", Job::Stems);
    let waiting = StemJob {
        video_id: other,
        youtube_id: "bbbbbbbbbbb".into(),
        audio_file_path: audio.to_string_lossy().into_owned(),
        ..job
    };
    assert!(matches!(first(Some(&pp.ex), &waiting).await, PeerStep::Deferred));
    let (status, attempts, next, _) = stem_state(&pp, other).await;
    assert_eq!((status, attempts), (None, 0));
    assert!(next.is_some());
}

#[tokio::test]
async fn a_song_with_no_audio_here_is_deferred_not_failed() {
    let (_snv, pp, job, audio) = snv_and_pp().await;
    std::fs::remove_file(&audio).unwrap();
    assert!(matches!(first(Some(&pp.ex), &job).await, PeerStep::Deferred));
    let (status, attempts, next, _) = stem_state(&pp, job.video_id).await;
    assert_eq!((status, attempts), (None, 0));
    assert!(next.is_some());
}
```

`crates/sp-server/src/stems/worker_tests_peer.rs` (hooked in `worker.rs` with `#[cfg(test)]\n#[path = "worker_tests_peer.rs"]\nmod tests_peer;` right after the `tests` hook):

```rust
//! #229: the stem worker asks its peers first.

use super::*;
use crate::peer::rig::{SNV_KEY, TestNode};
use std::sync::Arc;
use tokio::sync::RwLock;

const YT: &str = "aaaaaaaaaaa";

/// A stem worker whose venv python "exists" (an empty stub), so a tick gets
/// past the venv gate to the job; the fresh health registry keeps the 60 s
/// startup floor, so a Local job never starts a separation here.
fn worker(node: &TestNode) -> StemWorker {
    let python = crate::lyrics::bootstrap::venv_python_path(node.cache());
    std::fs::create_dir_all(python.parent().unwrap()).unwrap();
    std::fs::write(&python, b"").unwrap();
    StemWorker::new(
        node.pool().clone(),
        node.cache().to_path_buf(),
        Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
        Arc::new(RwLock::new(crate::obs::ObsState::default())),
    )
    .with_peer(node.ex.clone())
}

async fn status(node: &TestNode, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT stem_status FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_peers_stems_are_taken_by_the_stem_worker() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_stems(snv_id).await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    worker(&pp).process_next().await;
    assert_eq!(status(&pp, id).await.as_deref(), Some("done"));
}

#[tokio::test]
async fn with_no_peers_the_stem_worker_goes_on_as_before() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let pp = TestNode::start("pp", None).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    worker(&pp).process_next().await;
    assert_eq!(status(&pp, id).await, None, "pending: the startup floor holds the separation");
    let waits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM peer_waits").fetch_one(pp.pool()).await.unwrap();
    assert_eq!(waits, 0);
}
```

- [ ] **Step 3: Run (CI only):** `cargo test -p sp-server peer::stems stems::worker` — Expected now: compile FAIL.

- [ ] **Step 4: Implement** `crates/sp-server/src/peer/stems.rs`

```rust
//! #229: the stems job asks first. A peer's two stems are fetched as parts,
//! then placed under THIS node's audio name as the row records it AFTER the
//! transfer — read under `cache::SONG_FILES`, the lock a rename holds (#136,
//! `.claude/rules/song-files.md`) — and marked done.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::kind::{ArtifactKind, Job};
use crate::db::models_peer;
use crate::db::models_stems::{StemJob, mark_stems_done};
use crate::downloader::cache::SONG_FILES;

/// The stem worker's hook: fetch, wait or separate here.
pub async fn first(ex: Option<&Arc<Exchange>>, job: &StemJob) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    match ex.ask(Job::Stems, &job.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => {
            defer(ex, job.video_id, recheck).await;
            PeerStep::Deferred
        }
        Ask::Fetch(plan) => match adopt(ex, job, &plan).await {
            Ok(()) => {
                ex.fetched(Job::Stems, &job.youtube_id, &plan.peer.name, &plan.artifacts).await;
                PeerStep::Done
            }
            Err(e) => {
                let recheck = ex.fetch_failed(Job::Stems, &job.youtube_id, &plan.peer.name, &e).await;
                defer(ex, job.video_id, recheck).await;
                PeerStep::Deferred
            }
        },
    }
}

async fn defer(ex: &Exchange, video_id: i64, wait: Duration) {
    if let Err(e) = models_peer::defer_stems(&ex.pool, video_id, wait).await {
        warn!(video_id, %e, "exchange: deferring the stems failed");
    }
}

/// The peer's stems under the row's CURRENT audio name, done.
pub(crate) async fn adopt(ex: &Exchange, job: &StemJob, plan: &FetchPlan) -> Result<(), PeerError> {
    let vocals_part = ex.fetch(&plan.peer, plan.artifact(ArtifactKind::StemVocals)?).await?;
    let instrumental_part = ex.fetch(&plan.peer, plan.artifact(ArtifactKind::StemInstrumental)?).await?;
    let _files = SONG_FILES.lock().await;
    let audio: Option<Option<String>> = sqlx::query_scalar("SELECT audio_file_path FROM videos WHERE id = ?")
        .bind(job.video_id)
        .fetch_optional(&ex.pool)
        .await?;
    let audio = audio
        .flatten()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .ok_or_else(|| PeerError::Io("the song has no audio on disk here".into()))?;
    let (vocals, instrumental) = crate::stems::stem_paths(&audio);
    tokio::fs::rename(&vocals_part, &vocals).await?;
    tokio::fs::rename(&instrumental_part, &instrumental).await?;
    mark_stems_done(&ex.pool, job.video_id, &vocals.to_string_lossy(), &instrumental.to_string_lossy()).await?;
    Ok(())
}

#[cfg(test)]
#[path = "stems_tests.rs"]
mod tests;
```

In `stems/worker.rs`: field `peer: Option<Arc<crate::peer::Exchange>>,` (doc: "#229: asked before each separation; `None` in tests that do not need it"), `peer: None,` in `new()`, and:

```rust
    /// #229: ask the exchange's peers before each separation.
    pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self {
        self.peer = Some(peer);
        self
    }
```

In `process_next`, right after the `stem_duration_too_long` block's closing `}` (before the `startup_floor_defers` check):

```rust
        // #229: ask the peers first (`peer::stems`): a peer's stems are taken,
        // a peer's running separation waited for; else separate here,
        // announced until this tick ends.
        let _announced = match crate::peer::stems::first(self.peer.as_ref(), &job).await {
            crate::peer::PeerStep::Local(guard) => guard,
            crate::peer::PeerStep::Done | crate::peer::PeerStep::Deferred => return,
        };
```

- [ ] **Step 5: Run (CI only):** `cargo test -p sp-server peer::stems stems::` — Expected: PASS.
- [ ] **Step 6: Commit** — `test(#229): the stems job asks first (incl. a rename during the transfer)` then `feat(#229): StemWorker asks its peers first; peer::stems`.

### Task 9.2: `peer::lyrics` + `adopt_lyrics` + the `LyricsWorker` hook

**Files:**
- Create: `crates/sp-server/src/peer/lyrics.rs`, `crates/sp-server/src/peer/lyrics_tests.rs`, `crates/sp-server/src/lyrics/worker_tests_peer.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod lyrics;`), `crates/sp-server/src/db/models_peer.rs` (+ `adopt_lyrics`), `crates/sp-server/src/lyrics/worker.rs`

**Interfaces:**
- Consumes: `Exchange::{ask, fetch, fetched, fetch_failed, announce}`, `PeerClient::video`, `wire::PeerLyrics`, `crate::db::models::{VideoLyricsRow, record_lyrics_wait}`, `crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE`, `sp_core::lyrics::LyricsTrack`.
- Produces: `peer::lyrics::first(Option<&Arc<Exchange>>, &VideoLyricsRow) -> PeerStep`, `pub(crate) adopt(&Exchange, &VideoLyricsRow, &FetchPlan) -> Result<Adopted, PeerError>`, `pub(crate) enum Adopted { Track, NothingNewer }`; `models_peer::adopt_lyrics(&SqlitePool, video_id: i64, &PeerLyrics) -> Result<(), sqlx::Error>`; `LyricsWorker::with_peer(self, Arc<Exchange>) -> Self`.

- [ ] **Step 1: Write the failing tests** `crates/sp-server/src/peer/lyrics_tests.rs`

```rust
//! #229: the lyrics job asks first.

use super::*;
use crate::db::models::VideoLyricsRow;
use crate::peer::rig::{SNV_KEY, TestNode};

const YT: &str = "aaaaaaaaaaa";
const ROW_SQL: &str = "SELECT v.id, v.youtube_id, COALESCE(v.song, '') AS song, \
     COALESCE(v.artist, '') AS artist, v.duration_ms, v.audio_file_path, p.youtube_url, \
     v.lyrics_override_text, v.lyrics_time_offset_ms, v.spotify_track_id, v.spotify_resolved_at \
     FROM videos v JOIN playlists p ON p.id = v.playlist_id WHERE v.id = ?";

async fn lyrics_row(node: &TestNode, id: i64) -> VideoLyricsRow {
    sqlx::query_as(ROW_SQL).bind(id).fetch_one(node.pool()).await.unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct LyricsNow {
    has_lyrics: i64,
    lyrics_source: Option<String>,
    lyrics_pipeline_version: i64,
    lyrics_reference: i64,
    lyrics_translation_version: i64,
    lyrics_translation_gender: Option<String>,
}

async fn lyrics_now(node: &TestNode, id: i64) -> LyricsNow {
    sqlx::query_as(
        "SELECT has_lyrics, lyrics_source, lyrics_pipeline_version, lyrics_reference, \
         lyrics_translation_version, lyrics_translation_gender FROM videos WHERE id = ?",
    )
    .bind(id)
    .fetch_one(node.pool())
    .await
    .unwrap()
}

/// SNV serves `mtl+g35t` lyrics with ★, SK translated (v2, masculine); PP
/// has the song, no lyrics, and asks SNV. Returns SNV's JSON bytes.
async fn snv_and_pp() -> (TestNode, TestNode, i64, Vec<u8>) {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let json = snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    sqlx::query("UPDATE videos SET lyrics_reference = 1, lyrics_translation_version = 2, \
                 lyrics_translation_gender = 'm' WHERE id = ?")
        .bind(snv_id)
        .execute(snv.pool())
        .await
        .unwrap();
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    (snv, pp, id, json)
}

fn json_at(node: &TestNode) -> Option<Vec<u8>> {
    std::fs::read(node.cache().join(format!("{YT}_lyrics.json"))).ok()
}

#[tokio::test]
async fn a_peers_lyrics_are_taken_with_their_row() {
    let (_snv, pp, id, json) = snv_and_pp().await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    assert_eq!(json_at(&pp), Some(json));
    let now = lyrics_now(&pp, id).await;
    assert_eq!((now.has_lyrics, now.lyrics_source.as_deref()), (1, Some("mtl+g35t")));
    assert_eq!(now.lyrics_pipeline_version, i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION));
    assert_eq!(now.lyrics_reference, 1, "the ★ comes along");
    assert_eq!((now.lyrics_translation_version, now.lyrics_translation_gender.as_deref()), (2, Some("m")));
}

#[tokio::test]
async fn another_translation_gender_here_asks_for_a_local_retranslation() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET lyrics_translation_gender = 'f' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Done));
    let now = lyrics_now(&pp, id).await;
    assert_eq!((now.lyrics_translation_version, now.lyrics_translation_gender.as_deref()), (0, Some("f")));
}

#[tokio::test]
async fn an_operators_ask_here_is_never_answered_by_a_peer() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 1 WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await else {
        panic!("a reprocess asked here runs here")
    };
    sqlx::query("UPDATE videos SET lyrics_manual_priority = 0, lyrics_override_text = 'Moj text' WHERE id = ?")
        .bind(id)
        .execute(pp.pool())
        .await
        .unwrap();
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &lyrics_row(&pp, id).await).await else {
        panic!("an operator's text runs here")
    };
    assert_eq!(json_at(&pp), None, "nothing was fetched");
}

#[tokio::test]
async fn the_same_track_already_served_here_is_nothing_newer() {
    let (_snv, pp, id, _) = snv_and_pp().await;
    pp.give_lyrics(id, YT, "mtl+g35t").await;
    std::fs::write(pp.cache().join(format!("{YT}_lyrics.json")), b"local-marker").unwrap();
    let row = lyrics_row(&pp, id).await;
    let PeerStep::Local(Some(_guard)) = first(Some(&pp.ex), &row).await else {
        panic!("the same source at the same version runs here (the daily full-mix upgrade)")
    };
    assert_eq!(json_at(&pp), Some(b"local-marker".to_vec()), "the local file is untouched");
}

/// Review Focus 5 at the adopting end.
#[tokio::test]
async fn a_track_whose_source_differs_from_the_row_is_refused() {
    let (snv, pp, id, _) = snv_and_pp().await;
    let mut track: sp_core::lyrics::LyricsTrack =
        serde_json::from_slice(&json_at(&snv).unwrap()).unwrap();
    track.source = crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE.into();
    std::fs::write(snv.cache().join(format!("{YT}_lyrics.json")), serde_json::to_vec(&track).unwrap()).unwrap();
    sqlx::query("DELETE FROM peer_hashes").execute(snv.pool()).await.unwrap();
    snv.hash_now().await;
    let row = lyrics_row(&pp, id).await;
    assert!(matches!(first(Some(&pp.ex), &row).await, PeerStep::Deferred));
    assert_eq!(json_at(&pp), None);
    assert_eq!(lyrics_now(&pp, id).await.has_lyrics, 0);
    assert_eq!(std::fs::read_dir(pp.ex.parts_dir()).unwrap().count(), 0, "the refused part is dropped");
}
```

`crates/sp-server/src/lyrics/worker_tests_peer.rs` (hooked at the end of `lyrics/worker.rs`: `#[path = "worker_tests_peer.rs"]\n#[cfg(test)]\nmod tests_peer;`):

```rust
//! #229: the lyrics worker asks its peers first.

use super::*;
use crate::peer::rig::{SNV_KEY, TestNode};

const YT: &str = "aaaaaaaaaaa";

#[tokio::test]
async fn a_peers_lyrics_are_taken_by_the_lyrics_worker() {
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    snv.give_lyrics(snv_id, YT, "mtl+g35t").await;
    snv.hash_now().await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp.add_video(YT).await;
    pp.give_song(id, YT, "Way Maker", "Sinach").await;
    let (events, _) = broadcast::channel(16);
    let worker = LyricsWorker::new_for_test(pp.pool().clone(), pp.cache().to_path_buf(), events)
        .with_peer(pp.ex.clone());
    worker.process_next().await;
    let has: i64 = sqlx::query_scalar("SELECT has_lyrics FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pp.pool())
        .await
        .unwrap();
    assert_eq!(has, 1);
    assert!(pp.cache().join(format!("{YT}_lyrics.json")).exists());
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server peer::lyrics lyrics::worker::tests_peer` — Expected now: compile FAIL.

- [ ] **Step 3: Implement.** Append to `db/models_peer.rs` (add `use crate::peer::wire::PeerLyrics;`):

```rust
/// Row `video_id` takes a peer's lyrics row (its `{yt}_lyrics.json` is in
/// place already): the peer's source, version, alignment model and ★; its
/// translation version unless this row asks another translation gender —
/// then 0, so the local retranslate pass redoes the SK lines. (SQLite's SET
/// reads the OLD row, so the CASE sees this row's own gender.)
pub async fn adopt_lyrics(pool: &SqlitePool, video_id: i64, l: &PeerLyrics) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE videos SET has_lyrics = 1, lyrics_source = ?1, lyrics_pipeline_version = ?2, \
             lyrics_quality_score = NULL, lyrics_manual_priority = 0, lyrics_attempts = 0, \
             lyrics_next_attempt_at = NULL, \
             lyrics_processed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
             lyrics_alignment_model = ?3, lyrics_reference = ?4, \
             lyrics_translation_version = CASE WHEN lyrics_translation_gender IS NULL \
                 OR lyrics_translation_gender IS ?6 THEN ?5 ELSE 0 END, \
             lyrics_translation_gender = COALESCE(lyrics_translation_gender, ?6) \
         WHERE id = ?7",
    )
    .bind(&l.source)
    .bind(i64::from(l.pipeline_version))
    .bind(&l.alignment_model)
    .bind(i64::from(l.reference))
    .bind(i64::from(l.translation_version))
    .bind(&l.translation_gender)
    .bind(video_id)
    .execute(pool)
    .await?;
    Ok(())
}
```

`crates/sp-server/src/peer/lyrics.rs`:

```rust
//! #229: the lyrics job asks first. Never for an operator's own ask here (a
//! reprocess or "Nesedí": `lyrics_manual_priority`; a `lyrics_override_text`).
//! The peer's row must match its catalog and must not be the Live-Translate
//! track; a copy of what the row already serves (the same source at the same
//! version, e.g. the daily full-mix upgrade) is nothing newer: the job runs
//! here. The track is parsed as a typed `LyricsTrack` whose source must be the
//! row's, renamed into `{yt}_lyrics.json`, and the row takes the peer's
//! lyrics columns (`adopt_lyrics`).

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tracing::warn;

use super::Exchange;
use super::ask::{Ask, FetchPlan, PeerStep};
use super::client::PeerError;
use super::kind::{ArtifactKind, Job};
use super::wire::PeerLyrics;
use crate::dabing::subtitles::SOURCE_LIVE_TRANSLATE;
use crate::db::models::VideoLyricsRow;
use crate::db::models_peer;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Adopted {
    Track,
    NothingNewer,
}

/// The lyrics worker's hook: fetch, wait or process here.
pub async fn first(ex: Option<&Arc<Exchange>>, row: &VideoLyricsRow) -> PeerStep {
    let Some(ex) = ex else {
        return PeerStep::Local(None);
    };
    if wants_local(&ex.pool, row).await {
        return PeerStep::Local(Some(ex.announce(&row.youtube_id, Job::Lyrics)));
    }
    match ex.ask(Job::Lyrics, &row.youtube_id).await {
        Ask::Local(guard) => PeerStep::Local(Some(guard)),
        Ask::Wait { recheck, .. } => {
            defer(ex, row.id, recheck).await;
            PeerStep::Deferred
        }
        Ask::Fetch(plan) => match adopt(ex, row, &plan).await {
            Ok(Adopted::Track) => {
                ex.fetched(Job::Lyrics, &row.youtube_id, &plan.peer.name, &plan.artifacts).await;
                PeerStep::Done
            }
            Ok(Adopted::NothingNewer) => PeerStep::Local(Some(ex.announce(&row.youtube_id, Job::Lyrics))),
            Err(e) => {
                let recheck = ex.fetch_failed(Job::Lyrics, &row.youtube_id, &plan.peer.name, &e).await;
                defer(ex, row.id, recheck).await;
                PeerStep::Deferred
            }
        },
    }
}

async fn defer(ex: &Exchange, video_id: i64, wait: Duration) {
    if let Err(e) = crate::db::models::record_lyrics_wait(&ex.pool, video_id, wait).await {
        warn!(video_id, %e, "exchange: deferring the lyrics failed");
    }
}

/// An operator asked THIS node (a reprocess, a "Nesedí") or gave it the text.
async fn wants_local(pool: &SqlitePool, row: &VideoLyricsRow) -> bool {
    if row.lyrics_override_text.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        return true;
    }
    sqlx::query_scalar::<_, i64>("SELECT lyrics_manual_priority FROM videos WHERE id = ?")
        .bind(row.id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .is_some_and(|priority| priority != 0)
}

/// The peer's track into `{yt}_lyrics.json` with its row, or nothing newer.
pub(crate) async fn adopt(ex: &Exchange, row: &VideoLyricsRow, plan: &FetchPlan) -> Result<Adopted, PeerError> {
    let artifact = plan.artifact(ArtifactKind::Lyrics)?;
    let video = ex.client.video(&plan.peer, &row.youtube_id).await?;
    let lyrics = video
        .lyrics
        .ok_or_else(|| PeerError::BadResponse("the peer's row has no lyrics".into()))?;
    if lyrics.pipeline_version != artifact.version || lyrics.source == SOURCE_LIVE_TRANSLATE {
        return Err(PeerError::BadResponse("the peer's lyrics row does not match its catalog".into()));
    }
    if serves_the_same(&ex.pool, row.id, &lyrics).await? {
        return Ok(Adopted::NothingNewer);
    }
    let part = ex.fetch(&plan.peer, artifact).await?;
    let bytes = tokio::fs::read(&part).await?;
    let source = match serde_json::from_slice::<sp_core::lyrics::LyricsTrack>(&bytes) {
        Ok(track) => track.source,
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(PeerError::BadResponse(format!(
                "not a lyrics track (line {}, column {})",
                e.line(),
                e.column()
            )));
        }
    };
    if source != lyrics.source {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(PeerError::BadResponse(format!(
            "the track's source {source:?} is not the row's {:?}",
            lyrics.source
        )));
    }
    tokio::fs::rename(&part, ex.cache_dir.join(format!("{}_lyrics.json", row.youtube_id))).await?;
    models_peer::adopt_lyrics(&ex.pool, row.id, &lyrics).await?;
    Ok(Adopted::Track)
}

/// The row already serves this very track: the same source, the same version.
async fn serves_the_same(pool: &SqlitePool, video_id: i64, lyrics: &PeerLyrics) -> Result<bool, sqlx::Error> {
    let row: Option<(i64, Option<String>, i64)> =
        sqlx::query_as("SELECT has_lyrics, lyrics_source, lyrics_pipeline_version FROM videos WHERE id = ?")
            .bind(video_id)
            .fetch_optional(pool)
            .await?;
    Ok(matches!(
        row,
        Some((1, Some(source), version))
            if source == lyrics.source && version == i64::from(lyrics.pipeline_version)
    ))
}

#[cfg(test)]
#[path = "lyrics_tests.rs"]
mod tests;
```

In `lyrics/worker.rs`: field `pub(crate) peer: Option<Arc<crate::peer::Exchange>>,` (doc: "#229: asked before each song; `None` in tests that do not need it"), `peer: None,` in BOTH `new()` and `new_for_test()`, a `pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self { self.peer = Some(peer); self }` next to `with_wall_handles`, and in `process_next` right after the `"worker: processing {} ({} - {})"` INFO:

```rust
        // #229: ask the peers first (`peer::lyrics`): a peer's lyrics are
        // taken, a peer's running job waited for; else process here,
        // announced until this tick ends.
        let _announced = match crate::peer::lyrics::first(self.peer.as_ref(), &row).await {
            crate::peer::PeerStep::Local(guard) => guard,
            crate::peer::PeerStep::Done | crate::peer::PeerStep::Deferred => return,
        };
```

`cargo fmt --all`; `wc -l crates/sp-server/src/lyrics/worker.rs` (≈ 920).

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server peer::lyrics lyrics::` — Expected: PASS.
- [ ] **Step 5: Commit** — `test(#229): the lyrics job asks first — adopt, gender, operator, nothing newer, source check` then `feat(#229): LyricsWorker asks its peers first; peer::lyrics + adopt_lyrics`.

### Task 9.3: `peer::repair` + the metadata repair

**Files:**
- Create: `crates/sp-server/src/peer/repair.rs`, `crates/sp-server/src/reprocess/tests_peer.rs`
- Modify: `crates/sp-server/src/peer/mod.rs` (`pub mod repair;`), `crates/sp-server/src/reprocess/mod.rs`

**Interfaces:**
- Consumes: `NodeConfig`, `PeerClient::{catalog, video}`, `kind::acceptable`, `peer::download::adopted_title`, `models_peer::record_fetch`, `hash::sha256_hex`.
- Produces: `peer::repair::peer_title(&Exchange, youtube_id: &str) -> Option<DownloadTitle>`; `ReprocessWorker::with_peer(self, Arc<Exchange>) -> Self`; `ReprocessWorker::apply_title(&self, &ReprocessRow, song, artist, source: &str) -> Result<ReprocessOutcome, anyhow::Error>` (the existing locked rename + record + UPDATE, moved unchanged).

- [ ] **Step 1: Write the failing tests** `crates/sp-server/src/reprocess/tests_peer.rs` (hook at the end of `reprocess/mod.rs`: `#[cfg(test)]\n#[path = "tests_peer.rs"]\nmod tests_peer;`)

```rust
//! #229: the metadata repair takes a peer's title before it asks the providers.

use super::*;
use crate::downloader::cache::{audio_filename, video_filename};
use crate::peer::rig::{SNV_KEY, TestNode, bytes, counting_chain};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const YT: &str = "aaaaaaaaaaa";

/// PP holds the song under a parser title (`_gf` files, in the repair queue).
async fn pp_with_a_parser_title(pp: &TestNode) -> i64 {
    let id = pp.add_video(YT).await;
    let video = pp.cache().join(video_filename("Guess", "Unknown", YT, true));
    let audio = pp.cache().join(audio_filename("Guess", "Unknown", YT, true));
    std::fs::write(&video, bytes(2_000, 1)).unwrap();
    std::fs::write(&audio, bytes(3_000, 2)).unwrap();
    crate::db::models::mark_video_processed_pair(
        pp.pool(), id, "Guess", "Unknown", "regex", true,
        &video.to_string_lossy(), &audio.to_string_lossy(),
    )
    .await
    .unwrap();
    id
}

async fn title(node: &TestNode, id: i64) -> (String, String, Option<String>, i64) {
    sqlx::query_as("SELECT song, artist, metadata_source, gemini_failed FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(node.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_peers_provider_title_repairs_the_row_with_no_provider_call() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Way Maker", "Sinach").await;
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp_with_a_parser_title(&pp).await;
    let (chain, calls) = counting_chain();
    let mut worker = ReprocessWorker::new(pp.pool().clone(), Arc::new(chain), pp.cache().to_path_buf())
        .with_peer(pp.ex.clone());
    worker.process_all().await.unwrap();
    assert_eq!(title(&pp, id).await, ("Way Maker".into(), "Sinach".into(), Some("gemini".into()), 0));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(pp.cache().join(audio_filename("Way Maker", "Sinach", YT, false)).exists(), "renamed after the title");
    assert!(!pp.cache().join(audio_filename("Guess", "Unknown", YT, true)).exists());
    let record = crate::db::models_peer::fetch_record(pp.pool(), YT, "metadata").await.unwrap();
    assert_eq!(record.unwrap().0, "snv");
}

#[tokio::test]
async fn a_peers_parser_title_leaves_the_repair_to_the_providers() {
    let snv = TestNode::start("snv", Some(SNV_KEY)).await;
    let snv_id = snv.add_video(YT).await;
    snv.give_song(snv_id, YT, "Guess", "Unknown").await;
    sqlx::query("UPDATE videos SET gemini_failed = 1, metadata_source = 'regex' WHERE id = ?")
        .bind(snv_id)
        .execute(snv.pool())
        .await
        .unwrap();
    let pp = TestNode::start("pp", None).await;
    pp.set_peers(&[snv.as_peer(SNV_KEY)]).await;
    let id = pp_with_a_parser_title(&pp).await;
    let (chain, calls) = counting_chain();
    let mut worker = ReprocessWorker::new(pp.pool().clone(), Arc::new(chain), pp.cache().to_path_buf())
        .with_peer(pp.ex.clone());
    worker.process_all().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(title(&pp, id).await.0, "Chain Song");
}
```

- [ ] **Step 2: Run (CI only):** `cargo test -p sp-server reprocess::` — Expected now: compile FAIL (`with_peer`).

- [ ] **Step 3: Implement** `crates/sp-server/src/peer/repair.rs`

```rust
//! #229: the metadata repair asks first. A video in this node's repair queue
//! (a parser title) takes a peer's provider or operator title when a peer's
//! catalog lists one (metadata version ≥ 1): no provider is called here. Else
//! this node's providers repair it as before.

use tracing::{info, warn};

use super::Exchange;
use super::config::NodeConfig;
use super::download::adopted_title;
use super::hash::sha256_hex;
use super::kind::{ArtifactKind, acceptable};
use super::wire::now_ms;
use crate::db::models_peer;
use crate::metadata::manual::DownloadTitle;

/// A peer's title for `youtube_id`, or `None` (ask this node's providers).
pub async fn peer_title(ex: &Exchange, youtube_id: &str) -> Option<DownloadTitle> {
    let cfg = NodeConfig::load(&ex.pool).await.ok()?;
    if !cfg.asking() {
        return None;
    }
    for peer in &cfg.peers {
        let Ok(catalog) = ex.client.catalog(peer).await else {
            continue;
        };
        let listed = catalog.artifacts.iter().any(|a| {
            a.youtube_id == youtube_id && a.kind == ArtifactKind::Metadata && acceptable(a.kind, a.version)
        });
        if !listed {
            continue;
        }
        let Ok(video) = ex.client.video(peer, youtube_id).await else {
            continue;
        };
        let Some(title) = adopted_title(&video.metadata) else {
            continue;
        };
        let sha = sha256_hex(&video.metadata.to_bytes());
        let kind = ArtifactKind::Metadata.as_str();
        let version = video.metadata.version();
        if let Err(e) = models_peer::record_fetch(&ex.pool, youtube_id, kind, &peer.name, version, &sha, now_ms()).await {
            warn!(youtube_id, %e, "exchange: recording a fetch failed");
        }
        info!(youtube_id, source = %format!("peer:{}", peer.name), "exchange: the metadata repair takes a peer's title");
        return Some(title);
    }
    None
}
```

In `reprocess/mod.rs`:
- field `peer: Option<Arc<crate::peer::Exchange>>,` in `ReprocessWorker` (doc: "#229: asked before the providers; `None` in tests that do not need it"), `peer: None,` in `new()`, and `pub fn with_peer(mut self, peer: Arc<crate::peer::Exchange>) -> Self { self.peer = Some(peer); self }`;
- at the very top of `reprocess_one` (before the cooldown checks — a peer's title costs no provider call):

```rust
        // #229: a peer's provider or operator title repairs the row with no
        // provider call (`peer::repair`).
        let peer = self.peer.clone();
        if let Some(ex) = peer.as_deref()
            && let Some(title) = crate::peer::repair::peer_title(ex, &row.youtube_id).await
        {
            self.per_video_backoff.remove(&row.id);
            return self.apply_title(row, &title.song, &title.artist, title.source).await;
        }
```

- move everything from `let _files = crate::downloader::cache::SONG_FILES.lock().await;` to the final `Ok(ReprocessOutcome::Success)` into a new method, unchanged except `meta.song` / `meta.artist` / `meta.source.as_str()` become the parameters:

```rust
    /// #136: the row re-checked against the queue under `SONG_FILES`, its set
    /// renamed after `song` / `artist` (no `_gf`), recorded on every row that
    /// recorded it, the title written with `source`. The ONE repair write.
    async fn apply_title(
        &self,
        row: &ReprocessRow,
        song: &str,
        artist: &str,
        source: &str,
    ) -> Result<ReprocessOutcome, anyhow::Error> {
        let _files = crate::downloader::cache::SONG_FILES.lock().await;
        // … the existing block, verbatim, with `meta.song` → `song`,
        // `meta.artist` → `artist`, `meta.source.as_str()` → `source` …
        Ok(ReprocessOutcome::Success)
    }
```

and the provider path in `reprocess_one` ends with `self.per_video_backoff.remove(&row.id);` then `self.apply_title(row, &meta.song, &meta.artist, meta.source.as_str()).await`. The existing `reprocess/tests_files.rs` / `tests_chain.rs` tests pin that the moved block is unchanged.

- [ ] **Step 4: Run (CI only):** `cargo test -p sp-server reprocess::` — Expected: PASS (old + new).
- [ ] **Step 5: Commit** — `test(#229): the metadata repair takes a peer's title first` then `feat(#229): ReprocessWorker asks its peers first; apply_title holds the one repair write`.

### Task 9.4: wire the three workers, docs, push, PP's workers on

**Files:**
- Modify: `crates/sp-server/src/lib.rs`, `.claude/rules/peer-exchange.md`

- [ ] **Step 1: Wire.** In `lib.rs`, next to the other `*_shutdown` clones before the tools task: `let stem_exchange = exchange.clone();` and `let lyrics_exchange = exchange.clone();`; chain `.with_peer(stem_exchange)` onto `crate::stems::StemWorker::new(…)`, `.with_peer(lyrics_exchange)` onto `lyrics::LyricsWorker::new(…)`; at "8. Reprocess worker": `reprocess::ReprocessWorker::new(pool.clone(), metadata_chain, config.cache_dir.clone()).with_peer(exchange.clone())`. `cargo fmt --all`; `wc -l crates/sp-server/src/lib.rs` ≤ 1000.

- [ ] **Step 2: Append to `.claude/rules/peer-exchange.md`**

```markdown
## Stems, lyrics, metadata repair (`peer::{stems, lyrics, repair}`)

- Stems: the hook sits AFTER the venv gate and the terminal skip (the gate
  order is pinned by the stem worker's tests): a node fetches stems once it
  could separate them. The parts are renamed under `stem_paths(<the audio the
  row records AFTER the transfer>)`, read under `SONG_FILES`, then
  `mark_stems_done`; a wait/failed fetch → `defer_stems` (no attempt).
- Lyrics: never for an operator's ask here (`lyrics_manual_priority`, a
  `lyrics_override_text`); the peer's `/videos` row must match its catalog
  and must not be `gemini-live-translate`; the same source at the same
  version as the row already serves = nothing newer → runs here (the daily
  full-mix upgrade stays local); the JSON is parsed as `LyricsTrack` and its
  `source` must equal the row's; `adopt_lyrics` takes the ★ and the
  translation version (0 if this row asks another gender).
- Repair: `peer_title` before the providers (and before their cooldown);
  `apply_title` is the ONE repair write (#136 locked re-check + rename).
- Enabling a node: `lyrics_worker_enabled` / `stem_worker_enabled` = off
  means no fetch either (the kill switch stops the whole tick).
```

- [ ] **Step 3: Commit** — `feat(#229): stems, lyrics and repair workers ask their peers (lib.rs)` + `docs(#229): stems/lyrics/repair ask-first rules`; **push the lane**, monitor CI. SNV has no peers: every job runs as before (pinned by the `with_no_peers…` tests).

- [ ] **Step 4: MAIN SESSION OPS — after the release that carries Lanes 7–9 reaches PP** (deploy-pp green):
  1. PATCH PP `{"lyrics_worker_enabled":"true","stem_worker_enabled":"true"}` (phase 0 had them off).
  2. Watch PP's log for `exchange:` lines and `GET /api/v1/exchange/status` (`peers[0].last_read.ok`, `jobs`).
  3. Live check: add one new video to a playlist both sites sync; after SNV has processed it, PP must log `exchange: done with a peer's copy` for its download, stems and lyrics, and run no separation for it (`stem worker: separating` absent for that id). Record the evidence on #229.

---

## Self-review (run against the spec)

**Spec coverage — every requirement → its task:**

| Spec item | Where |
|---|---|
| `node_name` + peer list (base URL, Access service token, peer key) | Lane 1 (Tasks 1.1–1.4) |
| Keys only through the secret channel, never logged | Task 1.2 (`Debug`, error texts), 1.3 (masked GET, mask-preserving PATCH), ops steps in Lanes 4–6 |
| PP → SNV through `sp.newlevel.media` with an Access service token | Task 5.1 (headers, no redirects), Lane 5 ops (token + non-identity policy) |
| Peer key header on every request; Access in front on the public path | Task 4.1 |
| `GET /catalog?since=` — artifacts + running jobs | Tasks 3.3 (build), 4.1 (route) |
| `GET /videos/{id}` — metadata, `metadata_source`, lyrics source + version | Task 4.2 |
| `GET /artifact/{id}/{kind}` with Range | Tasks 4.3 (server), 5.2 (client resume) |
| Kinds video / audio / stem_vocals / stem_instrumental / lyrics / metadata (dub later) | Task 2.1 (+ `Unknown` for a later `dub`) |
| sha256 computed + cached, not per request | Tasks 3.1, 3.4 |
| Ask first: has it → fetch, sha check, name from the OWN row, done with `peer:<node>` | Lanes 7–9 (`decide`, `ask`, adopters; provenance `peer_fetches`) |
| Running it → defer with backoff, bounded ~2 h, then local | Tasks 7.1–7.3 + each job's defer |
| Nobody → process locally and announce | `Ask::Local(JobGuard)` (Task 7.3), pinned in Tasks 8.1, 8.2, 9.1 |
| Each heavy job: download, normalize, metadata, lyrics, stems | Lane 8 (download = download+normalize+metadata), Lane 9 (stems, lyrics, metadata repair) |
| Playlists synced per node | unchanged (no code) |
| Low priority, resume, one transfer per peer, #230 pauses | Task 4.3 (upload cap), 5.2 (Range, slot, pause), `peer_transfers_paused` |
| Two-node rig: fetch / wait / process; sha mismatch; Range resume; key refusals | rig (3.3); 8.1, 7.3, 9.x; 5.2 + 8.1; 4.3 + 5.2; 4.1 + 5.1 |
| Live post-deploy check: PP reads SNV's catalog through Cloudflare | Tasks 5.3 (probe), 6.3 (gate) |
| Runner `resolume-pp`; PP gets main releases only | Tasks 6.1, 6.2 |
| PP post-deploy: version, SP-program receiver, playback + MAX, facade + OBS manuál | Task 6.3 |
| PP's settings never overwritten by a deploy | Task 6.2 (no DB step) + 6.3 (`node_name pp`, peer `snv` asserted) |
| "produkcia beží" at PP stops CI and every touch | Task 6.4 rules (cancel + disable `deploy-pp.yml`) |
| SNV keeps today's behaviour (no peers) | Global Constraints + the `with_no_…` tests in 7.3, 8.1, 8.2, 9.1 |

**Placeholder scan:** no TBD/TODO; the one "verbatim move" (Task 9.3 `apply_title`) names the exact lines and the three substitutions.

**Type consistency checked:** `Exchange::new(SqlitePool, PathBuf) -> Arc<Exchange>` is stable across lanes (board and client are built inside); `PeerStep::{Done, Deferred, Local(Option<JobGuard>)}` is matched the same way in all four hooks; `PeerClient::fetch(&PeerConfig, &Artifact, &Path)` vs `Exchange::fetch(&PeerConfig, &Artifact)`; `models_peer::{waited, start_wait, end_wait}` take `job: &str` (`Job::as_str`); `record_fetch(pool, youtube_id, kind, node, version: u32, sha, at_ms)` everywhere; `rig::counting_chain()` is defined in Task 8.1 and reused in 9.3.

**Review Focus:** five lines, each with its test in the owning task (1→5.1, 2→1.3, 3→2.2+5.1, 4→9.1, 5→3.3+4.3+9.2).

## Spec gaps found (for the main session / the owner)

1. **"Mark the job done with `source = peer:<node>`"** — the row's `metadata_source` / `lyrics_source` drive `REPAIR_QUEUE_WHERE`, the ★ wall marker and `alignment_model_for_source`. The plan keeps the PEER's real values on the row and records the origin in `peer_fetches` (+ an INFO line). No dashboard shows it yet.
2. **Running jobs only.** Both sites sync the same playlists, so PP will often see a new song before SNV has STARTED it (no artifact, no running job) and process it locally. Announcing SNV's *queued* rows too (a `jobs[].state: running|queued`) would close that, bounded by the same 2 h. Needs the owner's call; not in this plan.
3. **An unreadable peer** (outage, refused key or token) is treated as "wait" (≤ 2 h), not "nobody has it" — the spec names only three cases; the plan picks the thrifty reading of "always check first".
4. **`since=`** filters by when the serving node LISTED (hashed) a file; metadata entries carry no time and are always listed; the phase-1 client reads the whole catalog (cached 60 s, ~1 MB) and does not use `since`.
5. **#230 pause** is not built here; `peer_transfers_paused` (pauses transfers both ways and the hasher) is the lever #230 can flip. A transfer already running finishes (≤ ~2 min at the cap).
6. **SNV's uplink capacity is unknown**: `peer_serve_max_mbps` defaults to 20 Mbit/s — read on site and set.
7. **"An SP-program receiver" at PP**: which PP consumer takes `SP-program` over NDI must be confirmed on site; the PP gate fails without one.
8. **A DB copied SNV → PP after SNV got its exchange settings** carries `node_name=snv` and SNV's key. Phase 0 step 4 must also set PP's `node_name=pp` and clear `peer_api_key` (now in the rules and Lane 6 ops).
9. **Stem fetching needs the lyrics venv** at the node (the stem worker's venv gate comes first, as its tests pin), and the `*_worker_enabled` kill switches stop fetching too: PP's workers are switched on only after the release that carries Lanes 7–9.
10. **Existing secrets** (`gemini_api_key`, OBS / remote passwords, Genius token) are returned in clear by `GET /api/v1/settings` today — out of scope here; a candidate ticket (the masking helpers of Task 1.2 can take them).
11. **`deploy-pp.yml` fires only once it is on `main`** (`workflow_run`), so PP's first CI deploy is the release after Lane 6; until then PP runs its phase-0 build.
12. **Cloudflare plan terms** on large media through the tunnel: after phase 0 only new songs move (hundreds of MB a week), but worth a look.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-10-06-pp-node-exchange.md`. Execution: the main session dispatches the nine lanes serially (one worker per lane, one push per lane, CI to terminal state before the next), doing the **MAIN SESSION OPS** steps itself at the points marked.
