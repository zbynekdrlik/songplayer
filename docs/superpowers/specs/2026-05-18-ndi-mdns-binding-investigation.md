# NDI mDNS Stale-Adapter Binding — Investigation + Fix (issue #60)

**Date:** 2026-05-18
**Status:** Implemented — see commits bbb13f2 (RED) + b892e2d (GREEN) on `dev`.

## Problem statement

On 2026-04-27 the production LED-wall went dark for >1 hour during a live
event. Diagnosis on win-resolume showed all 7 SongPlayer NDI senders alive
in-process but invisible to OBS distroAV on the same LAN. Verified via:

- `Get-NetUDPEndpoint -OwningProcess <SongPlayer.pid>` showed
  `169.254.144.214:5353` — an APIPA address that no current adapter held.
- `Get-NetIPAddress -IPAddress 169.254.144.214` returned empty.
- `Get-NetAdapter` showed `Ethernet` UP with `10.77.9.201`.
- Process restart picked up `10.77.9.201`, connections jumped to 2/sender.

OBS / Resolume / NDI Test Patterns running on the same machine were
unaffected.

## Why it happens

The NDI runtime opens its mDNS announce socket exactly once at
`NDIlib_initialize()` and binds it to the OS adapter table's first non-loopback
interface. The binding is never re-evaluated. Two structural facts compose
the bug:

1. **SongPlayer is launched by a Scheduled Task `AtLogon`** on win-resolume.
   This trigger fires before DHCP reliably completes on Windows, so the only
   adapter in `GetAdaptersAddresses` at that instant is the APIPA fallback
   `169.254.x.x` — the real LAN adapter still has no IP.
2. **`PlaybackEngine::new()` calls `NdiLib::load()` immediately** inside
   `sp-server::start()` (`crates/sp-server/src/lib.rs:539` →
   `crates/sp-server/src/playback/mod.rs:200`), which calls
   `NDIlib_initialize()` while only APIPA exists. The mDNS socket binds to
   APIPA. DHCP later assigns `10.77.9.201` but the NDI binding stays on the
   dead APIPA address → wall dark forever.

OBS / Resolume don't reproduce this because they are GUI applications the
user launches manually post-login (post-DHCP).

## What the NDI SDK does NOT expose (researched 2026-05-18)

The user reframed this issue 2026-05-01 per `feedback_no_ndi_bandaids.md`:
"OBS, Resolume, and other NDI applications do not require a process restart
when an NDI connection corrupts. If SongPlayer needs one, SongPlayer has a
bug in how it uses the NDI SDK." Investigation confirmed there is no
per-app NDI-SDK adapter-selection knob:

