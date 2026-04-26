# Resolume Push Hardening — Design

**Date:** 2026-04-26
**Trigger:** 2026-04-26 live event silently failed to push lyrics to Resolume for ~5 hours. Manual SongPlayer restart fixed it. Diagnostic analysis revealed three independent latent fragilities, all converging into the failure: invisible state changes, no auto-recovery on Resolume reconnect, and stale cached clip mapping.

**Goal:** Move the Resolume push chain from "works in the lab, silently dies in production" to "fails loud, self-heals, has visible state."

**Non-goal:** Make Resolume itself more reliable. We assume Arena can crash, hang, reload compositions, and switch projects. The system around it must adapt.

## Background

### What happened

Timeline of 2026-04-26 live event:
- **05:58 UTC** — SongPlayer 0.23.0 deployed and started.
- **05:59 UTC** — Lyrics worker logged 13 occurrences of `no Resolume subtitle clips found, skipping clear_subtitles` over 5 seconds. All at `debug!` level. Then silence.
- **09:07–10:10 UTC** — Arena unreachable. Logs filled with `clip mapping refresh failed` (debug level). 18 entries over ~1 hour, then silence again.
- **11:00:41 UTC** — Arena restarted. Last `clip mapping refresh failed`, then quiet.
- **11:04 UTC onward** — `updated Resolume clip mapping host=127.0.0.1 tokens=12 clips=22` succeeds repeatedly (info level — only success-after-change is logged at info).
- **Live event in progress** — operator notices wall has no lyrics. Restarts SongPlayer. Lyrics resume.

### Three latent fragilities

1. **Invisible state changes.** The lyrics-load step, the ShowSubtitles dispatch, the clip-cache miss-and-skip, all logged at `debug!` — filtered in production. We had zero visibility into the failure for 5 hours.

2. **No re-emit on recovery.** Even after the Resolume cache rebuilt cleanly post-Arena-restart, the engine never re-emitted the current lyric line. The wall stayed empty until the next line tick, which never happened (lyrics were never loaded for that song in the first place).

3. **Cached clip map survives indefinitely.** When Arena was unreachable for hours, then came back with a potentially-different composition, the old cached state could persist if the new composition happened to have overlapping tokens. There is no eviction-on-extended-failure today.

## Architecture

Three tiers, each independently shippable. T4 (CI drill) is deferred to a later spec.

### Tier 1 — Visibility

**Log promotion.** Every load + push + skip in the Resolume chain emits `info!` with structured fields. Mirrors what we did for the title chain in 2026-04-25's PR #53.

| Site | Today | After |
|---|---|---|
| `playback/lyrics_loader.rs` — load success | silent | `info! lyrics: loaded video_id=X lines=N source=Y pipeline_version=Z` |
| `playback/lyrics_loader.rs` — load failure | silent (Result discarded) | `warn! lyrics: failed to load video_id=X reason=Y` |
| `playback/mod.rs` — ShowSubtitles dispatch | silent | `info! ShowSubtitles dispatched playlist_id=X line_idx=Y text="..."` |
| `resolume/handlers.rs` — set_subtitles success | `debug!` | `info! set subtitle text on all clips token="#sp-subs" count=N` |
| `resolume/handlers.rs` — clips_for_subs cache miss | `debug!` | `warn! clips_for_subs cache miss — skipping push token=X` (warn because this means the wall is dark) |
| `resolume/driver.rs` — refresh failure | `debug!` | `warn!` after second consecutive failure (first failure stays debug — transient blips not noteworthy) |

**Dashboard health card.** New `<ResolumeHealthCard>` Leptos component reading a new `GET /api/v1/resolume/health` endpoint:

```json
{
  "host": "127.0.0.1",
  "last_refresh_ts": "2026-04-26T11:04:39Z",
  "last_refresh_ok": true,
  "consecutive_failures": 0,
  "circuit_breaker_open": false,
  "clips_by_token": {
    "#sp-title": 5,
    "#sp-subs": 4,
    "#sp-subs-next": 0,
    "#sp-subssk": 4
  }
}
```

Card renders:
- Green dot if `last_refresh_ok && circuit_breaker_open=false`
- Yellow dot if `consecutive_failures >= 1 || any clips_by_token = 0`
- Red dot if `circuit_breaker_open = true`

Operator sees in 2 seconds whether the wall pipeline is alive.

### Tier 2 — Auto-recovery

**Reconnect detection in `driver.rs`.** Track `consecutive_refresh_failures: u32`. State machine:

