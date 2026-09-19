# Autopilot log

One terse line per issue/round: decisions, key commits, verification.

- **#196 round 1 (restart-safe NDI, server)** — v0.60.0-dev.7. Deterministic
  restart-safe sender identity: port-availability wait + serialized id-order
  `send_create` (`playback/startup_senders.rs`, `runtime_pipeline.rs::ensure_pipeline_inner`);
  advertised-URL getter (`sp_ndi::source_url::parse_source_url`,
  `NDIlib_send_get_source_name` FFI) surfaced as `sender_url` on
  `/api/v1/ndi/health`; no dark-wall ladder for an output with no OBS input
  (`effective_dark_reason` + `PlaybackEngine::output_has_obs_input`). Commits:
  RED c92bb6f → GREEN 80e2467, fixes ec92122 / 50989f2 / eda1a88. CI green incl.
  Windows Build + Deploy + E2E (run 35473914064). Box-verified: no dark wall
  after the deploy restart (on-program SP-slow connections=2, all outputs 2–4).
  FINDING: `NDIlib_send_get_source_name().p_url_address` is empty for a local
  sender → `sender_url` reads null on the box; name→port visibility needs a
  different mechanism (NDIlib_find / own listen ports) — folded into round 2.
  REMAINING: round 2 (item 4 self-check, item 6 E2E one-restart, HealthBar,
  playbook), the sender_url mechanism revision, and the 10-restart box acceptance.
