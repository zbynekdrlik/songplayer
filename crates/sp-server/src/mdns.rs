//! LAN `sp.local` advertisement over mDNS (#51).
//!
//! `sp.newlevel.media` resolves only through the Cloudflare tunnel, so when
//! the church LAN loses upstream internet mid-event the dashboard becomes
//! unreachable even though the phone and the SongPlayer host share the local
//! network — a live single-point-of-failure. This module makes sp-server
//! advertise its own LAN-local hostname `sp.local` (an mDNS A record pointing
//! at the box's routable LAN IPv4), so the dashboard is reachable at
//! `http://sp.local:<port>` with no internet and no router changes (owner
//! ROZHODNUTÉ 2026-09-13). The internet name `sp.newlevel.media` is handled
//! separately behind Cloudflare Access (#155) and is out of scope here.
//!
//! IP selection mirrors the non-APIPA / non-loopback logic in
//! [`sp_ndi::network_ready`] so the record never binds the stale-APIPA
//! failure mode described in `CLAUDE.md` (the 2026-04-27 NDI incident).
//!
//! Coexistence with the NDI SDK's mDNS socket: that failure was the NDI
//! runtime binding its announce socket to a stale APIPA address once at
//! `NDIlib_initialize()` and never re-evaluating. This advertiser uses
//! `mdns-sd`, which binds `0.0.0.0:5353` with `SO_REUSEADDR` and re-announces
//! whenever the LAN IP changes, so it does not share that failure mode; the
//! two sockets coexist as standard mDNS multi-responders. This module never
//! touches the NDI path.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{info, warn};

/// mDNS service type advertised. `_http._tcp` so a browser / OS mDNS resolver
/// answers a direct A query for [`MDNS_HOSTNAME`] below.
const MDNS_SERVICE_TYPE: &str = "_http._tcp.local.";
/// Instance name under the service type.
const MDNS_INSTANCE: &str = "SongPlayer";
/// Fully-qualified `.local.` host the A record is published under. A phone
/// typing `http://sp.local:<port>` resolves THIS name.
const MDNS_HOSTNAME: &str = "sp.local.";
/// Bare host used to build the user-facing LAN URL (no trailing dot).
const LAN_HOSTNAME: &str = "sp.local";
/// Interval between LAN-IP re-checks — catches a DHCP lease change and the
/// boot-before-DHCP case (the same window issue #60 handles for NDI).
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// Settings key: value `"false"` (also `0`/`off`/`no`) disables the whole LAN
/// mDNS advertisement. Absent or anything else = enabled. Read once at
/// startup, so a change needs a restart (same contract as `genlock_pacing`).
pub const SETTING_LAN_MDNS_ENABLED: &str = "lan_mdns_enabled";

/// LAN address surfaced to the dashboard via `/api/v1/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LanStatus {
    /// `http://sp.local:<port>` while the mDNS record is actively advertised;
    /// `None` when disabled, registration failed, or no LAN IP yet.
    pub lan_url: Option<String>,
    /// The box's current routable LAN IPv4 as a raw fallback for display;
    /// `None` when none detected.
    pub lan_ip: Option<String>,
}

/// Shared, cloneable handle to the current LAN status. Written by the mDNS
/// task, read by the `/api/v1/status` handler.
pub type LanStatusHandle = Arc<RwLock<LanStatus>>;

/// Construct a fresh, empty status handle for [`crate::AppState`].
pub fn new_status_handle() -> LanStatusHandle {
    Arc::new(RwLock::new(LanStatus::default()))
}

/// `true` for a regular routable LAN IPv4 — not loopback, not link-local
/// (APIPA `169.254/16`), not unspecified (`0.0.0.0`). Mirrors the non-APIPA
/// predicate `sp_ndi::network_ready::is_real_ipv4` uses.
pub(crate) fn is_real_lan_ipv4(addr: Ipv4Addr) -> bool {
    !addr.is_loopback() && !addr.is_link_local() && !addr.is_unspecified()
}

