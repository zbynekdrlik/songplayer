---
paths:
  - "crates/sp-server/src/mdns.rs"
---

# LAN `sp.local` mDNS advertisement (#51)

sp-server advertises `sp.local` (an mDNS A record → the box's routable LAN
IPv4) so the dashboard stays reachable at `http://sp.local:8920` when the
church LAN loses internet. `crates/sp-server/src/mdns.rs`, wired in
`lib.rs::start()`, surfaced on `/api/v1/status` as `lan_url` / `lan_ip`, shown
by the sp-ui `LanAddress` header component. The internet name
`sp.newlevel.media` is a SEPARATE concern (behind Cloudflare Access, #155).

## Crates + version-matching gotcha

- `mdns-sd` (pure-Rust mDNS-SD responder — no system Avahi/Bonjour) +
  `if-addrs` (interface enumeration). Pin `if-addrs` to the SAME minor
  `mdns-sd` itself requires (0.21.3 → `if-addrs ^0.15`, `socket2 ^0.6`) so
  cargo unifies them into ONE build instead of compiling two copies. Check
  `mdns-sd`'s Cargo.toml before bumping either.
- Deps live directly in `crates/sp-server/Cargo.toml` (single consumer), not
  workspace deps.

## `ServiceInfo::new` is how you publish a plain host A record

`ServiceInfo::new(ty_domain, instance, host_name, ip, port, props)` — register
a `_http._tcp.local.` service whose `host_name` is `"sp.local."` (trailing
dot) and the daemon answers BOTH `_http._tcp` browse queries AND a direct A
query for `sp.local`. Arg types that compile: `ip` = `std::net::IpAddr::V4(..)`
(bare `Ipv4Addr` does NOT impl `AsIpAddrs`); `props` = `&[("path","/")][..]`
(a slice, not `&[..;N]`). `get_fullname()` is `instance + ty_domain`
(IP-independent), so re-registering on an IP change reuses the same fullname —
unregister the old first (same-name conflict is the trap the CLAUDE.md NDI note
warns about).

## Coexistence with the NDI SDK's mDNS socket

The NDI failure (CLAUDE.md "NDI runtime mDNS binding is process-global") was its
announce socket binding a stale APIPA at `NDIlib_initialize()` and never
re-evaluating. `mdns-sd` binds `0.0.0.0:5353` with `SO_REUSEADDR` and
re-announces on IP change, so it does NOT share that failure mode; the two
sockets coexist as standard mDNS multi-responders. This module never touches
the NDI path. Windows `SO_REUSEADDR` co-reception is the one genuinely
uncertain bit — verify live, don't assume.

## IP selection: the default route's address, mDNS pinned to it (#255)

`select_lan_ipv4(addrs, default_route)` filters non-loopback / non-link-local
(169.254/16) / non-unspecified via std `Ipv4Addr` methods (reimplemented, not
reused: `sp_ndi::network_ready::is_real_ipv4` is `pub(crate)` + Windows-only).

- **The default route's address wins** when the host holds it as a real
  address: `default_route_ipv4` = a UDP socket `connect`ed to TEST-NET-2
  (198.51.100.1, no packet sent) and its `local_addr`. On a two-network PC
  (dstack `docs/two-networks.md`: the LAN leg holds the only default route,
  the Dante VLAN leg none) that is the LAN leg. The old rule — the lowest
  RFC1918 address — picked resolume-snv's Dante leg 10.77.7.107 over the LAN
  10.77.9.201 on 10.11.2026, so `sp.local` resolved to the Dante network.
- No default route: the old rule (lowest private, else lowest routable).
- **The daemon is pinned to that interface** before every register
  (`MdnsRegistrar::pin_interface`: `disable_interface(All)` then
  `enable_interface(Addr(ip))`; mdns-sd applies its selections in order, the
  last match wins), so nothing is announced on the Dante leg.
- The standard's next step (dstack, 10.11.2026): a process that is both a
  Dante device and a LAN service binds its LAN services (HTTP 8920, facade
  4456) to the LAN address + 127.0.0.1 only — tracked on #255.
- Live check: `/api/v1/status` `lan_ip` = the LAN address; from dev1
  `avahi-browse -rpt _http._tcp` shows no SongPlayer record on the Dante VLAN
  interface (`enp38s0.70`).

## Testability: the `MdnsRegistrar` seam

The real `ServiceDaemon` opens a socket and can't run in CI, so `reconcile`
(the register / re-register / clear state machine) is generic over a tiny
`MdnsRegistrar` trait (`register_service` / `unregister_service`, named
DIFFERENTLY from ServiceDaemon's inherent `register`/`unregister` to avoid any
resolution collision) and unit-tested with a call-recording fake. Errors are
reduced to `String` in the trait so the fake needn't construct an
`mdns_sd::Error`.

## Operator toggle + live verification

- `lan_mdns_enabled` DB setting (default on) — read ONCE at startup, so a flip
  needs a SongPlayer restart. Toggle via the
  generic `PATCH /api/v1/settings {"lan_mdns_enabled":"false"}`; no dedicated
  UI.
- The feature only ever DEGRADES: any mDNS failure logs a warn and leaves no
  advertisement (server never crashes, NDI untouched).
- Live resolution CANNOT be checked in Linux CI. Post-deploy: `Resolve-DnsName
  sp.local` from the box AND a phone on the LAN with internet dropped, then
  open `http://sp.local:8920`. The routes_tests + e2e only prove the
  wiring/plumbing.