```
        successful refresh (count = 0)
          │
          ▼
       Healthy ──── refresh fails ──→ Failing(count=1)
          ▲                              │
          │                              │ refresh fails
          │                              ▼
          │                        Failing(count=2..N)
          │  refresh succeeds            │
          └──────────────────────────────┤  if count >= 1, fire RecoveryEvent
                                         │
                                         │ T+30s of failures
                                         ▼
                                    CircuitOpen — cache evicted
                                         │
                                         │ first refresh succeeds
                                         ▼
                                    fire RecoveryEvent, return to Healthy
```

`RecoveryEvent` is broadcast on a `tokio::sync::broadcast` channel.

**Engine re-emit handler in `playback/mod.rs`.** Engine subscribes to `RecoveryEvent`. On receipt, for every `playlist_id` whose pipeline is in `PlayState::Playing { video_id }` AND `scene_active = true` AND has a loaded `lyrics_state`:

1. Re-emit `ResolumeCommand::ShowTitle { song, artist }` (using the existing `title::push_title` helper)
2. Re-emit `ResolumeCommand::ShowSubtitles { ... }` for `lyrics_state.current_line_idx` (using existing line-emit helper, factored out if needed)

Idempotent — Resolume A/B title crossfade no-ops on same text; subtitles use direct text-set which is also a no-op visually if same text. Cost on recovery: ~2 HTTP bursts per active playlist. Acceptable.

**Lyrics-state reload guarantee.** On every `PipelineEvent::Started`, the engine **unconditionally** drops `pp.lyrics_state` and rebuilds it from disk via `lyrics_loader`. Audit current code; if the path already does this, add a regression test pinning the behavior. If it doesn't, fix it. **Without this guarantee, a failed lyrics load on the very first song of a session leaves `lyrics_state = None` for that song forever.**

### Tier 3 — Fail-loud

**Clip-presence assertion.** On every successful refresh in `driver.rs`, compare new token-counts against last-known. Emit `warn!` when:

- A token's count drops to 0 from non-zero (clips disappeared)
- A token's count was non-zero last refresh and is now 0 (regression — operator action like changing composition)
- At first refresh after boot, any expected token (`#sp-title`, `#sp-subs`, `#sp-subs-next`, `#sp-subssk`) has count 0 (composition is incomplete)

Example log line:
```
WARN composition token regressed: #sp-subs-next was=4 now=0
```

**Circuit breaker in `driver.rs`.** When `consecutive_refresh_failures` × `refresh_interval (10s)` ≥ 30s, evict the cached clip mapping entirely (`clip_mapping = HashMap::new()`). Set `circuit_breaker_open = true`. On the next successful refresh, rebuild from scratch, set `circuit_breaker_open = false`, fire `RecoveryEvent`.

This solves the "Arena reloaded a different composition under the cache" failure mode that today silently leaks stale mappings.

## Data flow — Arena restart drill (worked example)

```
T+0      Operator kills Arena.exe
T+10s    driver.rs: refresh #1 fails → consecutive_failures = 1, debug log
T+20s    driver.rs: refresh #2 fails → consecutive_failures = 2, WARN log
T+30s    driver.rs: 30s threshold reached → evict cache, set circuit_breaker_open = true, WARN log
T+40s+   driver.rs: refresh keeps failing, no further state change
...
T+300s   Operator restarts Arena
T+310s   driver.rs: refresh succeeds → rebuild clip map, fire RecoveryEvent, set circuit_breaker_open = false, INFO log
T+310s   engine: receives RecoveryEvent → for sp-fast (Playing + scene_active), re-emit ShowTitle + ShowSubtitles
T+311s   handlers.rs: set title text on all clips (info! visible)
T+311s   handlers.rs: set subtitle text on all clips (info! visible)
T+312s   wall: title + lyrics back, NO operator intervention required
T+312s   dashboard: ResolumeHealthCard goes green, last_refresh_ts updates
```

## API surface change

**New endpoint:** `GET /api/v1/resolume/health`

**Response shape:** see Tier 1 dashboard health card.

**Implementation:** read from a `ResolumeHealthSnapshot` struct that the `HostDriver` updates atomically on each refresh. Endpoint is read-only, no controls.

**Backward compatibility:** new endpoint, no breaking changes. Existing `/api/v1/resolume/hosts` and friends untouched.

## Files touched

