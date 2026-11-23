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

use std::net::{Ipv4Addr, SocketAddrV4};

use dlink::Ports;

/// #342: the slot rule is dlink's own (`dlink::slots`, dstack v0.6.191+):
/// slot `k` = ARC `4440 + k`, CMC `8800 + k`, flows `4455 + k`, process id
/// `k`, the first whose ports bind on the device's address and on the
/// wildcard. SongPlayer keeps only its own chooser below.
pub use dlink::slots::{SLOTS, binds, binds_with, first_free, slot};

/// Where a device's ports came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Set on the device (a test device's own slot); nothing probed.
    Given,
    /// Left by a predecessor of ours whose stack was still stopping.
    Lingering,
    /// The first slot free on the device's address.
    Probed,
    /// No slot free: Dante's defaults, the stack names the port.
    Defaults,
}

impl Source {
    /// Its name in the log.
    pub fn name(self) -> &'static str {
        match self {
            Source::Given => "given",
            Source::Lingering => "lingering",
            Source::Probed => "probed",
            Source::Defaults => "defaults",
        }
    }
}

/// The ports and process id the output's device starts with on `ip`: the
/// ones `given` when set, else the ones a stopping predecessor of ours left
/// (`lingering`), else the first free slot by `try_bind`; `None` keeps
/// Dante's defaults (no slot free). Only the last two cases probe.
pub fn choose(
    ip: Ipv4Addr,
    given: Option<(Ports, u16)>,
    lingering: Option<(Ports, u16)>,
    try_bind: impl Fn(SocketAddrV4) -> bool,
) -> (Option<(Ports, u16)>, Source) {
    match (given, lingering) {
        (Some(p), _) => (Some(p), Source::Given),
        (None, Some(p)) => (Some(p), Source::Lingering),
        (None, None) => match first_free(|p| binds_with(ip, p, &try_bind)) {
            Some(p) => (Some(p), Source::Probed),
            None => (None, Source::Defaults),
        },
    }
}

/// Whether ports left by a stopping predecessor are kept after a start on
/// them was refused with `refusal`: kept while the refused port's holder
/// (`holder_pid`) is this process (`own_pid`: our own stack still
/// stopping) or cannot be told; dropped when another program holds it, so
/// the next open probes a free slot instead of retrying for good.
pub fn keep_lingering(
    refusal: &str,
    holder_pid: impl Fn(u16) -> Option<u32>,
    own_pid: u32,
) -> bool {
    refused_port(refusal)
        .and_then(holder_pid)
        .is_none_or(|pid| pid == own_pid)
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