/// Choose ONE canonical LAN IPv4 to advertise from candidate interface
/// addresses. Prefers an RFC1918 private address (the church-LAN address —
/// the box is `10.77.9.201`), deterministically the lowest so a multi-NIC box
/// is stable across restarts; falls back to the lowest other routable
/// address; `None` when only loopback / APIPA / empty.
pub(crate) fn select_lan_ipv4(addrs: &[Ipv4Addr]) -> Option<Ipv4Addr> {
    let mut reals: Vec<Ipv4Addr> = addrs
        .iter()
        .copied()
        .filter(|a| is_real_lan_ipv4(*a))
        .collect();
    if reals.is_empty() {
        return None;
    }
    reals.sort_unstable();
    // `reals` is sorted, so the first private is the lowest private; if none is
    // private, fall back to the lowest routable address.
    if let Some(private) = reals.iter().copied().find(|a| a.is_private()) {
        return Some(private);
    }
    reals.first().copied()
}

/// Enumerate every IPv4 address on the host's interfaces via `if-addrs` (the
/// same crate `mdns-sd` uses internally). Loopback / APIPA are NOT filtered
/// here — [`select_lan_ipv4`] does that.
// mutants::skip — thin wrapper over the OS interface table; only meaningfully
// exercisable on a real host, and the pure `select_lan_ipv4` it feeds is
// mutation-covered.
#[cfg_attr(test, mutants::skip)]
fn list_ipv4_addresses() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|iface| match iface.addr {
                if_addrs::IfAddr::V4(v4) => Some(v4.ip),
                if_addrs::IfAddr::V6(_) => None,
            })
            .collect(),
        Err(e) => {
            warn!("mdns: failed to enumerate host interfaces: {e}");
            Vec::new()
        }
    }
}

/// Current best LAN IPv4 to advertise, or `None` if the host has no routable
/// IPv4 yet (e.g. booted before DHCP completed — the periodic re-check picks
/// it up once it appears).
// mutants::skip — composes the OS probe with the pure selector; the selector
// is mutation-covered directly.
#[cfg_attr(test, mutants::skip)]
fn current_lan_ipv4() -> Option<Ipv4Addr> {
    select_lan_ipv4(&list_ipv4_addresses())
}

/// Build the `ServiceInfo` for the `sp.local` A record at `ip`. Registering it
/// makes the daemon answer both `_http._tcp.local.` browse queries and direct
/// A queries for `sp.local`.
pub(crate) fn build_service_info(
    ip: Ipv4Addr,
    port: u16,
) -> Result<mdns_sd::ServiceInfo, mdns_sd::Error> {
    mdns_sd::ServiceInfo::new(
        MDNS_SERVICE_TYPE,
        MDNS_INSTANCE,
        MDNS_HOSTNAME,
        std::net::IpAddr::V4(ip),
        port,
        &[("path", "/")][..],
    )
}

/// The user-facing LAN URL for the given port.
fn lan_url(port: u16) -> String {
    format!("http://{LAN_HOSTNAME}:{port}")
}

/// Spawn the LAN `sp.local` mDNS advertiser. Reads [`SETTING_LAN_MDNS_ENABLED`]
/// (default enabled); when disabled, logs and does nothing (the dashboard then
/// shows no LAN address). Otherwise spawns a background task that registers the
/// `sp.local` A record for the current LAN IP, re-registers whenever the IP
/// changes or first appears, and deregisters + shuts the daemon down on the
/// shutdown broadcast. Never panics; any failure degrades to no advertisement.
pub async fn spawn_lan_mdns(
    pool: sqlx::SqlitePool,
    port: u16,
    status: LanStatusHandle,
    shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let enabled = match crate::db::models::get_setting(&pool, SETTING_LAN_MDNS_ENABLED).await {
        Ok(Some(v)) => setting_enabled(&v),
        Ok(None) => true,
        Err(e) => {
            warn!("mdns: could not read {SETTING_LAN_MDNS_ENABLED} ({e}); defaulting to enabled");
            true
        }
    };
    if !enabled {
        info!("mdns: LAN sp.local advertisement disabled by {SETTING_LAN_MDNS_ENABLED}");
        return;
    }
    tokio::spawn(run_lan_mdns(port, status, shutdown));
}

