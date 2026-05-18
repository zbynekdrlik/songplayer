//! Network-readiness gate for NDI initialization (issue #60).
//!
//! The NDI runtime binds its mDNS announce socket once at
//! `NDIlib_initialize()` and never re-evaluates. If the process starts before
//! DHCP completes, the only adapter present is APIPA (`169.254.x.x`). NDI
//! binds there and stays dark forever — the LED wall does not recover when
//! DHCP later assigns a real LAN address. Verified in production 2026-04-27
//! via `Get-NetUDPEndpoint` showing `169.254.144.214:5353` for the
//! SongPlayer process while the active LAN adapter was `10.77.9.201`.
//!
//! OBS-NDI / Resolume don't hit this because they are GUI apps the user
//! launches manually post-login (post-DHCP). SongPlayer is started by a
//! Scheduled Task `AtLogon`, which fires before DHCP completes.
//!
//! Fix: poll the OS adapter table and only return once at least one
//! non-link-local, non-loopback IPv4 address is present. Cap at `MAX_WAIT`;
//! if the cap is hit the caller proceeds anyway in degraded mode (matches
//! today's failure shape, but with explicit diagnostic logs).

use std::net::Ipv4Addr;
use std::thread::sleep;
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// Maximum total time spent waiting for a real adapter before giving up.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(60);

/// Time between adapter-table probes while waiting.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Returns `true` if `addr` is a regular LAN address (not link-local /
/// APIPA, not loopback). NDI mDNS uses IPv4, so only v4 is considered.
pub(crate) fn is_real_ipv4(addr: Ipv4Addr) -> bool {
    !addr.is_loopback() && !addr.is_link_local()
}

/// Poll `probe` every `poll_interval` until it returns at least one real
/// IPv4 address, or `max_wait` elapses.
///
/// Returns `true` if a real adapter was found before the cap, `false` on
/// timeout. The caller decides what to do on timeout — current usage is to
/// log a warn and proceed with `NDIlib_initialize` anyway (matches today's
/// failure mode but adds diagnostic visibility).
pub(crate) fn wait_for_network_ready_with_probe<F>(
    mut probe: F,
    max_wait: Duration,
    poll_interval: Duration,
) -> bool
where
    F: FnMut() -> Vec<Ipv4Addr>,
{
    let started = Instant::now();
    loop {
        let addrs = probe();
        if addrs.iter().copied().any(is_real_ipv4) {
            return true;
        }
        if started.elapsed() >= max_wait {
            return false;
        }
        sleep(poll_interval);
    }
}

