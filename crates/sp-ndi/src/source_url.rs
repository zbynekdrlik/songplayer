//! Pure parsing of an NDI sender's advertised source URL into a stable
//! `host:port` string (#196).
//!
//! `NDIlib_send_get_source_name` hands back a `NDIlib_source_t` whose
//! `p_url_address` is the address a DistroAV receiver reconnects to
//! (`connect BY-URL '10.77.9.201:5963'`). We surface just the `host:port`
//! on `/api/v1/ndi/health` so a restart's port shuffle is visible. The
//! parsing is pure so it is unit-tested on Linux CI with no NDI runtime.

/// Parse an NDI-advertised source URL into a canonical `host:port` string.
///
/// The SDK normally hands us a bare `host:port` (e.g. `10.77.9.201:5963`);
/// an optional `scheme://` prefix and a trailing `/path` are tolerated
/// defensively. Returns `None` when the input is empty, has no port, has an
/// empty host, or has a non-numeric port.
pub fn parse_source_url(raw: &str) -> Option<String> {
    // RED stub (#196) — real impl lands in the GREEN commit.
    let _ = raw;
    None
}

#[cfg(test)]
mod tests {
    use super::parse_source_url;

    #[test]
    fn bare_host_port_passes_through() {
        assert_eq!(
            parse_source_url("10.77.9.201:5963").as_deref(),
            Some("10.77.9.201:5963")
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            parse_source_url("  10.77.9.201:5961  ").as_deref(),
            Some("10.77.9.201:5961")
        );
    }

    #[test]
    fn scheme_and_path_are_stripped() {
        assert_eq!(
            parse_source_url("ndi://10.77.9.201:5970/stream").as_deref(),
            Some("10.77.9.201:5970")
        );
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(parse_source_url(""), None);
        assert_eq!(parse_source_url("   "), None);
    }

    #[test]
    fn missing_port_is_none() {
        assert_eq!(parse_source_url("10.77.9.201"), None);
    }

    #[test]
    fn empty_port_is_none() {
        assert_eq!(parse_source_url("10.77.9.201:"), None);
    }

    #[test]
    fn empty_host_is_none() {
        assert_eq!(parse_source_url(":5963"), None);
    }

    #[test]
    fn non_numeric_port_is_none() {
        assert_eq!(parse_source_url("10.77.9.201:abc"), None);
    }
}