/// Interpret a stored [`SETTING_LAN_MDNS_ENABLED`] value. Off only for an
/// explicit falsey token; everything else (incl. an unexpected value) stays
/// enabled — resilience should fail ON, not silently off.
pub(crate) fn setting_enabled(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "false" | "0" | "off" | "no"
    )
}

/// The advertiser loop. Owns the `ServiceDaemon` for its whole lifetime.
async fn run_lan_mdns(
    port: u16,
    status: LanStatusHandle,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    let daemon = match mdns_sd::ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mdns: could not start mDNS daemon ({e}); sp.local will not be advertised");
            return;
        }
    };

    // The IP currently advertised (None = nothing registered) + the fullname
    // of the registered service, needed to unregister it.
    let mut advertised_ip: Option<Ipv4Addr> = None;
    let mut advertised_fullname: Option<String> = None;

    let mut ticker = tokio::time::interval(REANNOUNCE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let ip = current_lan_ipv4();
        if ip != advertised_ip {
            reconcile(
                &daemon,
                port,
                ip,
                &mut advertised_ip,
                &mut advertised_fullname,
                &status,
            )
            .await;
        }

        tokio::select! {
            _ = shutdown.recv() => {
                info!("mdns: shutdown — deregistering sp.local");
                break;
            }
            _ = ticker.tick() => {}
        }
    }

    // Graceful goodbye: deregister then shut the daemon thread down.
    if let Some(fullname) = advertised_fullname.take() {
        match daemon.unregister(&fullname) {
            Ok(rx) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv_async()).await;
                info!("mdns: deregistered {fullname}");
            }
            Err(e) => warn!("mdns: unregister failed: {e}"),
        }
    }
    if let Err(e) = daemon.shutdown() {
        warn!("mdns: daemon shutdown failed: {e}");
    }
    *status.write().await = LanStatus::default();
}