/// Convenience wrapper: probe Win32 adapter table and wait at most
/// [`MAX_WAIT`] for a non-APIPA, non-loopback IPv4 to appear. Returns
/// `true` if a real adapter was found, `false` on timeout.
///
/// On non-Windows builds this returns `true` immediately — SongPlayer's
/// NDI sender only runs on Windows (see `cfg(windows)` gate at
/// `crates/sp-server/src/playback/mod.rs:196`), so on Linux there is no
/// NDI runtime to gate.
#[cfg(windows)]
pub(crate) fn wait_for_network_ready() -> bool {
    info!(
        "NDI: waiting for non-APIPA IPv4 adapter (cap {:?}, poll {:?}) — see issue #60",
        MAX_WAIT, POLL_INTERVAL
    );
    let result = wait_for_network_ready_with_probe(
        || {
            let addrs = list_active_ipv4_addresses();
            info!(
                "NDI network-readiness probe: {} address(es): {:?}",
                addrs.len(),
                addrs
            );
            addrs
        },
        MAX_WAIT,
        POLL_INTERVAL,
    );
    if result {
        info!("NDI network ready — proceeding with NDIlib_initialize()");
    } else {
        warn!(
            "NDI network NOT ready after {:?} — proceeding with NDIlib_initialize() anyway; \
             mDNS may bind to APIPA, NDI receivers may not see senders until process restart \
             (see issue #60)",
            MAX_WAIT
        );
    }
    result
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub(crate) fn wait_for_network_ready() -> bool {
    true
}

/// Probe the OS adapter table and return all IPv4 addresses currently bound
/// to any operational adapter. Stub for the non-Windows build (NDI sender
/// only runs on Windows in this project anyway — see `cfg(windows)` gate at
/// `crates/sp-server/src/playback/mod.rs:196`).
#[cfg(not(windows))]
pub(crate) fn list_active_ipv4_addresses() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// Probe the Win32 adapter table via `GetAdaptersAddresses` and return every
/// IPv4 address on an operational (`IfOperStatusUp`) adapter.
#[cfg(windows)]
pub(crate) fn list_active_ipv4_addresses() -> Vec<Ipv4Addr> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR, WIN32_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    let flags: GET_ADAPTERS_ADDRESSES_FLAGS =
        GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;

    // Per MSDN guidance, start with 15 KB and double on overflow up to 64 KB.
    let mut size: u32 = 15 * 1024;
    let mut buffer: Vec<u8> = Vec::new();
    let mut result: WIN32_ERROR = ERROR_BUFFER_OVERFLOW;

    for _ in 0..4 {
        buffer.resize(size as usize, 0);
        let addr_ptr = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        // SAFETY: AF_INET.0 fits in u32 (it's 2); buffer is sized to `size`;
        // GetAdaptersAddresses signature documented at MSDN; on overflow it
        // updates `size` to the required value.
        let rc = unsafe {
            GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(addr_ptr), &mut size)
        };
        result = WIN32_ERROR(rc);
        if result == NO_ERROR {
            break;
        }
        if result != ERROR_BUFFER_OVERFLOW {
            warn!("GetAdaptersAddresses failed: rc={}", rc);
            return Vec::new();
        }
        size = size.saturating_mul(2);
    }

    if result != NO_ERROR {
        warn!("GetAdaptersAddresses kept overflowing — bailing");
        return Vec::new();
    }

    let mut out = Vec::new();
    // SAFETY: NO_ERROR means the buffer was populated with a linked list of
    // IP_ADAPTER_ADDRESSES_LH headed by buffer.as_ptr().
    let mut adapter_ptr = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !adapter_ptr.is_null() {
        // SAFETY: adapter_ptr is non-null and points into our buffer; the
        // OS guarantees the linked-list shape.
        let adapter = unsafe { &*adapter_ptr };

        if adapter.OperStatus == IfOperStatusUp {
            let mut unicast_ptr = adapter.FirstUnicastAddress;
            while !unicast_ptr.is_null() {
                // SAFETY: unicast_ptr is a OS-owned linked-list node.
                let unicast = unsafe { &*unicast_ptr };
                let sockaddr = unicast.Address.lpSockaddr;
                if !sockaddr.is_null() {
                    // SAFETY: AF_INET-filtered call means every entry is SOCKADDR_IN.
                    let sa_family = unsafe { (*sockaddr).sa_family };
                    if sa_family == AF_INET {
                        let sin = sockaddr as *const SOCKADDR_IN;
                        // SAFETY: sin points to a SOCKADDR_IN (AF_INET branch).
                        let bytes = unsafe { (*sin).sin_addr.S_un.S_addr };
                        // SOCKADDR_IN stores the address in network byte order.
                        let addr = Ipv4Addr::from(u32::from_be(bytes));
                        out.push(addr);
                    }
                }
                unicast_ptr = unicast.Next;
            }
        }
        adapter_ptr = adapter.Next;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn is_real_rejects_loopback() {
        assert!(!is_real_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_real_ipv4(Ipv4Addr::new(127, 255, 255, 254)));
    }

    #[test]
    fn is_real_rejects_link_local_apipa() {
        // The actual production failure was 169.254.144.214 — pin it.
        assert!(!is_real_ipv4(Ipv4Addr::new(169, 254, 144, 214)));
        assert!(!is_real_ipv4(Ipv4Addr::new(169, 254, 0, 1)));
        assert!(!is_real_ipv4(Ipv4Addr::new(169, 254, 255, 254)));
    }

    #[test]
    fn is_real_accepts_routable_lan_addresses() {
        // The actual production LAN address — pin it.
        assert!(is_real_ipv4(Ipv4Addr::new(10, 77, 9, 201)));
        assert!(is_real_ipv4(Ipv4Addr::new(192, 168, 1, 50)));
        assert!(is_real_ipv4(Ipv4Addr::new(172, 16, 0, 1)));
        // Even public addresses count — NDI doesn't care about RFC1918.
        assert!(is_real_ipv4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn wait_returns_true_when_probe_finds_real_adapter_immediately() {
        let found = wait_for_network_ready_with_probe(
            || vec![Ipv4Addr::new(10, 77, 9, 201)],
            Duration::from_millis(200),
            Duration::from_millis(20),
        );
        assert!(
            found,
            "real adapter present on first probe should return true"
        );
    }

    #[test]
    fn wait_returns_true_when_real_adapter_appears_after_polls() {
        // Simulates DHCP completing partway through the wait: first 3 probes
        // return APIPA only, 4th probe sees the real adapter arrive.
        let counter = AtomicUsize::new(0);
        let found = wait_for_network_ready_with_probe(
            || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 3 {
                    vec![Ipv4Addr::new(169, 254, 1, 2)]
                } else {
                    vec![Ipv4Addr::new(169, 254, 1, 2), Ipv4Addr::new(10, 77, 9, 201)]
                }
            },
            Duration::from_secs(2),
            Duration::from_millis(20),
        );
        assert!(found, "real adapter appearing mid-wait should return true");
        assert!(
            counter.load(Ordering::SeqCst) >= 4,
            "should poll at least until real adapter appears"
        );
    }

    #[test]
    fn wait_returns_false_when_probe_only_yields_apipa_until_cap() {
        let counter = AtomicUsize::new(0);
        let found = wait_for_network_ready_with_probe(
            || {
                counter.fetch_add(1, Ordering::SeqCst);
                vec![Ipv4Addr::new(169, 254, 1, 2)]
            },
            Duration::from_millis(80),
            Duration::from_millis(20),
        );
        assert!(!found, "APIPA-only state should time out and return false");
        assert!(
            counter.load(Ordering::SeqCst) >= 2,
            "should poll at least twice before giving up"
        );
    }

    #[test]
    fn wait_returns_false_when_probe_yields_only_loopback() {
        let found = wait_for_network_ready_with_probe(
            || vec![Ipv4Addr::new(127, 0, 0, 1)],
            Duration::from_millis(60),
            Duration::from_millis(20),
        );
        assert!(!found, "loopback-only state should time out");
    }

    #[test]
    fn wait_returns_false_when_probe_returns_empty() {
        let found = wait_for_network_ready_with_probe(
            || vec![],
            Duration::from_millis(60),
            Duration::from_millis(20),
        );
        assert!(!found, "empty adapter list should time out");
    }
}
