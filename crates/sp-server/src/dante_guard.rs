//! #255 item C (ROZHODNUTÉ by dstack, the owner of the two-network
//! standard §4): SongPlayer's LAN services — HTTP `:8920` and the remote
//! facade `:4456` — keep listening on `0.0.0.0` (a bind to the LAN address
//! would break when it changes), and refuse at once every accepted
//! connection whose LOCAL address lies on the Dante network. The app must
//! be safe with the firewall off (the owner); the PC's TCP-in block is only
//! the second line.
//!
//! The Dante subnets are the env `DANTE_SUBNET` (one IPv4 CIDR or a comma
//! list, read once at start; a malformed value is one WARN and ignored)
//! plus, always, the subnet of each enabled `type: dante` audio output's
//! `ip`, taken from the host interface that holds that address. With
//! neither, nothing is refused.
//!
//! This file is the pure part (CIDR, containment, the set); the live
//! guard, the accept filter and the refresh task are in
//! `dante_guard_live.rs`.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use sp_core::audio_outputs::{OutputEntry, OutputType};

#[path = "dante_guard_live.rs"]
mod live;
pub use live::{DanteGuard, DanteRefusals, GuardedListener, Which, global, start};

/// The env variable naming the Dante subnet(s) (the same name the
/// Presenter uses).
pub const ENV_DANTE_SUBNET: &str = "DANTE_SUBNET";
/// At most one refusal WARN per listener per this interval.
pub const WARN_EVERY: Duration = Duration::from_secs(60);

/// An IPv4 network: its address with the host bits cleared, and its prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: Ipv4Addr,
    prefix: u8,
}

/// The netmask of a prefix (0 → no bit, 32 → every bit).
fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

impl Cidr {
    /// The network of `ip` under `prefix`; `None` for a prefix over 32.
    pub fn new(ip: Ipv4Addr, prefix: u8) -> Option<Cidr> {
        (prefix <= 32).then(|| Cidr {
            net: Ipv4Addr::from(u32::from(ip) & mask(32)),
            prefix,
        })
    }

    /// `a.b.c.d/n` (spaces around either part allowed).
    pub fn parse(s: &str) -> Option<Cidr> {
        let (ip, prefix) = s.trim().split_once('/')?;
        Cidr::new(ip.trim().parse().ok()?, prefix.trim().parse().ok()?)
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.net)
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.net, self.prefix)
    }
}

/// A comma list of CIDRs; blank items are skipped, any other bad item makes
/// the whole value an error (it is ignored, never half-applied).
pub fn parse_list(raw: &str) -> Result<Vec<Cidr>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| Cidr::parse(item).ok_or_else(|| format!("not an IPv4 CIDR: {item:?}")))
        .collect()
}

/// `DANTE_SUBNET`'s subnets: unset = none.
pub fn env_subnets(raw: Option<&str>) -> Result<Vec<Cidr>, String> {
    raw.map_or(Ok(Vec::new()), parse_list)
}

/// The configured addresses of the enabled Dante outputs.
pub fn dante_ips(entries: &[OutputEntry]) -> Vec<Ipv4Addr> {
    entries
        .iter()
        .filter(|e| e.enabled && e.kind == OutputType::Dante)
        .filter_map(|e| e.dante.as_ref()?.ip.trim().parse().ok())
        .collect()
}

/// The refused set: the env's subnets, then the subnet of every interface
/// address `(ip, prefix)` that is a Dante output's address, each once.
pub fn refused_set(env: &[Cidr], dante: &[Ipv4Addr], ifaces: &[(Ipv4Addr, u8)]) -> Vec<Cidr> {
    let mut set = env.to_vec();
    for ip in dante {
        for &(addr, prefix) in ifaces {
            if addr == *ip
                && let Some(cidr) = Cidr::new(addr, prefix)
                && !set.contains(&cidr)
            {
                set.push(cidr);
            }
        }
    }
    set
}

/// Whether a connection that arrived on `local` is refused (an IPv4-mapped
/// IPv6 address counts as its IPv4 one).
pub fn refuses(set: &[Cidr], local: IpAddr) -> bool {
    let v4 = match local {
        IpAddr::V4(a) => Some(a),
        IpAddr::V6(a) => a.to_ipv4_mapped(),
    };
    v4.is_some_and(|a| set.iter().any(|c| c.contains(a)))
}

/// Whether a refusal at `now` is logged (the first, then one per
/// [`WARN_EVERY`]).
pub fn warn_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|l| now.duration_since(l) >= WARN_EVERY)
}

#[cfg(test)]
#[path = "dante_guard_tests.rs"]
mod tests;