/// Register (or clear) the `sp.local` record so it matches `ip`, unregistering
/// any stale record first (a same-name re-register would otherwise conflict —
/// the exact trap the `CLAUDE.md` NDI note warns about). Keeps
/// `advertised_ip` / `advertised_fullname` and the shared status in sync.
async fn reconcile(
    daemon: &mdns_sd::ServiceDaemon,
    port: u16,
    ip: Option<Ipv4Addr>,
    advertised_ip: &mut Option<Ipv4Addr>,
    advertised_fullname: &mut Option<String>,
    status: &LanStatusHandle,
) {
    if let Some(old) = advertised_fullname.take() {
        if let Err(e) = daemon.unregister(&old) {
            warn!("mdns: unregister of stale record failed: {e}");
        }
    }

    let Some(new_ip) = ip else {
        warn!("mdns: no routable LAN IPv4 available yet; sp.local not advertised");
        *advertised_ip = None;
        *status.write().await = LanStatus::default();
        return;
    };

    match build_service_info(new_ip, port) {
        Ok(info) => {
            let fullname = info.get_fullname().to_string();
            match daemon.register(info) {
                Ok(()) => {
                    let url = lan_url(port);
                    info!("mdns: advertising {MDNS_HOSTNAME} A={new_ip} -> {url}");
                    *advertised_ip = Some(new_ip);
                    *advertised_fullname = Some(fullname);
                    *status.write().await = LanStatus {
                        lan_url: Some(url),
                        lan_ip: Some(new_ip.to_string()),
                    };
                }
                Err(e) => {
                    warn!("mdns: register of sp.local A={new_ip} failed: {e}");
                    // Retry on the next tick; still surface the raw IP fallback.
                    *advertised_ip = None;
                    *status.write().await = LanStatus {
                        lan_url: None,
                        lan_ip: Some(new_ip.to_string()),
                    };
                }
            }
        }
        Err(e) => {
            warn!("mdns: could not build sp.local service info for {new_ip}: {e}");
            *advertised_ip = None;
            *status.write().await = LanStatus {
                lan_url: None,
                lan_ip: Some(new_ip.to_string()),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_real_rejects_loopback_apipa_unspecified() {
        assert!(!is_real_lan_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        // The exact NDI production APIPA address from the 2026-04-27 incident.
        assert!(!is_real_lan_ipv4(Ipv4Addr::new(169, 254, 144, 214)));
        assert!(!is_real_lan_ipv4(Ipv4Addr::new(169, 254, 0, 1)));
        assert!(!is_real_lan_ipv4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn is_real_accepts_private_and_public() {
        // The actual production LAN address.
        assert!(is_real_lan_ipv4(Ipv4Addr::new(10, 77, 9, 201)));
        assert!(is_real_lan_ipv4(Ipv4Addr::new(192, 168, 1, 50)));
        assert!(is_real_lan_ipv4(Ipv4Addr::new(172, 16, 0, 1)));
        // A public address still counts as routable.
        assert!(is_real_lan_ipv4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn select_returns_none_when_no_real_addr() {
        assert_eq!(select_lan_ipv4(&[]), None);
        assert_eq!(
            select_lan_ipv4(&[Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(169, 254, 1, 2)]),
            None
        );
    }

    #[test]
    fn select_prefers_private_over_public() {
        // Even though 8.8.8.8 sorts lowest, the private LAN address wins.
        let got = select_lan_ipv4(&[Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(10, 77, 9, 201)]);
        assert_eq!(got, Some(Ipv4Addr::new(10, 77, 9, 201)));
    }

    #[test]
    fn select_is_deterministic_lowest_private() {
        // Multi-NIC box: pick the lowest private deterministically, ignore APIPA.
        let got = select_lan_ipv4(&[
            Ipv4Addr::new(192, 168, 1, 50),
            Ipv4Addr::new(10, 77, 9, 201),
            Ipv4Addr::new(169, 254, 1, 2),
        ]);
        assert_eq!(got, Some(Ipv4Addr::new(10, 77, 9, 201)));
    }

    #[test]
    fn select_falls_back_to_lowest_public_when_no_private() {
        let got = select_lan_ipv4(&[Ipv4Addr::new(203, 0, 113, 9), Ipv4Addr::new(8, 8, 8, 8)]);
        assert_eq!(got, Some(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn build_service_info_publishes_sp_local_a_record() {
        let ip = Ipv4Addr::new(10, 77, 9, 201);
        let info = build_service_info(ip, 8920).expect("service info builds");
        assert_eq!(info.get_hostname(), MDNS_HOSTNAME);
        assert_eq!(info.get_port(), 8920);
        assert!(info.get_fullname().contains(MDNS_INSTANCE));
        assert!(info.get_fullname().contains("_http._tcp.local."));
        assert!(
            info.get_addresses()
                .iter()
                .any(|a| *a == std::net::IpAddr::V4(ip)),
            "the constructed A record must carry the selected LAN IP"
        );
    }

    #[test]
    fn lan_url_uses_sp_local_and_port() {
        assert_eq!(lan_url(8920), "http://sp.local:8920");
        assert_eq!(lan_url(1234), "http://sp.local:1234");
    }

    #[test]
    fn setting_enabled_default_and_falsey_tokens() {
        // Enabled for anything that is not an explicit falsey token.
        assert!(setting_enabled("true"));
        assert!(setting_enabled("1"));
        assert!(setting_enabled("on"));
        assert!(setting_enabled("anything"));
        // Disabled only for the explicit falsey tokens (case / whitespace tolerant).
        assert!(!setting_enabled("false"));
        assert!(!setting_enabled("FALSE"));
        assert!(!setting_enabled("  off "));
        assert!(!setting_enabled("0"));
        assert!(!setting_enabled("no"));
    }
}
