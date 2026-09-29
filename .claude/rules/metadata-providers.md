---
paths:
  - "crates/sp-server/src/metadata/**"
  - "crates/sp-server/src/reprocess/**"
  - "crates/sp-server/src/api/metadata*.rs"
  - "crates/sp-server/src/gemini_api*.rs"
  - "crates/sp-server/src/lyrics/g35t_client.rs"
  - "e2e/post-deploy-metadata.spec.ts"
  - "e2e/post-deploy-flac.spec.ts"
---

# Metadata providers: ONE chain, the Gemini key LIST, a live gate (#136)

On 29.9.2026 ~110 videos played on the wall and the Presenter under their raw
YouTube title ("Stand On Your Promise by The Emerging Sound (feat. …)") and CI
stayed green. Three defects stacked (design record: #136 comment 5887954703):
the Gemini provider sent the whole comma-separated `gemini_api_key` as ONE
`x-goog-api-key` (Google refused it in ~40 ms), the reprocess worker had its
own list with Gemini alone (so it could never repair a row while Claude
answered correctly), and nothing ever ran the real providers.

## The one chain

- `metadata::provider_chain(ai_client, gemini_csv, gemini_model)` =
  [Claude (CLIProxyAPI), Gemini]. `lib.rs` builds it ONCE; the same `Arc` goes
  to `DownloadWorker`, `ReprocessWorker` and `AppState.metadata_chain` (the
  probe + `status.metadata`). Both workers take `Arc<ProviderChain>`; an
  ad-hoc provider list must not be wired in again (`ProviderChain::new` is
  `pub` for tests and composition, so this is a rule, not a type guarantee).
  Never build a second provider list anywhere.
- Gemini is in the chain even with no key: it then fails at once with "no API
  key configured", visible on the status, instead of silently vanishing.
- Every chain member is wrapped (`chain::Recorded`): each call's outcome +
  latency goes to `MetadataHealth` and to the log (INFO answered / WARN
  failed). `get_metadata` therefore logs a provider failure only at DEBUG.

## Gemini on the key list

- ONE Gemini key-list contract: `crate::gemini_api` (the API root,
  `gemini_keys_from_setting`, `key_verdict`, `send_on_key` = one key's
  request + its same-key 5xx retries after `RETRY_BACKOFFS` 2/4/8/16 s). The
  lyrics transcription (`lyrics::g35t_client`) and `metadata::gemini` rotate
  over every key and both send through `send_on_key` / judge by
  `key_verdict` — never a local copy of the rules or the loop (review rounds
  1-2 found two diverging copies). The dub worker (`dabing::worker`) takes
  the FIRST key only (no rotation).
- Metadata reads `gemini_api_key` / `gemini_model` ONCE at startup
  (`lib.rs`): a key change needs a SongPlayer restart (lyrics reads the
  setting per song).
- `GeminiProvider::new(keys, model)` takes the SPLIT list — always
  `gemini_api::gemini_keys_from_setting(csv)`, never the raw setting. One key
  per request; a 429 or a key refusal (403, a 400 whose body names the API
  key) → next key; a 5xx → the SAME key again after each pause; anything else
  stops (it would fail the same on every key). All keys failed →
  `RateLimited(detail)` when any was 429 (the reprocess worker's cooldown),
  else `ApiError(detail)`. The clean-up pass uses the key that answered and
  never retries (it is cosmetic).
- An error text / log line carries the key INDEX (`key 2 of 5`), the status
  and a ≤ 200-char body excerpt redacted (longest key first) BEFORE the cut —
  never a key. A recorded / probed provider error is cut to 300 chars
  (`health::bounded_error`): a Claude error carries the proxy's whole reply.
- Request tools: `"tools": [{"google_search": {}}]` is still the grounding
  tool for `generateContent` on Gemini 3 Pro (ai.google.dev, checked
  29.9.2026). The answer text is every non-thought part joined.

## Visibility + the gate

- `GET /api/v1/status.metadata {failed_videos, providers: [{name,
  last_ok_at_ms, last_error}]}`. `failed_videos` counts
  `health::REPAIR_QUEUE_WHERE` — the SAME predicate the reprocess worker
  selects by (one constant), `null` (never a false 0) if unreadable.
- `POST /api/v1/metadata/probe {youtube_id, title}` runs EACH provider on its
  own (concurrently, each bounded by `PROBE_TIMEOUT` = 180 s, below the spec's
  220 s so a hung provider fails the gate WITH its name), returns each
  outcome, writes nothing. 400 on an empty id / title; a missing field is
  axum's 422.
- `e2e/post-deploy-metadata.spec.ts` probes `gq-4FVRr_ow` with its YouTube
  title: the chain must be [claude, gemini] and every provider must answer
  "Stand On Your Promise" / an artist containing "Emerging Sound" (case-
  insensitive). A new provider, model or key-format change must keep this
  gate green on the box, not just the unit tests.
- The reprocess worker WARNs a failed row with EVERY provider's error, in
  chain order; the per-video backoff makes that one WARN per stage.
- Never gate provider health on stored rows ("at least one video has provider
  metadata"): `post-deploy-flac.spec.ts` had exactly that check and it PASSED
  on 29.9.2026 with 108 rows parser-titled (run 36553367664). It now checks
  only that stored metadata is clean; provider health is the live probe.
  `failed_videos` is logged, not asserted to 0 — right after a deploy the
  reprocess worker is still draining it.

## Tests (no live API in CI)

- `GeminiProvider::with_api_root` / `chain::provider_chain_at` point Gemini at
  a wiremock server; `metadata::test_support` has the fixed video, the
  Claude (`/v1/chat/completions`) and Gemini answer shapes, and
  `received_keys` (the header of every request, in order).
- wiremock's `header(k, v)` matcher SPLITS a request's header value on
  commas: a request carrying `k1,k2` does NOT match `header("x-goog-api-key",
  "k1")`. Assert the raw header through `received_keys` when the test is
  about what was sent.
- A Claude mock that fails must answer a 4xx: `AiClient` retries 429/5xx
  with 1 s / 2 s sleeps. A Gemini 5xx test sets millisecond pauses with
  `GeminiProvider::with_retry_backoffs` (test-only).
- `.cargo/mutants.toml` no longer excludes `reprocess/` (only its timer loop
  `ReprocessWorker::run`): a changed line there needs its killing test.
- The #136 RED kept the old behaviour in two named spots (the key list joined
  into one header; `try_providers` keeping only the last error) — the same
  pattern works for a later change here.