| File | Purpose | New / Modified |
|---|---|---|
| `crates/sp-server/src/resolume/driver.rs` | Reconnect counter, circuit breaker, RecoveryEvent emit, health snapshot | Modified |
| `crates/sp-server/src/resolume/handlers.rs` | Log promotion (clips_for_subs miss → warn) | Modified |
| `crates/sp-server/src/resolume/mod.rs` | Export RecoveryEvent broadcast handle, expose health snapshot | Modified |
| `crates/sp-server/src/playback/lyrics_loader.rs` | Info on success, warn on failure (audit current loader) | Modified |
| `crates/sp-server/src/playback/mod.rs` | Subscribe to RecoveryEvent, re-emit current line+title for active pipelines | Modified |
| `crates/sp-server/src/api/routes.rs` | New `GET /api/v1/resolume/health` route | Modified |
| `crates/sp-server/src/api/mod.rs` | Wire route | Modified |
| `sp-ui/src/components/resolume_health.rs` | New Leptos component | New |
| `sp-ui/src/pages/dashboard.rs` (or equivalent) | Add ResolumeHealthCard to dashboard | Modified |

Estimated diff: ~400 lines added across 8 files. No new crates, no migrations.

## Verification

### Unit / integration

- `driver.rs` — `wiremock` Resolume server that goes silent for 30s; assert circuit_breaker_open transitions, then returns clips and asserts RecoveryEvent fires once.
- `driver.rs` — single transient failure (one refresh fails, next succeeds) does NOT fire RecoveryEvent.
- `handlers.rs` — clip-skip path with empty cache emits `warn!` (use a `tracing-test` capture).
- `playback/mod.rs` — RecoveryEvent received with active Playing pipeline triggers ShowTitle + ShowSubtitles re-emit on the resolume_tx mock channel.
- `lyrics_loader.rs` — load failure produces `warn!` and returns None; load success produces `info!` with line count.

### Manual on win-resolume (post-deploy)

1. Start playback on sp-fast with a song that has lyrics.
2. Verify dashboard ResolumeHealthCard shows green, all four tokens populated.
3. Kill Arena.exe. Wait 30s. Verify ResolumeHealthCard goes red, log shows `composition refresh failed` warn at T+20s, `circuit open` warn at T+30s.
4. Restart Arena. Verify within 15s: ResolumeHealthCard goes green, log shows `RecoveryEvent fired`, `set title text on all clips`, `set subtitle text on all clips`. Verify wall shows title + lyrics again without restarting SongPlayer.

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `RecoveryEvent` fires on transient single-poll glitch | medium | Spurious re-emit (idempotent — visible only as one extra HTTP burst) | Threshold `consecutive_failures >= 1` AT TIME OF SUCCESS — i.e. one fail then immediate success still fires; a single fail with no recovery does not. Acceptable. |
| Per-pipeline re-emit duplicates a normal line tick that fires moments later | low | Operator sees same text twice | Idempotent set_text — same text = no visual change |
| Cache eviction during brief Arena hang (<30s) | low | Disruptive flash of empty card if dashboard polls during the window | 30s threshold tolerates normal Arena hiccups; dashboard updates atomically |
| Dashboard health card becomes a frontend maintenance burden | low | Small UI surface | Read-only card consuming one endpoint; no controls or state |
| Lyrics-state-reload-on-Started change accidentally breaks legitimate state preservation across pipeline events | low | Lyrics flicker mid-song | Audit existing code first; if state preservation is intentional anywhere (e.g., for `Position` events), keep that path. Only force reload on `Started`. |

## Out of scope (recorded for future)

- **T4: CI drill — Arena restart simulation.** Highest assurance, most complex to build (requires CI runner with Arena installable and scriptable). Separate spec when justified.
- **Auto-trigger #sp-subs clips.** SongPlayer doesn't trigger #sp-subs clips today; only writes their text. If no clip is connected on a layer, text stays invisible. Auto-triggering would change the contract with operators (who currently control which clip is live). Decision deferred — current behavior preserved.
- **Presenter / OBS health endpoints.** Same architecture pattern would apply (last_dispatch_ts, recent_failures). Deferred to keep this scope tight.
- **A/V codec hardening (AV1 avoidance at download).** Separate sub-project per the brainstorming decomposition. Will get its own spec.

## Open questions answered during brainstorm

- **Recovery threshold timing**: 30s circuit-breaker, no extra threshold for RecoveryEvent (any successful-after-failed counts). Approved.
- **Idempotent re-emit**: yes, no last-pushed-text dedup needed. Approved.
- **Dashboard card**: read-only card, no force-refresh button. Approved by silence (user said "continue").