- `NDIlib_send_create_t` has exactly 4 fields (`p_ndi_name`, `p_groups`,
  `clock_video`, `clock_audio`) — confirmed in `Processing.NDI.Send.h:36-57`
  shipped in `lib/ndi/` of [DistroAV](https://github.com/DistroAV/DistroAV).
  obs-ndi#26 confirms: "The NDI SDK doesn't provide the ability to bind NDI
  to specific network adapters."
- `NDIlib_initialize()` takes no arguments. There is no `_v2` settings
  variant for global init (confirmed in `Processing.NDI.Lib.h:117`).
- DistroAV's `obs_module_load` → `ndiLib->initialize()` call
  (`src/plugin-main.cpp:397`) is identical to SongPlayer's call. Its
  `NDIlib_send_create_t` struct in `src/ndi-output.cpp:224-235` is identical
  to SongPlayer's `send_create_with_clocking`. **The only meaningful
  difference is timing**: DistroAV's initialize runs when OBS starts —
  interactively, after the user is logged in and the desktop is up — not at
  boot.
- The `ndi-config.v1.json` config file at `C:\ProgramData\NDI\` has an
  `ndi.adapters.allowed` array but pinning to a specific IP breaks
  portability (every machine has a different LAN) and reintroduces the
  exact dark-wall failure mode the moment the LAN changes.
- `NDI_DISCOVERY_SERVER` env var bypasses mDNS entirely but requires a
  running Discovery Server on the LAN — not deployed here.

## Fix shipped

**Defer `NDIlib_initialize()` until the OS reports at least one non-APIPA,
non-loopback IPv4 adapter.** Single new module
`crates/sp-ndi/src/network_ready.rs`:

- `wait_for_network_ready_with_probe(probe, max_wait, poll_interval)` —
  pure polling loop with injected probe so Linux CI tests the gate without
  touching Win32. Returns `true` on real-adapter sighting, `false` on
  timeout.
- `list_active_ipv4_addresses()` — `cfg(windows)` calls Win32
  `GetAdaptersAddresses` (AF_INET, `IfOperStatusUp` filter, skip
  anycast/multicast/DNS); `cfg(not(windows))` returns an empty Vec.
- `wait_for_network_ready()` — convenience wrapper used by
  `NdiLib::load()`. Cap 60 s, poll every 2 s. On timeout, proceeds with
  `NDIlib_initialize()` anyway and logs a structured WARN with a pointer
  to this investigation; the failure mode is no worse than today, but is
  now diagnosable from logs alone.

Wiring in `crates/sp-ndi/src/ndi_sdk.rs::NdiLib::load()` happens AFTER
successful library open but BEFORE the `(initialize)()` call, so the
existing graceful-degradation paths for "NDI DLL not installed" are
preserved.

## Test coverage

Eight unit tests in `network_ready.rs::tests`, all pure Rust on Linux CI:

- `is_real_ipv4` pins the two production-failure addresses
  (`169.254.144.214` → rejected, `10.77.9.201` → accepted).
- `wait_for_network_ready_with_probe` covers: immediate find,
  DHCP-completes-mid-wait (>=4 polls), APIPA-only-until-cap (>=2 polls),
  loopback-only timeout, empty-list timeout.

The Win32 `list_active_ipv4_addresses` impl is exercised on win-resolume
by the existing post-deploy E2E suite (the same machine where the bug
manifested).

## Out of scope — explicitly rejected alternatives

- **Per-sender `NDIlib_send_destroy` + `send_create` recreate**: ripped out
  in v0.26.0 (commit 4756669, ref `crates/sp-server/src/playback/ndi_health.rs`).
  Cannot fix runtime-level mDNS binding; same-name conflict makes
  `send_create` return null. Documented in `CLAUDE.md` "Disabled subsystems"
  to prevent reintroduction.
- **Full NDIlib_destroy + initialize re-init recovery**: structurally
  correct but expensive (every sender must be dropped, every receiver
  reconnected) and risky enough to need its own design with real-NDI
  integration tests. Investigation shows the boot-race is the actual root
  cause for the observed failure, so a network-readiness gate at startup
  is sufficient and removes the need for a recovery system.
- **`NDI_CONFIG_DIR` + `adapters.allowed` config file**: pins SongPlayer
  to a specific IP per machine, reintroduces dark-wall on any LAN change.
- **Scheduled-task trigger change to `AtStartup` with delay**: secondary
  belt-and-braces only; doesn't fix machines that boot quickly, doesn't
  fix re-runs after a manual restart on a degraded network.

## Sources

- [DistroAV repo](https://github.com/DistroAV/DistroAV) — `src/plugin-main.cpp`,
  `src/ndi-output.cpp`, `lib/ndi/Processing.NDI.Send.h`
- [obs-ndi#26 — NDI doesn't bind to selected adapter](https://github.com/obs-ndi/obs-ndi/issues/26)
- [NDI docs — Configuration Files](https://docs.ndi.video/all/developing-with-ndi/sdk/configuration-files)
- [NDI docs — NIC Selection](https://docs.ndi.video/all/getting-started/white-paper/nic-selection)
- [NDI docs — mDNS white paper](https://docs.ndi.video/all/getting-started/white-paper/discovery-and-registration/mdns)
