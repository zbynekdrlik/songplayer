// SPDX-License-Identifier: MIT OR Apache-2.0
//
// `SLOTS`, `slot`, `first_free`, `binds` and `binds_with` are copied from
// dstack's `crates/dstack-endpoint/src/slots.rs` at tag v0.6.178 (MIT OR
// Apache-2.0, dstack's LICENSE-MIT is in `THIRD-PARTY-NOTICES.txt`).
// `dstack-endpoint` itself pulls cpal (a second audio stack SongPlayer has no
// use for), so the 30 lines live here until dstack moves them into `dlink`;
// then this copy is deleted and the output calls dlink's.

//! #324: the native Dante output's control ports, picked as dstack's own
//! apps pick theirs. Slot `k` is ARC `4440 + k`, CMC `8800 + k`, flows
//! `4455 + k` and process id `k`; the first slot whose three ports are free
//! on the device's address AND on the wildcard address is taken (the
//! wildcard probe keeps Audinate's ConMon, which holds 0.0.0.0:8800 on a
//! PC with Dante Controller or DVS, from being shadowed: dstack #200). The
//! RX / TX flow ports follow the slot by dlink's own rule (29800 + 64k).
//! No slot free = Dante's defaults, and the stack's start names the port.
//!
//! A bind refusal of a chosen port is named with the port's holder where
//! Windows can tell (`holder_text`, the UDP table read through the IP
//! Helper in `dante_slots_win.rs`; no process is spawned).

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use dlink::Ports;

/// How many slots are tried: ARC 4440-4454 never meets flows 4455 up.
pub const SLOTS: u16 = 15;

/// Slot `k`'s ports and process id.
pub fn slot(k: u16) -> (Ports, u16) {
    (
        Ports {
            arc: 4440 + k,
            cmc: 8800 + k,
            flows: 4455 + k,
        },
        k,
    )
}

/// The first of the [`SLOTS`] whose ports `free` says are free.
pub fn first_free(free: impl Fn(&Ports) -> bool) -> Option<(Ports, u16)> {
    (0..SLOTS).map(slot).find(|(p, _)| free(p))
}

/// Whether UDP `ports` are all free for a device on `ip` now
/// ([`binds_with`] on real sockets, which close again).
pub fn binds(ip: Ipv4Addr, ports: &Ports) -> bool {
    binds_with(ip, ports, |at| UdpSocket::bind(at).is_ok())
}

/// Whether `ports` are all free for a device on `ip`, `try_bind` saying
/// whether a UDP socket binds at an address and port now: each port binds
/// on `ip` (no other device holds it there) and on the wildcard address (no
/// other program holds it there, such as Audinate's ConMon).
pub fn binds_with(ip: Ipv4Addr, ports: &Ports, try_bind: impl Fn(SocketAddrV4) -> bool) -> bool {
    [ports.arc, ports.cmc, ports.flows].iter().all(|&port| {
        try_bind(SocketAddrV4::new(ip, port))
            && try_bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
    })
}

/// The ports and process id the output's device starts with on `ip`: the
/// ones `given` (a test device's own slot) when set, else the first free
/// slot by `try_bind`; `None` keeps Dante's defaults (no slot free).
pub fn choose(
    ip: Ipv4Addr,
    given: Option<(Ports, u16)>,
    try_bind: impl Fn(SocketAddrV4) -> bool,
) -> Option<(Ports, u16)> {
    given.or_else(|| first_free(|p| binds_with(ip, p, &try_bind)))
}

/// The ports a device runs on: the chosen ones, else Dante's defaults
/// (slot 0, what dlink binds with none set).
pub fn effective(chosen: Option<(Ports, u16)>) -> (Ports, u16) {
    chosen.unwrap_or_else(|| slot(0))
}

/// The UDP port a stack's bind refusal names: its text is `"<what> on UDP
/// <port>: <error>"` (dstack's `host::bind_sockets`).
pub fn refused_port(text: &str) -> Option<u16> {
    let (_, rest) = text.split_once(" on UDP ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// One row of the PC's UDP table: the local address, port and owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpRow {
    pub addr: Ipv4Addr,
    pub port: u16,
    pub pid: u32,
}

/// The process holding `port` against a device on `ip`: a socket bound on
/// `ip` itself first, else one on the wildcard address.
pub fn holder_of(rows: &[UdpRow], ip: Ipv4Addr, port: u16) -> Option<u32> {
    let on = |addr: Ipv4Addr| {
        rows.iter()
            .find(|r| r.port == port && r.addr == addr)
            .map(|r| r.pid)
    };
    on(ip).or_else(|| on(Ipv4Addr::UNSPECIFIED))
}

/// A stack refusal `text` with the refused port's holder named, `holder`
/// answering a port's holder as `(pid, image name)` where Windows tells;
/// a text that names no port, or a port with no known holder, is kept.
pub fn holder_text(text: &str, holder: impl Fn(u16) -> Option<(u32, String)>) -> String {
    match refused_port(text).and_then(|port| holder(port).map(|h| (port, h))) {
        Some((port, (pid, image))) => format!("{text}; UDP {port} is held by {image} (pid {pid})"),
        None => text.to_string(),
    }
}

/// The file name of an image path (`C:\…\Arena.exe` → `Arena.exe`).
pub fn image_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// A UDP table row's port: the low 16 bits of `dwLocalPort`, in network
/// byte order.
pub fn row_port(dw_local_port: u32) -> u16 {
    u16::from_be((dw_local_port & 0xFFFF) as u16)
}

/// A UDP table row's address: `dwLocalAddr` holds the address's bytes in
/// network order, as they lie in memory.
pub fn row_addr(dw_local_addr: u32) -> Ipv4Addr {
    Ipv4Addr::from(dw_local_addr.to_ne_bytes())
}

/// The holder of UDP `port` against a device on `ip`, where Windows tells
/// (`GetExtendedUdpTable`, then the owner's image name); `None` elsewhere.
#[cfg_attr(test, mutants::skip)] // Windows glue: `None` off Windows by design
pub fn port_holder(ip: Ipv4Addr, port: u16) -> Option<(u32, String)> {
    #[cfg(windows)]
    {
        super::dante_slots_win::port_holder(ip, port)
    }
    #[cfg(not(windows))]
    {
        let _ = (ip, port);
        None
    }
}

#[cfg(test)]
#[path = "dante_slots_tests.rs"]
mod tests;
